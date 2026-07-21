//! Live Paper Trading Binary — Multi-Strategy
//!
//! Connects to Binance's **public** WebSocket (no API key needed) and streams
//! BTC/USDT 1-minute klines.  Each closed candle is fed into **all** active
//! strategies simultaneously, each with its own independent paper portfolio.
//!
//! ## Improvements implemented
//!
//! 1. **Hard stop tightened (1.5% → 1.0%) + trailing stop added** — baseline
//!    slots now have a 1.0% hard stop, trailing stop (arm 0.5%, trail 0.4pp),
//!    and 120-bar time stop.  Trailing stop captures a portion of winning
//!    moves without relying solely on RSI>70.
//!
//! 2. **Bollinger RSI strategy replaces v1+Div** — requires RSI<30 AND price
//!    at/below lower Bollinger Band (SMA20 − 2σ) simultaneously.  Two
//!    orthogonal oversold signals, higher average signal quality.
//!
//! 3. **Regime detection (v3) with early seed** — `RegimeGatedStrategy` blocks
//!    Long entries when EMA(20) < EMA(100) by >0.3%.  The slow EMA now seeds
//!    after 20 bars (was 100), eliminating the 100-min blackout at session start.
//!
//! 4. **Minimum strength filter** — each slot has a configurable minimum entry
//!    strength.  Filters out barely-oversold RSI entries (strength < 0.02–0.05)
//!    which historically had poor R:R.
//!
//! 5. **EMA50 → EMA20 for v2 trend filter** — halves the warmup period and
//!    creates a more responsive trend gate for EMA+EdgeRebound strategy.
//!
//! 6. **Fear & Greed filter** — fetched once at startup from alternative.me.
//!    When the score is ≥70 (Greed/Extreme Greed) all Long entries are blocked
//!    across every slot (exits are always allowed).
//!
//! 7. **LLM pre-trade filter (v4)** — optional fifth slot that calls Claude
//!    Haiku before every Long entry, passing the last 20 candles as context.
//!    Activated only when `ANTHROPIC_API_KEY` is set; disabled otherwise.
//!
//! 8. **Regime logged in trade CSV** — entry regime label written to the
//!    `regime` column so blocked-vs-passed decisions are reviewable.
//!
//! 9. **Liquidity-sweep strategy (v5)** — `LiquiditySweepStrategy` fades
//!    daily-level liquidity sweeps: prior-day extremes raided on a volume
//!    spike then rejected back toward the range mid, gated by the dual-EMA
//!    regime filter.  Uses wide daily-scale risk controls (2.0% hard stop,
//!    1440-bar max hold) matching the `backtest_all` production config.
//!
//! ## Usage
//!
//! ```sh
//! cargo run --release --bin live_paper
//! # with LLM slot:
//! ANTHROPIC_API_KEY=sk-ant-... cargo run --release --bin live_paper
//! ```
//!
//! Press Ctrl-C to stop gracefully.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use barter_data::event::{DataKind, MarketEvent};
use barter_data::subscription::candle::Candle;
use barter_instrument::exchange::ExchangeId;
use barter_instrument::instrument::market_data::kind::MarketDataInstrumentKind;
use barter_instrument::instrument::market_data::MarketDataInstrument;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use tokio_tungstenite::connect_async;

use rust_decimal::Decimal;

use trading_bot_1::backtest::paper_executor::PaperExecutor;
use trading_bot_1::backtest::report::BacktestReport;
use trading_bot_1::risk_manager::RiskControls;
use trading_bot_1::strategy_engine::live::{
    BollingerRsiStrategy, EntryMode, LiquiditySweepStrategy, RegimeDetector, RegimeGatedStrategy,
    RsiStrategy, TrendFilter,
};
use trading_bot_1::strategy_engine::llm_filter::{CandleSnapshot, LlmPreTradeFilter};
use trading_bot_1::strategy_engine::SignalGenerator;
use trading_bot_1::types::Decision;

const WS_URL: &str = "wss://stream.binance.com:9443/ws/btcusdt@kline_1m";
const INITIAL_CAPITAL: f64 = 10_000.0;
const STATUS_INTERVAL: usize = 5;
const AUTOSAVE_INTERVAL: usize = 60;

// ---------------------------------------------------------------------------
// Fear & Greed Index (alternative.me, free, no auth)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct FngResponse {
    data: Vec<FngEntry>,
}

