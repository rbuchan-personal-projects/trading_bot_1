//! Strategy Engine Module (The Brain)
//!
//! Takes market data and decides whether to Buy, Sell, or Hold.
//! Generic over its dependencies to support dependency injection.
//!
//! ## Key Types
//! - [`Signal`] — a rich trading signal carrying metadata + confidence.
//! - [`SignalGenerator`] — trait that converts a barter-data
//!   [`MarketEvent`] into an optional [`Signal`].
//! - [`Strategy`] — simpler trait operating on our own [`MarketTick`].
//!
//! ## Key Libraries
//! - `barter-data` — normalized market events
//! - `rust_ti` — technical indicators (SMA, EMA, RSI, MACD, …)

pub mod live;
pub mod llm_filter;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use barter_data::event::MarketEvent;
use barter_instrument::{
    exchange::ExchangeId,
    instrument::market_data::MarketDataInstrument,
};

use crate::types::{Decision, MarketTick, TradingSignal};
use crate::market_data::MarketProvider;
use crate::execution::OrderExecutor;
use crate::risk_manager::RiskEvaluator;

// ---------------------------------------------------------------------------
// Signal
// ---------------------------------------------------------------------------

/// A rich trading signal produced by a [`SignalGenerator`] in response to a
/// barter-data [`MarketEvent`].
///
/// Carries enough context for downstream risk checks and order execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    /// The exchange this signal originated from.
    pub exchange: ExchangeId,
    /// The instrument the signal applies to.
    pub instrument: MarketDataInstrument,
    /// Timestamp when the signal was generated.
    pub time: DateTime<Utc>,
    /// The directional decision.
    pub decision: Decision,
    /// Confidence / strength of the signal (0.0 – 1.0).
    ///
    /// Strategies can use this to scale position size or filter weak signals.
    pub strength: Decimal,
}

// ---------------------------------------------------------------------------
// SignalGenerator trait
// ---------------------------------------------------------------------------

/// Trait for generating [`Signal`]s from barter-data [`MarketEvent`]s.
///
/// Implementations will typically maintain internal state (e.g. indicator
/// buffers fed by `rust_ti`) and return `Some(Signal)` only when the
/// strategy's entry/exit criteria are met.
///
/// Returns `None` when there is no actionable signal (insufficient data,
/// neutral conditions, etc.).
///
/// ### Example (pseudocode)
/// ```ignore
/// impl SignalGenerator for MyStrategy {
///     fn generate_signal(&mut self, event: &MarketEvent) -> Option<Signal> {
///         // 1. Extract price from event.kind (Trade / Candle / …)
///         // 2. Push price into rust_ti indicator (e.g. RSI, EMA)
///         // 3. Evaluate rules → return Some(Signal) or None
///     }
/// }
/// ```
pub trait SignalGenerator: Send + Sync {
    /// Analyse a normalised market event and optionally emit a trading signal.
    fn generate_signal(&mut self, event: &MarketEvent) -> Option<Signal>;

    /// Optional one-line status string for dashboard display (e.g. regime label).
    /// Strategies that track extra state can override this; default is `None`.
    fn status_line(&self) -> Option<String> { None }
}

// ---------------------------------------------------------------------------
// Legacy Strategy trait (operates on MarketTick)
// ---------------------------------------------------------------------------

/// Simpler trait for strategies that operate on our own [`MarketTick`] type
/// rather than barter-data's [`MarketEvent`].
///
/// Implement this to define different trading algorithms
/// (e.g., moving average crossover, mean reversion, momentum).
#[async_trait]
pub trait Strategy: Send + Sync {
    /// Analyze a market tick and produce a trading signal.
    fn analyze(&self, tick: &MarketTick) -> TradingSignal;
}

/// The core trading engine, generic over all its dependencies.
///
/// This design allows swapping in mock implementations for
/// backtesting, paper trading, or unit testing.
///
/// # Type Parameters
/// - `M`: Market data provider
/// - `E`: Order executor
/// - `R`: Risk evaluator
pub struct StrategyEngine<M, E, R>
where
    M: MarketProvider,
    E: OrderExecutor,
    R: RiskEvaluator,
{
    pub market: M,
    pub executor: E,
    pub risk: R,
    pub strategy: Box<dyn Strategy>,
}

impl<M, E, R> StrategyEngine<M, E, R>
where
    M: MarketProvider,
    E: OrderExecutor,
    R: RiskEvaluator,
{
    /// Create a new strategy engine with injected dependencies.
    pub fn new(market: M, executor: E, risk: R, strategy: Box<dyn Strategy>) -> Self {
        StrategyEngine {
            market,
            executor,
            risk,
            strategy,
        }
    }

    /// Run one tick of the trading loop:
    /// 1. Fetch market data
    /// 2. Generate signal via strategy
    /// 3. Validate with risk manager
    /// 4. Execute order if approved
    pub async fn tick(&self, symbol: &str) -> Result<TradingSignal, Box<dyn std::error::Error>> {
        // 1. Fetch latest market data
        let market_tick = self.market.get_tick(symbol).await?;

        // 2. Run strategy to get a signal
        let signal = self.strategy.analyze(&market_tick);

        // TODO: Steps 3 & 4 — risk check then execute
        // match &signal {
        //     TradingSignal::Buy | TradingSignal::Sell => {
        //         self.risk.validate_trade(symbol, quantity, price).await?;
        //         self.executor.execute(order).await?;
        //     }
        //     TradingSignal::Hold => {}
        // }

        Ok(signal)
    }
}

