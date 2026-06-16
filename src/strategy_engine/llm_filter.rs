//! LLM-based pre-trade entry filter using the Claude Haiku API.
//!
//! [`LlmPreTradeFilter`] maintains a rolling window of recent candles and,
//! before each Long entry, asks Claude Haiku whether the price action context
//! supports the trade.  On API failure it defaults to allowing the trade so
//! that connectivity issues never silently disable the strategy.
//!
//! The filter is deliberately stateless with respect to the strategy — it only
//! looks at raw OHLCV data and the current price, not at RSI internals.  This
//! gives the LLM an independent view of price action rather than just
//! rubber-stamping the RSI signal.

use std::collections::VecDeque;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Candle snapshot (OHLCV without Binance-specific fields)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CandleSnapshot {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

// ---------------------------------------------------------------------------
// Anthropic Messages API shapes (minimal — only what we need)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<Message<'a>>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    text: String,
}

// ---------------------------------------------------------------------------
// LlmPreTradeFilter
// ---------------------------------------------------------------------------

pub struct LlmPreTradeFilter {
    client: reqwest::Client,
    api_key: String,
    candle_history: VecDeque<CandleSnapshot>,
    history_size: usize,
}

impl LlmPreTradeFilter {
    /// Creates a new filter.  `api_key` is your Anthropic API key.
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            candle_history: VecDeque::with_capacity(21),
            history_size: 20,
        }
    }

    /// Push a closed candle into the history window.  Call this on every
    /// closed bar so the context stays current regardless of whether the
    /// strategy signals.
    pub fn push_candle(&mut self, candle: CandleSnapshot) {
        self.candle_history.push_back(candle);
        if self.candle_history.len() > self.history_size {
            self.candle_history.pop_front();
        }
    }

    /// Returns true once we have a full history window and can make a
    /// meaningful assessment.  Returns true (allow) before that so the
    /// strategy isn't silently inactive during warm-up.
    pub fn is_ready(&self) -> bool {
        self.candle_history.len() >= self.history_size
    }

    /// Ask Claude Haiku whether the current price action supports a Long entry.
    ///
    /// Returns `true` (proceed) or `false` (skip).  Defaults to `true` on
    /// any API or network error so connectivity issues don't disable trading.
    pub async fn approve_entry(&self, current_price: f64) -> bool {
        if !self.is_ready() {
            return true;
        }

        let candle_lines: Vec<String> = self.candle_history
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let body = c.close - c.open;
                let dir = if body >= 0.0 { "▲" } else { "▼" };
                format!(
                    "{:>2}. {} O:{:.0} H:{:.0} L:{:.0} C:{:.0} V:{:.0}",
                    i + 1, dir, c.open, c.high, c.low, c.close, c.volume
                )
            })
            .collect();

        let prompt = format!(
            "You are a quantitative trading analyst. \
Evaluate a potential LONG entry on BTC/USDT 1-minute chart.

Last 20 closed candles (oldest → newest):
{}

Current price: ${:.2}
Signal reason: RSI(14) crossed back up through oversold (30).

Assess the price action only. Reply on one line in exactly this format:
VERDICT|SCORE
Where VERDICT is GO or NOGO and SCORE is an integer 1–10.
Example: GO|7

Do not add any other text.",
            candle_lines.join("\n"),
            current_price
        );

        let request = AnthropicRequest {
            model: "claude-haiku-4-5-20251001",
            max_tokens: 15,
            messages: vec![Message { role: "user", content: prompt }],
        };

        match self.client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request)
            .send()
            .await
        {
            Ok(resp) => match resp.json::<AnthropicResponse>().await {
                Ok(data) => {
                    let text = data.content.first().map(|c| c.text.trim()).unwrap_or("NOGO|0");
                    let approved = text.starts_with("GO");
                    let icon = if approved { "✅" } else { "❌" };
                    println!("   🤖 LLM {icon} {text}");
                    approved
                }
                Err(e) => {
                    eprintln!("   ⚠️  LLM response parse error: {e} — allowing trade");
                    true
                }
            },
            Err(e) => {
                eprintln!("   ⚠️  LLM API error: {e} — allowing trade");
                true
            }
        }
    }
}