#[derive(serde::Deserialize)]
struct FngEntry {
    value: String,
    value_classification: String,
}

async fn fetch_fear_greed() -> Option<(u8, String)> {
    let resp = reqwest::Client::new()
        .get("https://api.alternative.me/fng/?limit=1")
        .send()
        .await
        .ok()?;
    let fng: FngResponse = resp.json().await.ok()?;
    let entry = fng.data.first()?;
    let score: u8 = entry.value.parse().ok()?;
    Some((score, entry.value_classification.clone()))
}

// ---------------------------------------------------------------------------
// Binance WebSocket JSON structures
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct BinanceWsMsg {
    #[serde(rename = "e")]
    _event_type: String,
    #[serde(rename = "k")]
    kline: BinanceKline,
}

#[derive(Debug, serde::Deserialize)]
struct BinanceKline {
    #[serde(rename = "T")]
    close_time_ms: u64,
    #[serde(rename = "o")]
    open: String,
    #[serde(rename = "h")]
    high: String,
    #[serde(rename = "l")]
    low: String,
    #[serde(rename = "c")]
    close: String,
    #[serde(rename = "v")]
    volume: String,
    #[serde(rename = "x")]
    is_closed: bool,
    #[serde(rename = "n")]
    trade_count: u64,
}

impl BinanceKline {
    fn to_market_event(&self) -> MarketEvent {
        let close_time = DateTime::from_timestamp_millis(self.close_time_ms as i64)
            .unwrap_or_else(Utc::now);
        let candle = Candle {
            close_time,
            open: self.open.parse().unwrap_or(0.0),
            high: self.high.parse().unwrap_or(0.0),
            low: self.low.parse().unwrap_or(0.0),
            close: self.close.parse().unwrap_or(0.0),
            volume: self.volume.parse().unwrap_or(0.0),
            trade_count: self.trade_count,
        };
        let instrument = MarketDataInstrument::from(("btc", "usdt", MarketDataInstrumentKind::Spot));
        MarketEvent {
            time_exchange: close_time,
            time_received: Utc::now(),
            exchange: ExchangeId::BinanceSpot,
            instrument,
            kind: DataKind::Candle(candle),
        }
    }

    fn to_candle_snapshot(&self) -> CandleSnapshot {
        CandleSnapshot {
            open: self.open.parse().unwrap_or(0.0),
            high: self.high.parse().unwrap_or(0.0),
            low: self.low.parse().unwrap_or(0.0),
            close: self.close.parse().unwrap_or(0.0),
            volume: self.volume.parse().unwrap_or(0.0),
        }
    }
}

// ---------------------------------------------------------------------------
// Strategy slot — strategy + executor + optional LLM filter
// ---------------------------------------------------------------------------

struct StrategySlot {
    name: String,
    strategy: Box<dyn SignalGenerator>,
    executor: PaperExecutor,
    /// When `Some`, every Long entry must be approved by the LLM before
    /// the executor processes it.  Exits are always passed through.
    llm_filter: Option<LlmPreTradeFilter>,
    /// Minimum signal strength for Long entries (0.0 = no filter).
    /// Weak RSI dips (strength < threshold) are skipped entirely.
    min_strength: Decimal,
}

impl StrategySlot {
    fn new(name: &str, strategy: Box<dyn SignalGenerator>, capital: f64) -> Self {
        Self {
            name: name.to_string(),
            strategy,
            executor: PaperExecutor::new(capital),
            llm_filter: None,
            min_strength: Decimal::ZERO,
        }
    }

    fn new_with_risk(
        name: &str,
        strategy: Box<dyn SignalGenerator>,
        capital: f64,
        controls: RiskControls,
    ) -> Self {
        Self {
            name: name.to_string(),
            strategy,
            executor: PaperExecutor::with_risk(capital, controls),
            llm_filter: None,
            min_strength: Decimal::ZERO,
        }
    }

    fn new_with_llm(
        name: &str,
        strategy: Box<dyn SignalGenerator>,
        capital: f64,
        controls: RiskControls,
        api_key: String,
    ) -> Self {
        Self {
            name: name.to_string(),
            strategy,
            executor: PaperExecutor::with_risk(capital, controls),
            llm_filter: Some(LlmPreTradeFilter::new(api_key)),
            min_strength: Decimal::ZERO,
        }
    }

    /// Set minimum entry strength; Long signals below this threshold are dropped.
    fn with_min_strength(mut self, val: f64) -> Self {
        self.min_strength = Decimal::try_from(val).unwrap_or(Decimal::ZERO);
        self
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║         🚀  Live Paper Trader — Multi-Strategy              ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║  Exchange  : Binance (public WebSocket, no API key)         ║");
    println!("║  Pair      : BTCUSDT                                        ║");
    println!("║  Timeframe : 1-minute klines                                ║");
    println!("║  Capital   : $10,000 per strategy (paper)                   ║");
    println!("║                                                             ║");
    println!("║  Strategies:                                                ║");
    println!("║    1. v1 RSI+stop        RSI<30 | stop 1.0% | trail 0.5%  ║");
    println!("║    2. v1 BB+RSI+stop     RSI<30 AND price≤lower-BB(20,2σ) ║");
    println!("║    3. v2 EMA20+rebound   EdgeRebound + EMA20 trend filter  ║");
    println!("║    4. v3 Regime+RSI      dual-EMA(20/100) regime gate      ║");
    println!("║    5. v4 LLM+Regime+RSI  Claude Haiku pre-trade filter     ║");
    println!("║       (slot 5 requires ANTHROPIC_API_KEY env var)          ║");
    println!("║                                                             ║");
    println!("║  Global gates (all slots):                                  ║");
    println!("║    • Min strength filter per slot (0.02–0.05)              ║");
    println!("║    • Fear & Greed index — blocks longs when score ≥ 70    ║");
    println!("║                                                             ║");
    println!("║  Press Ctrl-C to stop and generate final reports.           ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    // --- Graceful shutdown ---
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        println!("\n⏹  Ctrl-C received — shutting down gracefully...");
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    // --- Fear & Greed fetch (improvement 3) ---
    println!("📊 Fetching Fear & Greed Index...");
    let (fg_score, fg_label, fg_blocks_longs) = match fetch_fear_greed().await {
        Some((score, label)) => {
            let blocks = score >= 70;
            let icon = match score {
                0..=24 => "😱",
                25..=49 => "😨",
                50..=74 => "😐",
                _ => "🤑",
            };
            println!(
                "   {icon} Score: {score}/100 — {label} | Blocks Long entries: {}",
                if blocks { "YES ⛔" } else { "no ✓" }
            );
            (score, label, blocks)
        }
        None => {
            println!("   ⚠️  F&G API unavailable — gate disabled");
            (50u8, "Unknown".to_string(), false)
        }
    };

    // --- LLM API key (improvement 4) ---
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
    if anthropic_key.is_some() {
        println!("🤖 ANTHROPIC_API_KEY found — v4 LLM slot enabled (Claude Haiku)");
    } else {
        println!("ℹ️  ANTHROPIC_API_KEY not set — v4 LLM slot disabled");
    }
    println!();

    // --- Risk control presets ---

    // Improvement 1: tightened hard stop (1.5% → 1.0%) + trailing stop added.
    // The -1.52% loss in Jun 9-10 hit the old 1.5% stop exactly — tightening
    // to 1.0% caps that.  Trailing stop (arm 0.5%, trail 0.4pp) locks in a
    // portion of winning moves without waiting for RSI>70.
    let baseline_risk = RiskControls {
        hard_stop_pct: Some(1.0),
        trailing_stop_pct: Some(0.4),
        trailing_arm_pct: Some(0.5),
        max_hold_bars: Some(120),
    };

    // v2 EMA+rebound: same trailing logic, slightly wider arm (0.6%) to give
    // EdgeRebound entries a bit more room before the trail kicks in.
    let v2_controls = RiskControls {
        hard_stop_pct: Some(1.0),
        trailing_stop_pct: Some(0.5),
        trailing_arm_pct: Some(0.6),
        max_hold_bars: Some(90),
    };

    // v3/v4 Regime strategies: regime gate already filters bad entries so we
    // can afford a slightly looser time cap (90 bars).
    let regime_risk = RiskControls {
        hard_stop_pct: Some(1.0),
        trailing_stop_pct: Some(0.4),
        trailing_arm_pct: Some(0.5),
        max_hold_bars: Some(90),
    };

    // v5 Sweep: daily-level sweep reversions play out over hours, so this uses
    // the wide daily-scale controls from backtest_all — a 2.0% hard stop behind
    // the strategy's own invalidation exit, trailing armed only after a real
    // 2.0% move, and a day-scale (1440-bar) max hold.
    let sweep_risk = RiskControls {
        hard_stop_pct: Some(2.0),
        trailing_stop_pct: Some(1.5),
        trailing_arm_pct: Some(2.0),
        max_hold_bars: Some(1440),
    };

    // --- Build strategy slots ---
    let mut slots: Vec<StrategySlot> = vec![
        // 1. v1 RSI: tighter stop (1.0%), trailing stop arm 0.5%/trail 0.4pp,
        //    minimum strength filter (0.05) drops barely-oversold entries.
        StrategySlot::new_with_risk(
            "v1 RSI+stop",
            Box::new(RsiStrategy::new()),
            INITIAL_CAPITAL,
            baseline_risk.clone(),
        ).with_min_strength(0.05),

        // 2. v1 BB+RSI: replaces v1+Div — requires RSI<30 AND price ≤ lower
        //    Bollinger Band (SMA20 − 2σ).  Two orthogonal signals; fires less
        //    often but at more genuine extremes.
        StrategySlot::new_with_risk(
            "v1 BB+RSI+stop",
            Box::new(BollingerRsiStrategy::new()),
            INITIAL_CAPITAL,
            baseline_risk.clone(),
        ).with_min_strength(0.03),

        // 3. v2 EMA20+rebound: EdgeRebound entry (RSI crosses back through 30)
        //    + EMA20 trend filter (was EMA50 — halves warmup to 20 bars).
        StrategySlot::new_with_risk(
            "v2 EMA20+rebound+risk",
            Box::new(
                RsiStrategy::with_params(14, 30.0, 70.0)
                    .with_entry_mode(EntryMode::EdgeRebound)
                    .with_trend_filter(TrendFilter::new(20)),
            ),
            INITIAL_CAPITAL,
            v2_controls,
        ).with_min_strength(0.02),

        // 4. v3 Regime+RSI: dual-EMA(20/100) classifier blocks longs during
        //    downtrends.  Slow EMA seeded after 20 bars (was 100) so the
        //    regime gate is active from session start, not after 1.7h.
        StrategySlot::new_with_risk(
            "v3 Regime+RSI+risk",
            Box::new(RegimeGatedStrategy::new(
                Box::new(RsiStrategy::new()),
                RegimeDetector::new(20, 100, 0.3).with_slow_early_seed(20),
            )),
            INITIAL_CAPITAL,
            regime_risk.clone(),
        ).with_min_strength(0.05),

        // 5. v5 Sweep(1d,fade,regime): fades daily-level liquidity sweeps —
        //    prior-day extremes raided on a volume spike then rejected, faded
        //    back toward the range mid.  Dual-EMA regime gate blocks
        //    counter-trend fades.  Matches the backtest_all production config.
        StrategySlot::new_with_risk(
            "v5 Sweep(1d,fade,regime)",
            Box::new(LiquiditySweepStrategy::new()),
            INITIAL_CAPITAL,
            sweep_risk,
        ),
    ];

    // 5. v4 LLM+Regime+RSI — only when API key present; uses same early seed.
    if let Some(key) = anthropic_key {
        slots.push(
            StrategySlot::new_with_llm(
                "v4 LLM+Regime+RSI",
                Box::new(RegimeGatedStrategy::new(
                    Box::new(RsiStrategy::new()),
                    RegimeDetector::new(20, 100, 0.3).with_slow_early_seed(20),
                )),
                INITIAL_CAPITAL,
                regime_risk,
                key,
            ).with_min_strength(0.05),
        );
    }

    let mut bar_count: usize = 0;
    let mut last_close_price: Option<f64> = None;
    let start_time = Utc::now();

    // --- Connect to Binance WebSocket ---
    println!("📡 Connecting to Binance WebSocket...");
    let (ws_stream, _response) = connect_async(WS_URL)
        .await
        .expect("Failed to connect to Binance WebSocket");
    println!("✅ Connected! Waiting for kline data...");
    println!("   (RSI needs ~15 candles to warm up, Regime needs ~100)\n");

    let (_write, mut read) = ws_stream.split();

    // --- Main event loop ---
    while running.load(Ordering::SeqCst) {
        let msg = tokio::select! {
            msg = read.next() => msg,
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                if !running.load(Ordering::SeqCst) { break; }
                continue;
            }
        };

        let msg = match msg {
            Some(Ok(msg)) => msg,
            Some(Err(e)) => { eprintln!("⚠️  WebSocket error: {e}"); break; }
            None => { println!("⚠️  WebSocket stream ended"); break; }
        };

        let text = match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            tokio_tungstenite::tungstenite::Message::Ping(_) => continue,
            tokio_tungstenite::tungstenite::Message::Pong(_) => continue,
            tokio_tungstenite::tungstenite::Message::Close(_) => {
                println!("⚠️  WebSocket closed by server");
                break;
            }
            _ => continue,
        };

        let ws_msg: BinanceWsMsg = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => { eprintln!("⚠️  JSON parse error: {e}"); continue; }
        };

        // Live ticks — show price but don't feed strategies
        if !ws_msg.kline.is_closed {
            let price: f64 = ws_msg.kline.close.parse().unwrap_or(0.0);
            print!("\r🔄 Live tick | BTC: ${:.2} | Waiting for candle close...   ", price);
            std::io::stdout().flush().ok();
            continue;
        }

        // --- Closed candle ---
        bar_count += 1;
        let event = ws_msg.kline.to_market_event();
        let candle_snap = ws_msg.kline.to_candle_snapshot();
        let price: f64 = ws_msg.kline.close.parse().unwrap_or(0.0);
        last_close_price = Some(price);
        let candle_time = DateTime::from_timestamp_millis(ws_msg.kline.close_time_ms as i64)
            .unwrap_or_else(Utc::now);

        print!("\r{}\r", " ".repeat(70));
        println!(
            "🕯  bar {:>5} | ${:.2} | O:{} H:{} L:{} C:{} | {}",
            bar_count, price,
            ws_msg.kline.open, ws_msg.kline.high, ws_msg.kline.low, ws_msg.kline.close,
            candle_time.format("%Y-%m-%d %H:%M UTC"),
        );

        for slot in slots.iter_mut() {
            // --- Push candle to LLM history (before generating signal) ---
            if let Some(ref mut llm) = slot.llm_filter {
                llm.push_candle(candle_snap.clone());
            }

            // --- Strategy signal ---
            if let Some(signal) = slot.strategy.generate_signal(&event) {
                let decision_str = match &signal.decision {
                    Decision::Long => "🟢 LONG ",
                    Decision::Short => "🟠 SHORT",
                    Decision::ClosePosition => "🔴 CLOSE",
                };

                // --- Fear & Greed gate (Long entries only) ---
                if matches!(signal.decision, Decision::Long) && fg_blocks_longs {
                    println!(
                        "🚫 {} | F&G BLOCKED | score={}/100 ({}) | bar {}",
                        slot.name, fg_score, fg_label, bar_count
                    );

                // --- Minimum strength filter (Long entries only) ---
                } else if matches!(signal.decision, Decision::Long)
                    && signal.strength < slot.min_strength
                {
                    println!(
                        "⬜ {} | STR-SKIP | str:{} < {:.2} | bar {}",
                        slot.name, signal.strength, slot.min_strength, bar_count
                    );

                } else {
                    // --- LLM gate (Long entries on v4 slot only) ---
                    let llm_approved = if matches!(signal.decision, Decision::Long) {
                        if let Some(ref filter) = slot.llm_filter {
                            println!(
                                "📊 {} | {} | ${:.2} | str: {} | 🤖 querying LLM...",
                                slot.name, decision_str, price, signal.strength
                            );
                            filter.approve_entry(price).await
                        } else {
                            true
                        }
                    } else {
                        true // exits always pass
                    };

                    if llm_approved {
                        // Tag regime for CSV logging before opening the position.
                        if matches!(signal.decision, Decision::Long) {
                            if let Some(regime) = slot.strategy.status_line() {
                                slot.executor.tag_next_entry(regime);
                            }
                        }
                        let extra = slot.strategy.status_line()
                            .map(|s| format!(" | {}", s))
                            .unwrap_or_default();
                        println!(
                            "📊 {} | {} | ${:.2} | str: {} | bar {}{} | {}",
                            slot.name, decision_str, price, signal.strength,
                            bar_count, extra, candle_time.format("%H:%M:%S UTC"),
                        );
                        slot.executor.process_signal(bar_count, price, &signal);
                    }
                }
            }

            // --- Risk exits (always run, even if signal was gated) ---
            if let Some(exit_pnl) = slot.executor.check_risk_exit(bar_count, price, candle_time) {
                println!(
                    "🛑 {} | RISK-EXIT @ ${:.2} | {:+.2}% | bar {}",
                    slot.name, price, exit_pnl, bar_count
                );
            }

            slot.executor.sample_equity(price);
        }

        // --- Periodic status dashboard ---
        if bar_count % STATUS_INTERVAL == 0 {
            let runtime = Utc::now().signed_duration_since(start_time);
            let hours = runtime.num_hours();
            let mins = runtime.num_minutes() % 60;
            println!(
                "─── Bar {:>5} | ${:.2} | {}h{}m | F&G: {}/100 ({}) ───",
                bar_count, price, hours, mins, fg_score, fg_label
            );
            for slot in slots.iter() {
                let pos = if slot.executor.is_in_position() { "📌" } else { "  " };
                let regime_tag = slot.strategy.status_line()
                    .map(|s| format!(" [{}]", s))
                    .unwrap_or_default();
                println!(
                    "  {} {:>28}{:<12} | Eq: ${:>10.2} | P&L: {:+6.2}% | Trades: {:>3}",
                    pos,
                    slot.name,
                    regime_tag,
                    slot.executor.equity,
                    slot.executor.total_pnl_pct(),
                    slot.executor.total_trades(),
                );
            }
        }

        // --- Auto-save ---
        if bar_count % AUTOSAVE_INTERVAL == 0 {
            for slot in slots.iter() {
                save_report(&slot.executor, &slot.name, bar_count, "autosave");
            }
            println!("💾 Auto-saved reports for all strategies");
        }
    }

    // --- Final reports ---
    println!("\n");
    println!("═══════════════════════════════════════════════════════════════");
    println!("                   📋  FINAL REPORTS");
    println!("═══════════════════════════════════════════════════════════════");

    for slot in slots.iter_mut() {
        if slot.executor.is_in_position() {
            match last_close_price {
                Some(p) => {
                    slot.executor.force_close(bar_count, p, Utc::now());
                    println!("⚠️  {} — force-closed open position @ ${:.2}", slot.name, p);
                }
                None => {
                    slot.executor.discard_open_position();
                    println!("⚠️  {} — discarded open position (no market data)", slot.name);
                }
            }
        }

        let report = BacktestReport::from_executor(
            &slot.executor,
            &format!("{} — Live Paper", slot.name),
            "Binance BTCUSDT 1m WebSocket",
            bar_count,
        );
        report.print_summary();

        let filename = save_report(&slot.executor, &slot.name, bar_count, "final");
        println!("💾 {} → {}", slot.name, filename);
    }

    // --- Side-by-side comparison ---
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                  📊  Strategy Comparison                    ║");
    println!("╠══════════════════════════════╦══════════════╦═══════════════╣");
    println!("║ Strategy                     ║  P&L         ║  Trades       ║");
    println!("╠══════════════════════════════╬══════════════╬═══════════════╣");
    for slot in slots.iter() {
        println!(
            "║ {:>28} ║ {:>+10.2}%  ║ {:>5} ({:.0}%)  ║",
            slot.name,
            slot.executor.total_pnl_pct(),
            slot.executor.total_trades(),
            slot.executor.win_rate(),
        );
    }
    println!("╚══════════════════════════════╩══════════════╩═══════════════╝");
    println!();
    println!(
        "  Fear & Greed at session start: {}/100 — {}",
        fg_score, fg_label
    );
    println!();
}

fn save_report(executor: &PaperExecutor, strategy_name: &str, bar_count: usize, label: &str) -> String {
    let safe_name: String = strategy_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
        .collect();

    let ts = Utc::now().format("%Y%m%d_%H%M%S");
    let json_path = format!("data/reports/live_{}_{}_{}.json", safe_name, label, ts);
    let csv_path  = format!("data/reports/live_{}_{}_{}_trades.csv", safe_name, label, ts);

    let report = BacktestReport::from_executor(
        executor,
        &format!("{} — Live Paper", strategy_name),
        "Binance BTCUSDT 1m WebSocket",
        bar_count,
    );

    if let Err(e) = report.write_json(&json_path) {
        eprintln!("⚠️  Failed to write JSON report: {e}");
    }
    if let Err(e) = report.write_trades_csv(&csv_path) {
        eprintln!("⚠️  Failed to write trades CSV: {e}");
    }

    json_path
}
