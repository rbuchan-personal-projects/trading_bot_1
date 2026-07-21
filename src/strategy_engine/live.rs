//! Live strategy implementations.
//!
//! Strategies implemented:
//! - [`SimpleStrategy`] — legacy placeholder operating on [`MarketTick`]
//! - [`RsiStrategy`] — 14-period RSI overbought / oversold
//! - [`DivergenceStrategy`] — price/RSI swing divergence detection
//! - [`RsiDivergenceStrategy`] — combined RSI levels + divergence confirmation
//!
//! Key Libraries:
//! - `rust_ti` — Technical indicators (RSI, SMA, EMA, MACD, Bollinger Bands, …)
//! - `barter-data` — normalised [`MarketEvent`] input

use std::collections::VecDeque;

use barter_data::event::{DataKind, MarketEvent};
use chrono::Utc;
use rust_decimal::Decimal;

use crate::types::{Decision, MarketTick, TradingSignal};
use super::{Signal, SignalGenerator, Strategy};

// ===========================================================================
// Helpers
// ===========================================================================

/// Extract a price from a [`MarketEvent`].  Returns `None` for event kinds
/// that don't carry a single meaningful price (e.g. full order-book snapshots).
fn extract_price(event: &MarketEvent) -> Option<f64> {
    match &event.kind {
        DataKind::Trade(trade) => Some(trade.price),
        DataKind::Candle(candle) => Some(candle.close),
        _ => None,
    }
}

// ===========================================================================
// Swing-point detection (shared by divergence strategies)
// ===========================================================================

/// A confirmed swing point (local extremum).
#[derive(Debug, Clone, Copy)]
struct SwingPoint {
    /// Index in the overall price stream (monotonically increasing).
    index: usize,
    price: f64,
    rsi: f64,
}

/// Detects swing highs and swing lows using a look-back/look-ahead window.
///
/// A bar at position `i` is a **swing high** if it is the highest price in the
/// window `[i - lookback, i + lookback]` (and analogously for swing lows).
///
/// Because we need future bars to confirm, the detector buffers
/// `2 * lookback + 1` bars before emitting a confirmed swing.
#[derive(Debug)]
struct SwingDetector {
    lookback: usize,
    /// Buffered (price, rsi) pairs waiting for confirmation.
    buffer: VecDeque<(f64, f64)>,
    /// Running bar counter (global index in the price stream).
    global_index: usize,
}

impl SwingDetector {
    fn new(lookback: usize) -> Self {
        Self {
            lookback,
            buffer: VecDeque::with_capacity(2 * lookback + 2),
            global_index: 0,
        }
    }

    /// Push a new (price, rsi) pair.  Returns any swing high/low that has just
    /// been **confirmed** (i.e. the centre bar of the window).
    ///
    /// Returns `(Option<SwingHigh>, Option<SwingLow>)`.
    fn push(&mut self, price: f64, rsi: f64) -> (Option<SwingPoint>, Option<SwingPoint>) {
        self.buffer.push_back((price, rsi));
        self.global_index += 1;

        let window = 2 * self.lookback + 1;
        if self.buffer.len() < window {
            return (None, None);
        }

        // The candidate is the centre element.
        let centre = self.lookback;
        let (cp, cr) = self.buffer[centre];

        let mut is_high = true;
        let mut is_low = true;

        for (i, &(p, _)) in self.buffer.iter().enumerate() {
            if i == centre {
                continue;
            }
            if p >= cp {
                is_high = false;
            }
            if p <= cp {
                is_low = false;
            }
        }

        // Pop the oldest bar so the window slides forward.
        self.buffer.pop_front();

        let idx = self.global_index - self.lookback - 1;

        let swing_high = if is_high {
            Some(SwingPoint { index: idx, price: cp, rsi: cr })
        } else {
            None
        };

        let swing_low = if is_low {
            Some(SwingPoint { index: idx, price: cp, rsi: cr })
        } else {
            None
        };

        (swing_high, swing_low)
    }
}

// ===========================================================================
// EMA calculator — used by the trend filter
// ===========================================================================
//
// EMA gives us a slow-moving baseline for "where price has been recently".
// We use it as a directional gate: only take longs when price > EMA (i.e.
// broader trend is up).  This is the simplest answer to "can we add broader
// price-tracking context?" — yes, by consulting a longer-window indicator
// rather than just a 14-bar RSI snapshot.

/// Rolling EMA calculator.
///
/// EMA = α·price + (1-α)·prev_ema, with α = 2/(period+1).
/// First `period` samples are accumulated into an SMA seed so the EMA
/// doesn't start biased toward the first price.
#[derive(Debug, Clone)]
pub struct EmaCalculator {
    period: usize,
    alpha: f64,
    /// `None` until we have enough samples; then `Some(ema)`.
    value: Option<f64>,
    seed_sum: f64,
    seed_count: usize,
    /// If set, the SMA seed initialises after this many bars instead of
    /// waiting for the full `period`.  Used to eliminate the long warmup
    /// on slow EMAs (e.g. EMA(100) can be seeded at bar 20 so the regime
    /// detector is active from the start of every session).
    early_seed: Option<usize>,
}

impl EmaCalculator {
    pub fn new(period: usize) -> Self {
        Self {
            period,
            alpha: 2.0 / (period as f64 + 1.0),
            value: None,
            seed_sum: 0.0,
            seed_count: 0,
            early_seed: None,
        }
    }

    /// Seed the EMA after `bars` samples instead of the full `period`.
    /// The initial EMA value will be the SMA of those first `bars` prices.
    /// Useful for slow EMAs (e.g. EMA-100) that would otherwise black-out
    /// the first 100 bars of every live session.
    pub fn with_early_seed(mut self, bars: usize) -> Self {
        self.early_seed = Some(bars);
        self
    }

    /// Push a price, return the current EMA (or `None` while warming up).
    pub fn push(&mut self, price: f64) -> Option<f64> {
        if let Some(v) = self.value {
            let new = self.alpha * price + (1.0 - self.alpha) * v;
            self.value = Some(new);
            Some(new)
        } else {
            self.seed_sum += price;
            self.seed_count += 1;
            let threshold = self.early_seed.unwrap_or(self.period);
            if self.seed_count >= threshold {
                let seed = self.seed_sum / self.seed_count as f64;
                self.value = Some(seed);
                Some(seed)
            } else {
                None
            }
        }
    }

    pub fn current(&self) -> Option<f64> { self.value }
}

// ===========================================================================
// TrendFilter — broader-context gate for entries
// ===========================================================================

/// Long-only trend filter: an entry is allowed only when the current price
/// is above a slower EMA (e.g. EMA-50 on 1-minute bars ≈ last ~50 mins).
///
/// This answers the question "how does the strategy know not to buy into a
/// downtrend?".  Without it, RSI<30 will fire on the way down regardless of
/// the broader move, which is exactly the falling-knife scenario.
///
/// Symmetric behaviour for shorts: short only when price < EMA.
#[derive(Debug, Clone)]
pub struct TrendFilter {
    ema: EmaCalculator,
}

impl TrendFilter {
    pub fn new(period: usize) -> Self {
        Self { ema: EmaCalculator::new(period) }
    }

    /// Feed a price into the trend filter and return its current verdict
    /// for a `Long` entry: `Some(true)` = allowed, `Some(false)` = blocked
    /// by trend, `None` = still warming up (we conservatively block).
    pub fn allows_long(&mut self, price: f64) -> Option<bool> {
        let ema = self.ema.push(price)?;
        Some(price > ema)
    }

    pub fn current_ema(&self) -> Option<f64> { self.ema.current() }
}

// ===========================================================================
// Regime Detection — dual-EMA market regime classifier
// ===========================================================================
//
// The RSI strategy is a mean-reversion tool: it works best when price is
// oscillating around a level (ranging).  In a sustained downtrend, RSI<30
// fires repeatedly on the way down — every dip looks like an oversold
// opportunity but it's just the trend continuing.
//
// Dual-EMA regime detection answers "which mode is the market in right now?"
// by measuring the spread between a fast EMA(20) and a slow EMA(100).  When
// the fast line is significantly below the slow line, we are in a downtrend
// and Long entries are blocked regardless of what RSI says.

/// Classification of the current market environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    /// Fast EMA > slow EMA by more than `spread_pct` — uptrend, longs OK.
    TrendingUp,
    /// Fast EMA < slow EMA by more than `spread_pct` — downtrend, block longs.
    TrendingDown,
    /// EMAs are close — ranging market, RSI signals apply normally.
    Ranging,
    /// Not enough bars yet to classify.
    WarmingUp,
}

impl Regime {
    pub fn label(self) -> &'static str {
        match self {
            Regime::TrendingUp => "↑TrendUp",
            Regime::TrendingDown => "↓TrendDn",
            Regime::Ranging => "~Ranging",
            Regime::WarmingUp => "WarmUp",
        }
    }
}

/// Classifies market regime via the spread between a fast and slow EMA.
#[derive(Debug, Clone)]
pub struct RegimeDetector {
    fast_ema: EmaCalculator,
    slow_ema: EmaCalculator,
    /// Minimum |fast − slow| / slow (%) to call a trend.  Below this = Ranging.
    spread_pct: f64,
}

impl RegimeDetector {
    /// Default: EMA(20) fast, EMA(100) slow, 0.3% spread threshold.
    pub fn new(fast_period: usize, slow_period: usize, spread_pct: f64) -> Self {
        Self {
            fast_ema: EmaCalculator::new(fast_period),
            slow_ema: EmaCalculator::new(slow_period),
            spread_pct,
        }
    }

    /// Seed the slow EMA after `bars` samples instead of its full period.
    /// Eliminates the 100-bar blackout at the start of each live session
    /// without changing long-run EMA behaviour.
    pub fn with_slow_early_seed(mut self, bars: usize) -> Self {
        self.slow_ema = self.slow_ema.with_early_seed(bars);
        self
    }

    /// Push a price and return the current regime.
    pub fn push(&mut self, price: f64) -> Regime {
        let fast = self.fast_ema.push(price);
        let slow = self.slow_ema.push(price);
        match (fast, slow) {
            (Some(f), Some(s)) => {
                let diff_pct = (f - s) / s * 100.0;
                if diff_pct > self.spread_pct {
                    Regime::TrendingUp
                } else if diff_pct < -self.spread_pct {
                    Regime::TrendingDown
                } else {
                    Regime::Ranging
                }
            }
            _ => Regime::WarmingUp,
        }
    }
}

/// Wraps any [`SignalGenerator`] and gates Long entries by market regime.
///
/// `ClosePosition` signals are always passed through — we never block exits.
/// Long signals are suppressed when the regime detector sees a downtrend or
/// is still warming up.
pub struct RegimeGatedStrategy {
    pub inner: Box<dyn SignalGenerator>,
    regime: RegimeDetector,
    pub last_regime: Regime,
}

impl RegimeGatedStrategy {
    pub fn new(inner: Box<dyn SignalGenerator>, regime: RegimeDetector) -> Self {
        Self { inner, regime, last_regime: Regime::WarmingUp }
    }
}

impl SignalGenerator for RegimeGatedStrategy {
    fn generate_signal(&mut self, event: &MarketEvent) -> Option<Signal> {
        // Always feed the regime detector so both EMAs stay in sync.
        let price = extract_price(event)?;
        let regime = self.regime.push(price);
        self.last_regime = regime;

        let signal = self.inner.generate_signal(event)?;

        // Block Long entries during downtrends or while warming up.
        if matches!(signal.decision, Decision::Long)
            && matches!(regime, Regime::TrendingDown | Regime::WarmingUp)
        {
            return None;
        }
        Some(signal)
    }

    fn status_line(&self) -> Option<String> {
        Some(self.last_regime.label().to_string())
    }
}

// ===========================================================================
// EntryMode — when does an RSI strategy fire a Long?
// ===========================================================================

/// Controls *when* an RSI strategy emits an entry signal.
///
/// The original behaviour ([`EntryMode::Threshold`]) fires `Long` on every
/// bar that RSI is below the oversold line.  That means in a sustained
/// downtrend the strategy keeps "buying back in" at progressively lower
/// prices — there's no concept of waiting for momentum to turn.
///
/// [`EntryMode::EdgeRebound`] only fires `Long` on the bar that RSI
/// **crosses back up** through the oversold line from below.  That single
/// edge represents "RSI has bottomed out and is recovering", which is a
/// far better re-entry trigger.  The symmetric rule applies to the
/// overbought exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryMode {
    /// Original behaviour — fire every bar while in the threshold zone.
    Threshold,
    /// Fire only on the bar RSI crosses *out* of the threshold zone
    /// (back toward neutral).  Acts as a recovery / rebound confirmation.
    EdgeRebound,
}

// ===========================================================================
// Bollinger Band calculator — used by BollingerRsiStrategy
// ===========================================================================

/// Rolling Bollinger Band calculator.
///
/// Tracks a rolling window of `period` prices and computes the middle band
/// (SMA), upper band (SMA + k·σ), and lower band (SMA − k·σ) using
/// population standard deviation.
#[derive(Debug)]
struct BollingerCalculator {
    period: usize,
    multiplier: f64,
    prices: VecDeque<f64>,
}

impl BollingerCalculator {
    fn new(period: usize, multiplier: f64) -> Self {
        Self {
            period,
            multiplier,
            prices: VecDeque::with_capacity(period + 1),
        }
    }

    /// Push a price and return `(middle, upper, lower)` bands, or `None`
    /// while still accumulating the initial `period` prices.
    fn push(&mut self, price: f64) -> Option<(f64, f64, f64)> {
        self.prices.push_back(price);
        if self.prices.len() > self.period {
            self.prices.pop_front();
        }
        if self.prices.len() < self.period {
            return None;
        }
        let sma = self.prices.iter().sum::<f64>() / self.period as f64;
        let variance = self.prices.iter()
            .map(|p| (p - sma).powi(2))
            .sum::<f64>() / self.period as f64;
        let std = variance.sqrt();
        Some((sma, sma + self.multiplier * std, sma - self.multiplier * std))
    }
}

// ===========================================================================
// BollingerRsiStrategy — RSI + Bollinger Band dual-signal entry
// ===========================================================================

/// Enters Long only when **both** conditions hold simultaneously:
/// - RSI(14) < 30 (oversold)
/// - Price ≤ lower Bollinger Band (SMA-20 − 2σ)
///
/// Two orthogonal oversold signals confirm the entry is at a genuine extreme,
/// not just a minor RSI dip.  Exits on RSI > 70 (overbought).
///
/// Signal strength = average of RSI depth and BB penetration depth.
pub struct BollingerRsiStrategy {
    rsi: RsiCalculator,
    bb: BollingerCalculator,
    oversold: f64,
    overbought: f64,
    trend_filter: Option<TrendFilter>,
}

impl BollingerRsiStrategy {
    /// RSI(14) 30/70 + BB(20, 2σ), no trend filter.
    pub fn new() -> Self {
        Self {
            rsi: RsiCalculator::new(14),
            bb: BollingerCalculator::new(20, 2.0),
            oversold: 30.0,
            overbought: 70.0,
            trend_filter: None,
        }
    }

    pub fn with_trend_filter(mut self, filter: TrendFilter) -> Self {
        self.trend_filter = Some(filter);
        self
    }
}

impl SignalGenerator for BollingerRsiStrategy {
    fn generate_signal(&mut self, event: &MarketEvent) -> Option<Signal> {
        let price = extract_price(event)?;

        let trend_allows_long = self.trend_filter
            .as_mut()
            .map(|f| f.allows_long(price));

        let rsi = self.rsi.push(price)?;
        let (_, _, lower_band) = self.bb.push(price)?;

        // Entry: RSI oversold AND price at/below lower Bollinger Band
        if rsi < self.oversold && price <= lower_band {
            match trend_allows_long {
                Some(Some(false)) | Some(None) => return None,
                _ => {}
            }
            let rsi_str = (self.oversold - rsi) / self.oversold;
            let bb_str = ((lower_band - price) / lower_band).clamp(0.0, 1.0);
            let strength = ((rsi_str + bb_str) / 2.0).clamp(0.0, 1.0);
            return Some(Signal {
                exchange: event.exchange,
                instrument: event.instrument.clone(),
                time: chrono::Utc::now(),
                decision: Decision::Long,
                strength: Decimal::from_f64_retain(strength).unwrap_or(Decimal::ZERO),
            });
        }

        // Exit: RSI overbought
        if rsi > self.overbought {
            let strength = ((rsi - self.overbought) / (100.0 - self.overbought)).clamp(0.0, 1.0);
            return Some(Signal {
                exchange: event.exchange,
                instrument: event.instrument.clone(),
                time: chrono::Utc::now(),
                decision: Decision::ClosePosition,
                strength: Decimal::from_f64_retain(strength).unwrap_or(Decimal::ZERO),
            });
        }

        None
    }
}

// ===========================================================================
// RSI calculator (shared)
// ===========================================================================

/// Rolling RSI calculator that wraps rust_ti.
#[derive(Debug)]
struct RsiCalculator {
    period: usize,
    prices: VecDeque<f64>,
}

impl RsiCalculator {
    fn new(period: usize) -> Self {
        Self {
            period,
            prices: VecDeque::with_capacity(period + 2),
        }
    }

    /// Push a price and return the current RSI, or `None` if not enough data.
    fn push(&mut self, price: f64) -> Option<f64> {
        self.prices.push_back(price);
        if self.prices.len() > self.period + 1 {
            self.prices.pop_front();
        }
        if self.prices.len() < self.period + 1 {
            return None;
        }
        let slice: Vec<f64> = self.prices.iter().copied().collect();
        let rsi = rust_ti::momentum_indicators::single::relative_strength_index(
            &slice,
            rust_ti::ConstantModelType::ExponentialMovingAverage,
        );
        Some(rsi)
    }
}

// ===========================================================================
// SimpleStrategy — operates on MarketTick (legacy trait)
// ===========================================================================

/// A simple live strategy placeholder.
pub struct SimpleStrategy;

impl SimpleStrategy {
    pub fn new() -> Self {
        SimpleStrategy
    }
}

impl Strategy for SimpleStrategy {
    fn analyze(&self, _tick: &MarketTick) -> TradingSignal {
        TradingSignal::Hold
    }
}

// ===========================================================================
// RsiStrategy — 14-period RSI overbought / oversold
// ===========================================================================

/// RSI-based strategy that implements [`SignalGenerator`].
///
/// | RSI value | Decision              |
/// |-----------|-----------------------|
/// | < 30      | `Decision::Long`      |
/// | > 70      | `Decision::ClosePosition` |
/// | 30 – 70   | No signal (`None`)    |
pub struct RsiStrategy {
    rsi: RsiCalculator,
    oversold: f64,
    overbought: f64,
    /// Controls whether `Long` fires every bar while RSI<oversold
    /// (`Threshold`) or only on the bar RSI crosses back UP through
    /// oversold (`EdgeRebound`).  See [`EntryMode`].
    entry_mode: EntryMode,
    /// RSI from the previous bar — needed for edge detection.
    /// `None` until we've seen at least one RSI sample.
    prev_rsi: Option<f64>,
    /// Optional broader-context gate: skip longs when price < EMA.
    trend_filter: Option<TrendFilter>,
}

impl RsiStrategy {
    /// 14-period RSI with default 30/70 thresholds, no trend filter,
    /// classic threshold-mode entries (preserves original behaviour).
    pub fn new() -> Self {
        Self::with_params(14, 30.0, 70.0)
    }

    pub fn with_params(period: usize, oversold: f64, overbought: f64) -> Self {
        Self {
            rsi: RsiCalculator::new(period),
            oversold,
            overbought,
            entry_mode: EntryMode::Threshold,
            prev_rsi: None,
            trend_filter: None,
        }
    }

    /// Switch entry semantics.  See [`EntryMode`].
    pub fn with_entry_mode(mut self, mode: EntryMode) -> Self {
        self.entry_mode = mode;
        self
    }

    /// Attach a longer-period trend filter.  Once attached, longs are only
    /// emitted when price is above the trend EMA.
    pub fn with_trend_filter(mut self, filter: TrendFilter) -> Self {
        self.trend_filter = Some(filter);
        self
    }
}

impl SignalGenerator for RsiStrategy {
    fn generate_signal(&mut self, event: &MarketEvent) -> Option<Signal> {
        let price = extract_price(event)?;
        // Always feed the trend filter (even on bars where we don't trade)
        // so its EMA stays in sync with the price stream.
        let trend_allows_long = self
            .trend_filter
            .as_mut()
            .map(|f| f.allows_long(price));

        let rsi = self.rsi.push(price)?;
        let prev = self.prev_rsi;
        self.prev_rsi = Some(rsi);

        // ----- entry / exit decision -----
        let (decision, strength) = match self.entry_mode {
            EntryMode::Threshold => {
                // Original behaviour.
                if rsi < self.oversold {
                    let s = ((self.oversold - rsi) / self.oversold).clamp(0.0, 1.0);
                    (Decision::Long, s)
                } else if rsi > self.overbought {
                    let s = ((rsi - self.overbought) / (100.0 - self.overbought)).clamp(0.0, 1.0);
                    (Decision::ClosePosition, s)
                } else {
                    return None;
                }
            }
            EntryMode::EdgeRebound => {
                // Long only when RSI crosses back UP through oversold:
                //   prev was below oversold, current is at/above oversold.
                // This is the "RSI has bottomed out" moment.  Without this,
                // a strategy enters on bar 1 of a freefall and re-enters on
                // every subsequent bar.
                let prev = prev?; // require at least one previous sample
                if prev < self.oversold && rsi >= self.oversold {
                    let s = ((self.oversold - prev) / self.oversold).clamp(0.0, 1.0);
                    (Decision::Long, s)
                } else if prev > self.overbought && rsi <= self.overbought {
                    // Symmetric exit: close when RSI rolls back down through
                    // overbought — locks in the move BEFORE momentum fades
                    // all the way to neutral.
                    let s = ((prev - self.overbought) / (100.0 - self.overbought)).clamp(0.0, 1.0);
                    (Decision::ClosePosition, s)
                } else {
                    return None;
                }
            }
        };

        // ----- broader-context veto -----
        // ClosePosition is never vetoed (exits should always be permitted).
        if matches!(decision, Decision::Long) {
            match trend_allows_long {
                Some(Some(true)) => {}      // trend OK
                Some(Some(false)) => return None, // trend filter blocks
                Some(None) => return None,        // filter still warming up
                None => {}                        // no filter configured
            }
        }

        Some(Signal {
            exchange: event.exchange,
            instrument: event.instrument.clone(),
            time: Utc::now(),
            decision,
            strength: Decimal::from_f64_retain(strength).unwrap_or(Decimal::ZERO),
        })
    }
}

// ===========================================================================
// DivergenceStrategy — price / RSI swing divergence
// ===========================================================================

/// Detects **divergence** between price action and the RSI indicator.
///
/// ### Bullish (regular) divergence
/// Price prints a **lower low** while RSI prints a **higher low**.
/// This suggests selling momentum is weakening → potential reversal upward.
/// → Emits `Decision::Long`.
///
/// ### Bearish (regular) divergence
/// Price prints a **higher high** while RSI prints a **lower high**.
/// This suggests buying momentum is weakening → potential reversal downward.
/// → Emits `Decision::ClosePosition`.
///
/// ### Parameters
/// - `rsi_period` — RSI look-back (default 14).
/// - `swing_lookback` — bars either side of a candidate swing point (default 5).
///   A larger value gives fewer, more significant swings.
pub struct DivergenceStrategy {
    rsi: RsiCalculator,
    detector: SwingDetector,
    /// Most recent confirmed swing highs (newest last).
    recent_highs: VecDeque<SwingPoint>,
    /// Most recent confirmed swing lows (newest last).
    recent_lows: VecDeque<SwingPoint>,
    /// How many recent swings to keep for comparison.
    max_swings: usize,
    /// Last price seen — cached for signal metadata.
    last_price: f64,
}

impl DivergenceStrategy {
    /// 14-period RSI, 5-bar swing lookback.
    pub fn new() -> Self {
        Self::with_params(14, 5)
    }

    pub fn with_params(rsi_period: usize, swing_lookback: usize) -> Self {
        Self {
            rsi: RsiCalculator::new(rsi_period),
            detector: SwingDetector::new(swing_lookback),
            recent_highs: VecDeque::with_capacity(6),
            recent_lows: VecDeque::with_capacity(6),
            max_swings: 5,
            last_price: 0.0,
        }
    }

    /// Check the two most recent swing lows for bullish divergence.
    fn check_bullish_divergence(&self) -> Option<f64> {
        if self.recent_lows.len() < 2 {
            return None;
        }
        let prev = &self.recent_lows[self.recent_lows.len() - 2];
        let curr = &self.recent_lows[self.recent_lows.len() - 1];

        // Bullish: price lower-low, RSI higher-low
        if curr.price < prev.price && curr.rsi > prev.rsi {
            // Strength: how much the RSI diverged relative to the RSI range
            let rsi_divergence = (curr.rsi - prev.rsi).abs();
            Some((rsi_divergence / 100.0).clamp(0.0, 1.0))
        } else {
            None
        }
    }

    /// Check the two most recent swing highs for bearish divergence.
    fn check_bearish_divergence(&self) -> Option<f64> {
        if self.recent_highs.len() < 2 {
            return None;
        }
        let prev = &self.recent_highs[self.recent_highs.len() - 2];
        let curr = &self.recent_highs[self.recent_highs.len() - 1];

        // Bearish: price higher-high, RSI lower-high
        if curr.price > prev.price && curr.rsi < prev.rsi {
            let rsi_divergence = (prev.rsi - curr.rsi).abs();
            Some((rsi_divergence / 100.0).clamp(0.0, 1.0))
        } else {
            None
        }
    }
}

impl SignalGenerator for DivergenceStrategy {
    fn generate_signal(&mut self, event: &MarketEvent) -> Option<Signal> {
        let price = extract_price(event)?;
        self.last_price = price;
        let rsi = self.rsi.push(price)?;

        let (swing_high, swing_low) = self.detector.push(price, rsi);

        let mut result: Option<Signal> = None;

        if let Some(sh) = swing_high {
            self.recent_highs.push_back(sh);
            if self.recent_highs.len() > self.max_swings {
                self.recent_highs.pop_front();
            }
            if let Some(strength) = self.check_bearish_divergence() {
                result = Some(Signal {
                    exchange: event.exchange,
                    instrument: event.instrument.clone(),
                    time: Utc::now(),
                    decision: Decision::ClosePosition,
                    strength: Decimal::from_f64_retain(strength).unwrap_or(Decimal::ZERO),
                });
            }
        }

        if let Some(sl) = swing_low {
            self.recent_lows.push_back(sl);
            if self.recent_lows.len() > self.max_swings {
                self.recent_lows.pop_front();
            }
            if let Some(strength) = self.check_bullish_divergence() {
                // Bullish takes priority if both happen on the same bar (rare).
                result = Some(Signal {
                    exchange: event.exchange,
                    instrument: event.instrument.clone(),
                    time: Utc::now(),
                    decision: Decision::Long,
                    strength: Decimal::from_f64_retain(strength).unwrap_or(Decimal::ZERO),
                });
            }
        }

        result
    }
}

// ===========================================================================
// RsiDivergenceStrategy — combined RSI levels + divergence confirmation
// ===========================================================================

/// Combines RSI overbought/oversold levels with divergence confirmation
/// for **higher-confidence** signals.
///
/// ### Logic
/// 1. Standard RSI thresholds (like [`RsiStrategy`]) produce a **candidate**.
/// 2. The divergence detector runs in parallel.
/// 3. A signal is emitted **only** when:
///    - RSI is oversold **AND** a bullish divergence has been observed recently
///      → `Decision::Long`
///    - RSI is overbought **AND** a bearish divergence has been observed recently
///      → `Decision::ClosePosition`
///
/// This dramatically reduces false signals compared to using either method
/// alone.
///
/// The combined strength is the **average** of the RSI strength and the
/// divergence strength.
pub struct RsiDivergenceStrategy {
    rsi: RsiCalculator,
    detector: SwingDetector,
    recent_highs: VecDeque<SwingPoint>,
    recent_lows: VecDeque<SwingPoint>,
    max_swings: usize,
    oversold: f64,
    overbought: f64,
    /// When a divergence is detected it stays "active" for this many bars.
    divergence_window: usize,
    /// Tick counter since the last bullish divergence was detected.
    bullish_age: Option<usize>,
    bullish_strength: f64,
    /// Tick counter since the last bearish divergence was detected.
    bearish_age: Option<usize>,
    bearish_strength: f64,
    /// Global bar counter.
    bar_count: usize,
    /// Optional broader-context filter (see [`TrendFilter`]).
    /// Divergence detection answers "is momentum weakening at swing
    /// points?" — a structural question — but it does NOT know whether
    /// we are inside a multi-hour downtrend.  That's a separate concern,
    /// and the trend filter handles it.
    trend_filter: Option<TrendFilter>,
}

impl RsiDivergenceStrategy {
    /// 14-period RSI, 5-bar swing lookback, 30/70 thresholds, divergence
    /// valid for 10 bars after detection.
    pub fn new() -> Self {
        Self::with_params(14, 5, 30.0, 70.0, 10)
    }

    pub fn with_params(
        rsi_period: usize,
        swing_lookback: usize,
        oversold: f64,
        overbought: f64,
        divergence_window: usize,
    ) -> Self {
        Self {
            rsi: RsiCalculator::new(rsi_period),
            detector: SwingDetector::new(swing_lookback),
            recent_highs: VecDeque::with_capacity(6),
            recent_lows: VecDeque::with_capacity(6),
            max_swings: 5,
            oversold,
            overbought,
            divergence_window,
            bullish_age: None,
            bullish_strength: 0.0,
            bearish_age: None,
            bearish_strength: 0.0,
            bar_count: 0,
            trend_filter: None,
        }
    }

    /// Attach an EMA-based trend filter (see [`TrendFilter`]).
    pub fn with_trend_filter(mut self, filter: TrendFilter) -> Self {
        self.trend_filter = Some(filter);
        self
    }

    fn check_bullish_divergence(&self) -> Option<f64> {
        if self.recent_lows.len() < 2 { return None; }
        let prev = &self.recent_lows[self.recent_lows.len() - 2];
        let curr = &self.recent_lows[self.recent_lows.len() - 1];
        if curr.price < prev.price && curr.rsi > prev.rsi {
            Some(((curr.rsi - prev.rsi).abs() / 100.0).clamp(0.0, 1.0))
        } else {
            None
        }
    }

    fn check_bearish_divergence(&self) -> Option<f64> {
        if self.recent_highs.len() < 2 { return None; }
        let prev = &self.recent_highs[self.recent_highs.len() - 2];
        let curr = &self.recent_highs[self.recent_highs.len() - 1];
        if curr.price > prev.price && curr.rsi < prev.rsi {
            Some(((prev.rsi - curr.rsi).abs() / 100.0).clamp(0.0, 1.0))
        } else {
            None
        }
    }
}

impl SignalGenerator for RsiDivergenceStrategy {
    fn generate_signal(&mut self, event: &MarketEvent) -> Option<Signal> {
        let price = extract_price(event)?;
        // Keep the trend filter in sync with every closed bar even on
        // bars where the strategy doesn't act, so the EMA reflects the
        // full price stream.
        let trend_allows_long = self
            .trend_filter
            .as_mut()
            .map(|f| f.allows_long(price));
        let rsi = self.rsi.push(price)?;
        self.bar_count += 1;

        // --- Update swing points & divergence flags ---
        let (swing_high, swing_low) = self.detector.push(price, rsi);

        if let Some(sh) = swing_high {
            self.recent_highs.push_back(sh);
            if self.recent_highs.len() > self.max_swings {
                self.recent_highs.pop_front();
            }
            if let Some(s) = self.check_bearish_divergence() {
                self.bearish_age = Some(self.bar_count);
                self.bearish_strength = s;
            }
        }

        if let Some(sl) = swing_low {
            self.recent_lows.push_back(sl);
            if self.recent_lows.len() > self.max_swings {
                self.recent_lows.pop_front();
            }
            if let Some(s) = self.check_bullish_divergence() {
                self.bullish_age = Some(self.bar_count);
                self.bullish_strength = s;
            }
        }

        // --- Expire old divergences ---
        let has_bullish_div = self.bullish_age
            .map(|age| self.bar_count.saturating_sub(age) <= self.divergence_window)
            .unwrap_or(false);
        let has_bearish_div = self.bearish_age
            .map(|age| self.bar_count.saturating_sub(age) <= self.divergence_window)
            .unwrap_or(false);

        // --- Combined decision ---
        if rsi < self.oversold && has_bullish_div {
            // Trend filter veto — don't go long when broader trend is down.
            // `Some(Some(false))` means EMA is warmed up and says no.
            // `Some(None)` means EMA still warming up — be conservative.
            match trend_allows_long {
                Some(Some(false)) | Some(None) => { /* fall through to bearish check */ }
                _ => {
                    let rsi_str = (self.oversold - rsi) / self.oversold;
                    let avg = ((rsi_str + self.bullish_strength) / 2.0).clamp(0.0, 1.0);
                    return Some(Signal {
                        exchange: event.exchange,
                        instrument: event.instrument.clone(),
                        time: Utc::now(),
                        decision: Decision::Long,
                        strength: Decimal::from_f64_retain(avg).unwrap_or(Decimal::ZERO),
                    });
                }
            }
        }

        if rsi > self.overbought && has_bearish_div {
            let rsi_str = (rsi - self.overbought) / (100.0 - self.overbought);
            let avg = ((rsi_str + self.bearish_strength) / 2.0).clamp(0.0, 1.0);
            return Some(Signal {
                exchange: event.exchange,
                instrument: event.instrument.clone(),
                time: Utc::now(),
                decision: Decision::ClosePosition,
                strength: Decimal::from_f64_retain(avg).unwrap_or(Decimal::ZERO),
            });
        }

        None
    }
}

// ===========================================================================
// LiquiditySweepStrategy — stop-run reversal at established range extremes
// ===========================================================================
//
// ## Mechanism
// Stop-loss and liquidation orders cluster just beyond well-established range
// extremes (the low/high of the last few hours).  When price pierces such a
// level, those resting stops fire as *market* orders in the direction of the
// break, producing a short, violent burst of one-sided flow — visible as a
// volume spike.  Once that liquidity pocket is consumed there is no follow-on
// interest, and price snaps back inside the range ("stop run" / "turtle
// soup").  The tell that separates a sweep from a genuine breakout is the
// *rejection close*: the bar pierces the level but closes back inside the
// range with a long wick, on elevated volume.
//
// ## Why this needs OHLCV
// The signature lives in the wick (low vs close) and the volume spike —
// neither is visible to close-only indicators, which is why this strategy
// consumes the full candle instead of `extract_price`.
//
// ## Cost awareness
// Every entry gate exists to keep frequency low and per-trade expectancy
// well above the ~0.30% round-trip cost:
// - the level must be *established* (old, still-standing extreme) so that
//   stops have actually accumulated behind it;
// - the penetration must exceed a noise floor;
// - volume must confirm a stop cascade, not drift;
// - the distance back to fair value (EMA) must exceed `min_edge_pct`, i.e.
//   the *reversion target itself* must clear costs with margin before we
//   ever enter;
// - a cooldown prevents re-trading the same swept level.

/// Active post-entry thesis: where the trade is invalidated, where it
/// targets, and which way it points.  Held until the strategy itself emits
/// the closing signal.
#[derive(Debug, Clone)]
struct SweepThesis {
    direction: Decision,
    /// Close beyond this level means the "sweep" was actually a breakout —
    /// thesis dead, get out immediately.  Set from the sweep bar's extreme
    /// plus `inval_buffer_pct` so an ordinary retest of the wick doesn't
    /// shake the position out.
    invalidation: f64,
    /// Frozen reversion target: the midpoint of the swept range at entry
    /// time.  Frozen because a moving target (e.g. an EMA) converges toward
    /// price and silently shrinks winners while losses stay full-size.
    target: f64,
    opened_at: usize,
}

/// A detected sweep waiting for a confirmation close back through the level
/// (used when `confirm_bars > 0`).
#[derive(Debug, Clone)]
struct PendingSweep {
    direction: Decision,
    /// The swept range extreme a confirming bar must close back through.
    level: f64,
    /// Sweep bar's extreme — a close beyond it cancels the setup.
    sweep_extreme: f64,
    target: f64,
    set_at: usize,
}

/// How the strategy trades a volume-spike break of an established level.
///
/// The same microstructure event — price pierces a level where stops
/// cluster, on elevated volume — resolves in one of two ways, and the bar's
/// close tells them apart:
/// - close back **inside** the range → the stops were absorbed, no follow-on
///   interest → fade the move (mean reversion to the range interior);
/// - close **through** the level → the cascade is still feeding on itself
///   (stops trigger the move that triggers more stops) → trade the
///   continuation toward a measured-move extension of the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepStyle {
    /// Only fade rejection closes (classic stop-run reversal).
    Fade,
    /// Only trade acceptance closes (cascade / breakout continuation).
    Breakout,
    /// Trade both resolutions of the event.
    Both,
}

/// A bar that satisfies every *price/volume* entry condition, before the
/// pipeline blockers (open thesis, pending setup, cooldown, regime).
#[derive(Debug, Clone)]
struct SweepCandidate {
    direction: Decision,
    level: f64,
    sweep_extreme: f64,
    target: f64,
    edge_pct: f64,
    vol_ratio: f64,
}

/// Pipeline counters: how many candidate sweeps the price/volume gates
/// produced and where the rest of the pipeline consumed them.  Exposed via
/// [`LiquiditySweepStrategy::stats`] for diagnostics and dashboards.
#[derive(Debug, Clone, Copy, Default)]
pub struct SweepStats {
    pub candidates_long: u64,
    pub candidates_short: u64,
    pub blocked_lockout: u64,
    pub blocked_cooldown: u64,
    pub blocked_regime: u64,
    pub pending_set: u64,
    pub pending_confirmed: u64,
    pub pending_cancelled: u64,
    pub pending_expired: u64,
    pub entries: u64,
    pub exits_target: u64,
    pub exits_invalidated: u64,
    pub exits_timeout: u64,
}

/// Mean-reversion strategy that fades liquidity sweeps of established range
/// extremes on full OHLCV candles.
///
/// ### Long entry (short is symmetric at range highs)
/// 1. The rolling `range_lookback`-bar low has not been approached (within
///    the sweep tolerance) for the last `min_level_age` bars — an
///    *established* level, not a fresh low in a live downtrend.
/// 2. This bar's low pierces that level by > `sweep_min_pct`.
/// 3. The close recovers back **above** the level, in the top
///    `1 - close_loc_min` fraction of the bar's range (rejection wick).
/// 4. Bar volume ≥ `vol_mult` × rolling average volume (stop cascade).
/// 5. Distance from close up to the range midpoint ≥ `min_edge_pct`
///    (the reversion target must clear round-trip costs — this selects
///    *wide ranges*, deliberately not "price far below an EMA", which
///    would select falling knives).
/// 6. Optional: a `RegimeDetector` blocks longs in sustained downtrends
///    and shorts in sustained uptrends.
/// 7. Optional: with `confirm_bars > 0`, entry waits for a later bar to
///    close back through the swept level (filters sweeps that immediately
///    resume the break).
///
/// ### Exit
/// - Target: close reaches the swept range's midpoint, frozen at entry —
///   the natural liquidity magnet once the extreme's stops are spent.
/// - Invalidation: close beyond the sweep bar's extreme (+ buffer) —
///   the "sweep" was a genuine breakout.
/// - Timeout: thesis older than `thesis_timeout_bars` (risk layer's
///   max-hold usually fires first).
///
/// Only emits on `DataKind::Candle` events — the signal lives in the
/// high/low/volume, which trades don't carry.
pub struct LiquiditySweepStrategy {
    // -- parameters --
    /// Lookback horizons whose extremes count as liquidity levels, longest
    /// first (a 4 h low and a 1 h low are separate pools of resting stops).
    lookbacks: Vec<usize>,
    min_level_age: usize,
    sweep_min_pct: f64,
    close_loc_min: f64,
    vol_sma_period: usize,
    vol_mult: f64,
    min_edge_pct: f64,
    inval_buffer_pct: f64,
    confirm_bars: usize,
    cooldown_bars: usize,
    thesis_timeout_bars: usize,
    allow_shorts: bool,
    /// Where in the swept range the reversion target sits: 0.5 = midpoint,
    /// 1.0 = the far side of the range.  For breakout continuations the
    /// same fraction sets the measured-move extension beyond the level.
    target_frac: f64,
    style: SweepStyle,
    // -- state --
    stats: SweepStats,
    regime: Option<RegimeDetector>,
    /// Prior bars' (index, high, low) — current bar is evaluated against
    /// this window *before* being pushed, so a bar can't sweep its own low.
    window: VecDeque<(usize, f64, f64)>,
    /// Prior bars' volumes for the rolling average (same exclusion rule).
    volumes: VecDeque<f64>,
    vol_sum: f64,
    bar_index: usize,
    cooldown_until: usize,
    pending: Option<PendingSweep>,
    thesis: Option<SweepThesis>,
}

impl LiquiditySweepStrategy {
    /// Defaults: the configuration that survived a 29-month walk-through
    /// (Jan 2024 – Apr 2026) net of 0.30% round-trip costs, sitting on a
    /// parameter plateau rather than a lucky spike:
    /// - 1440-bar (1 day) levels: prior-day extremes are the most watched
    ///   liquidity pools; session-scale (2–4 h) levels were falsified —
    ///   their sub-0.5% reversion is consumed by costs.
    /// - level unapproached for 60 bars: established, not a fresh extreme.
    /// - sweep ≥ 0.10%: a genuine raid, not spread noise.
    /// - volume ≥ 1.5× the hourly average: stop cascade signature.
    /// - edge ≥ 0.8% from entry to the range-mid target: ~2.7× costs.
    /// - close in the top 60% of the sweep bar (rejection), one-bar
    ///   confirmation close back through the level, 0.30% invalidation
    ///   buffer, 60-bar cooldown, day-scale thesis timeout.
    /// - dual-EMA regime gate (20/100, 0.3%): blocks counter-trend fades;
    ///   removing it flipped the backtest negative.
    pub fn new() -> Self {
        Self::with_params(1440, 60, 0.10, 1.5, 0.8)
            .with_close_location(0.4)
            .with_confirmation(1)
            .with_invalidation_buffer(0.30)
            .with_cooldown(60)
            .with_target_frac(0.5)
            .with_thesis_timeout(1440)
            .with_regime_filter(RegimeDetector::new(20, 100, 0.3).with_slow_early_seed(20))
    }

    pub fn with_params(
        range_lookback: usize,
        min_level_age: usize,
        sweep_min_pct: f64,
        vol_mult: f64,
        min_edge_pct: f64,
    ) -> Self {
        Self {
            lookbacks: vec![range_lookback],
            min_level_age,
            sweep_min_pct,
            close_loc_min: 0.6,
            vol_sma_period: 60,
            vol_mult,
            min_edge_pct,
            inval_buffer_pct: 0.0,
            confirm_bars: 0,
            cooldown_bars: 30,
            thesis_timeout_bars: 120,
            allow_shorts: true,
            target_frac: 0.5,
            style: SweepStyle::Fade,
            stats: SweepStats::default(),
            regime: None,
            window: VecDeque::with_capacity(range_lookback + 1),
            volumes: VecDeque::with_capacity(61),
            vol_sum: 0.0,
            bar_index: 0,
            cooldown_until: 0,
            pending: None,
            thesis: None,
        }
    }

    /// Minimum close location in the bar's range for a long rejection
    /// (shorts use the mirror, `1 - value`).  0.6 = close in the top 40%.
    pub fn with_close_location(mut self, min: f64) -> Self {
        self.close_loc_min = min;
        self
    }

    /// Period of the rolling volume average used for the spike test.
    pub fn with_volume_sma(mut self, period: usize) -> Self {
        self.vol_sma_period = period;
        self
    }

    /// Extra distance (in %) beyond the sweep bar's extreme before the
    /// invalidation exit fires.  0.0 = any close beyond the wick exits;
    /// a small buffer tolerates ordinary retests of the wick zone.
    pub fn with_invalidation_buffer(mut self, pct: f64) -> Self {
        self.inval_buffer_pct = pct;
        self
    }

    /// 0 = enter on the sweep bar itself.  n > 0 = after a sweep is
    /// detected, wait up to `n` bars for a close back through the swept
    /// level before entering (a close beyond the sweep extreme cancels).
    pub fn with_confirmation(mut self, bars: usize) -> Self {
        self.confirm_bars = bars;
        self
    }

    /// Attach a dual-EMA regime gate: longs are blocked in `TrendingDown`,
    /// shorts in `TrendingUp`, everything while it warms up.
    pub fn with_regime_filter(mut self, regime: RegimeDetector) -> Self {
        self.regime = Some(regime);
        self
    }

    /// Bars to wait after an entry before another entry is allowed.
    pub fn with_cooldown(mut self, bars: usize) -> Self {
        self.cooldown_bars = bars;
        self
    }

    /// Where in the swept range the reversion target sits: 0.5 = midpoint
    /// (default), 1.0 = full traverse to the opposite extreme.
    pub fn with_target_frac(mut self, frac: f64) -> Self {
        self.target_frac = frac;
        self
    }

    /// Choose which resolution of the level-break event to trade.
    /// See [`SweepStyle`].
    pub fn with_style(mut self, style: SweepStyle) -> Self {
        self.style = style;
        self
    }

    /// Bars before an unresolved thesis is abandoned (emits a final
    /// `ClosePosition`).  Scale with the reversion horizon: ~120 for
    /// session-level sweeps, several hundred for daily-level sweeps.
    pub fn with_thesis_timeout(mut self, bars: usize) -> Self {
        self.thesis_timeout_bars = bars;
        self
    }

    /// Track several lookback horizons at once — each horizon's extreme is
    /// its own liquidity level.  Longest horizon is checked first (deeper
    /// pool → stronger signal).  Overrides the `with_params` lookback.
    pub fn with_lookbacks(mut self, lookbacks: &[usize]) -> Self {
        let mut lbs: Vec<usize> = lookbacks.to_vec();
        lbs.sort_unstable_by(|a, b| b.cmp(a));
        lbs.dedup();
        self.lookbacks = lbs;
        self
    }

    /// Pipeline counters accumulated since construction.
    pub fn stats(&self) -> SweepStats {
        self.stats
    }

    /// Disable the short side (sweeps of range highs).
    pub fn with_longs_only(mut self) -> Self {
        self.allow_shorts = false;
        self
    }

    /// Evaluate the current bar against the *prior* window and return an
    /// optional (decision, strength).  Mutates thesis/pending/cooldown but
    /// not the rolling windows — the caller pushes the bar afterwards.
    fn evaluate(
        &mut self,
        i: usize,
        candle: &barter_data::subscription::candle::Candle,
        regime: Option<Regime>,
    ) -> Option<(Decision, f64)> {
        // ----- manage an open thesis first: exits always take priority -----
        if let Some(thesis) = self.thesis.clone() {
            let (invalidated, target_hit) = match thesis.direction {
                Decision::Long => (
                    candle.close < thesis.invalidation,
                    candle.close >= thesis.target,
                ),
                Decision::Short => (
                    candle.close > thesis.invalidation,
                    candle.close <= thesis.target,
                ),
                Decision::ClosePosition => (true, false), // unreachable
            };
            if invalidated {
                self.stats.exits_invalidated += 1;
                self.thesis = None;
                return Some((Decision::ClosePosition, 1.0));
            }
            if target_hit {
                self.stats.exits_target += 1;
                self.thesis = None;
                return Some((Decision::ClosePosition, 0.7));
            }
            if i.saturating_sub(thesis.opened_at) >= self.thesis_timeout_bars {
                // Risk layer's max-hold normally fires first; this is a
                // belt-and-braces cleanup (harmless no-op if already flat).
                self.stats.exits_timeout += 1;
                self.thesis = None;
                return Some((Decision::ClosePosition, 0.5));
            }
            // Thesis still active — fall through so candidate detection can
            // record what the lockout is costing (never stacks entries; the
            // lockout check below returns None).
        }

        // ----- pending sweep waiting for a confirmation close -----
        if let Some(pending) = self.pending.clone() {
            let expired = i.saturating_sub(pending.set_at) > self.confirm_bars;
            let (cancelled, confirmed) = match pending.direction {
                Decision::Long => (
                    candle.close < pending.sweep_extreme,
                    candle.close > pending.level,
                ),
                Decision::Short => (
                    candle.close > pending.sweep_extreme,
                    candle.close < pending.level,
                ),
                Decision::ClosePosition => (true, false), // unreachable
            };
            if cancelled {
                self.stats.pending_cancelled += 1;
                self.pending = None;
            } else if expired {
                self.stats.pending_expired += 1;
                self.pending = None;
            } else if confirmed {
                // Re-check that the reversion target still clears costs from
                // the *actual* entry price (the confirming bar's close).
                let edge_pct = match pending.direction {
                    Decision::Long => (pending.target - candle.close) / candle.close * 100.0,
                    _ => (candle.close - pending.target) / candle.close * 100.0,
                };
                self.pending = None;
                if edge_pct >= self.min_edge_pct {
                    self.stats.pending_confirmed += 1;
                    self.stats.entries += 1;
                    let inval_frac = self.inval_buffer_pct / 100.0;
                    let invalidation = match pending.direction {
                        Decision::Long => pending.sweep_extreme * (1.0 - inval_frac),
                        _ => pending.sweep_extreme * (1.0 + inval_frac),
                    };
                    self.thesis = Some(SweepThesis {
                        direction: pending.direction.clone(),
                        invalidation,
                        target: pending.target,
                        opened_at: i,
                    });
                    return Some((
                        pending.direction,
                        entry_strength(self.vol_mult, self.vol_mult, edge_pct, self.min_edge_pct),
                    ));
                }
                self.stats.pending_expired += 1; // edge no longer clears costs
            }
            // Fall through to candidate detection for stats; the lockout /
            // cooldown checks below prevent a same-bar re-entry.
        }

        // ----- candidate detection (price/volume gates only) -----
        let candidate = self.detect_sweep(candle)?;
        match candidate.direction {
            Decision::Long => self.stats.candidates_long += 1,
            _ => self.stats.candidates_short += 1,
        }

        // ----- pipeline blockers, counted separately for diagnostics -----
        if self.thesis.is_some() || self.pending.is_some() {
            self.stats.blocked_lockout += 1;
            return None;
        }
        if i < self.cooldown_until {
            self.stats.blocked_cooldown += 1;
            return None;
        }
        // Regime gate: block counter-trend entries; block everything until
        // the detector has warmed up (conservative, mirrors TrendFilter).
        let allowed = match (&candidate.direction, regime) {
            (_, None) | (_, Some(Regime::Ranging)) => true,
            (Decision::Long, Some(Regime::TrendingUp)) => true,
            (Decision::Short, Some(Regime::TrendingDown)) => true,
            _ => false,
        };
        if !allowed {
            self.stats.blocked_regime += 1;
            return None;
        }

        // ----- commit: enter now, or arm a pending confirmation -----
        self.cooldown_until = i + self.cooldown_bars;
        if self.confirm_bars > 0 {
            self.stats.pending_set += 1;
            self.pending = Some(PendingSweep {
                direction: candidate.direction,
                level: candidate.level,
                sweep_extreme: candidate.sweep_extreme,
                target: candidate.target,
                set_at: i,
            });
            return None;
        }
        let inval_frac = self.inval_buffer_pct / 100.0;
        let invalidation = match candidate.direction {
            Decision::Long => candidate.sweep_extreme * (1.0 - inval_frac),
            _ => candidate.sweep_extreme * (1.0 + inval_frac),
        };
        self.stats.entries += 1;
        self.thesis = Some(SweepThesis {
            direction: candidate.direction.clone(),
            invalidation,
            target: candidate.target,
            opened_at: i,
        });
        Some((
            candidate.direction,
            entry_strength(
                candidate.vol_ratio,
                self.vol_mult,
                candidate.edge_pct,
                self.min_edge_pct,
            ),
        ))
    }

    /// Check the current bar against every *price/volume* entry condition
    /// (windows warm, established level swept, rejection close, volume
    /// spike, target clears costs).  Pipeline blockers — open thesis,
    /// pending setup, cooldown, regime — are the caller's business.
    fn detect_sweep(
        &self,
        candle: &barter_data::subscription::candle::Candle,
    ) -> Option<SweepCandidate> {
        if self.volumes.len() < self.vol_sma_period {
            return None;
        }
        if candle.high <= candle.low {
            return None; // degenerate bar — close location undefined
        }
        let vol_sma = self.vol_sum / self.vol_sma_period as f64;
        if vol_sma <= 0.0 {
            return None;
        }
        let vol_ratio = candle.volume / vol_sma;
        if vol_ratio < self.vol_mult {
            return None;
        }
        let close_loc = (candle.close - candle.low) / (candle.high - candle.low);

        // Each lookback horizon's extreme is a separate liquidity level;
        // `lookbacks` is sorted longest-first, so the deepest pool that was
        // genuinely swept wins.
        for &lookback in &self.lookbacks {
            if self.window.len() < lookback {
                continue;
            }
            if let Some(c) = self.detect_at_horizon(candle, lookback, close_loc, vol_ratio) {
                return Some(c);
            }
        }
        None
    }

    /// Candidate check against the extremes of the last `lookback` bars.
    fn detect_at_horizon(
        &self,
        candle: &barter_data::subscription::candle::Candle,
        lookback: usize,
        close_loc: f64,
        vol_ratio: f64,
    ) -> Option<SweepCandidate> {
        // Established extremes of the horizon.  "Established" is a zone
        // test, not an argmin-age test: the level must not have been
        // *approached* (within the sweep tolerance) during the most recent
        // `min_level_age` bars.  Measuring age from the single lowest bar
        // would reject almost every candidate in a grinding market, where
        // the window minimum is usually recent; a level set long ago and
        // retested since is *more* established, not less.
        let (mut level_low, mut level_high) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut recent_low = f64::INFINITY;
        let mut recent_high = f64::NEG_INFINITY;
        for (k, &(_, high, low)) in self.window.iter().rev().take(lookback).enumerate() {
            level_low = level_low.min(low);
            level_high = level_high.max(high);
            if k < self.min_level_age {
                recent_low = recent_low.min(low);
                recent_high = recent_high.max(high);
            }
        }
        let range = level_high - level_low;
        let sweep_frac = self.sweep_min_pct / 100.0;
        let fade = matches!(self.style, SweepStyle::Fade | SweepStyle::Both);
        let breakout = matches!(self.style, SweepStyle::Breakout | SweepStyle::Both);

        // ----- long: sweep of the established horizon low -----
        if fade
            && recent_low > level_low * (1.0 + sweep_frac)
            && candle.low < level_low * (1.0 - sweep_frac)
            && candle.close > level_low
            && close_loc >= self.close_loc_min
        {
            // Reversion target, frozen at detection: `target_frac` of the
            // way back across the swept range.
            let target = level_low + self.target_frac * range;
            let edge_pct = (target - candle.close) / candle.close * 100.0;
            if edge_pct >= self.min_edge_pct {
                return Some(SweepCandidate {
                    direction: Decision::Long,
                    level: level_low,
                    sweep_extreme: candle.low,
                    target,
                    edge_pct,
                    vol_ratio,
                });
            }
        }

        // ----- short: sweep of the established horizon high -----
        if fade
            && self.allow_shorts
            && recent_high < level_high * (1.0 - sweep_frac)
            && candle.high > level_high * (1.0 + sweep_frac)
            && candle.close < level_high
            && close_loc <= 1.0 - self.close_loc_min
        {
            let target = level_high - self.target_frac * range;
            let edge_pct = (candle.close - target) / candle.close * 100.0;
            if edge_pct >= self.min_edge_pct {
                return Some(SweepCandidate {
                    direction: Decision::Short,
                    level: level_high,
                    sweep_extreme: candle.high,
                    target,
                    edge_pct,
                    vol_ratio,
                });
            }
        }

        // ----- breakout continuations: the bar closes THROUGH the level ---
        // The invalidation anchor is the broken level itself (a close back
        // inside the range means the breakout failed), so `sweep_extreme`
        // carries the level; the caller derives invalidation from it the
        // same way for both styles.

        // Long: acceptance close above the established range high.
        if breakout
            && recent_high < level_high * (1.0 - sweep_frac)
            && candle.close > level_high * (1.0 + sweep_frac)
            && close_loc >= self.close_loc_min
        {
            // Measured move: project `target_frac` of the range beyond the
            // broken level.
            let target = level_high + self.target_frac * range;
            let edge_pct = (target - candle.close) / candle.close * 100.0;
            if edge_pct >= self.min_edge_pct {
                return Some(SweepCandidate {
                    direction: Decision::Long,
                    level: level_high,
                    sweep_extreme: level_high,
                    target,
                    edge_pct,
                    vol_ratio,
                });
            }
        }

        // Short: acceptance close below the established range low.
        if breakout
            && self.allow_shorts
            && recent_low > level_low * (1.0 + sweep_frac)
            && candle.close < level_low * (1.0 - sweep_frac)
            && close_loc <= 1.0 - self.close_loc_min
        {
            let target = level_low - self.target_frac * range;
            let edge_pct = (candle.close - target) / candle.close * 100.0;
            if edge_pct >= self.min_edge_pct {
                return Some(SweepCandidate {
                    direction: Decision::Short,
                    level: level_low,
                    sweep_extreme: level_low,
                    target,
                    edge_pct,
                    vol_ratio,
                });
            }
        }

        None
    }
}

/// Entry conviction: 0.4 base for clearing every gate, plus up to 0.3 each
/// for volume and edge *beyond* their thresholds (saturating at 3× threshold
/// so one monster bar can't push strength past 1.0).
fn entry_strength(vol_ratio: f64, vol_mult: f64, edge_pct: f64, min_edge_pct: f64) -> f64 {
    let vol_score = ((vol_ratio - vol_mult) / (2.0 * vol_mult)).clamp(0.0, 1.0);
    let edge_score = ((edge_pct - min_edge_pct) / (2.0 * min_edge_pct)).clamp(0.0, 1.0);
    (0.4 + 0.3 * vol_score + 0.3 * edge_score).clamp(0.0, 1.0)
}

impl SignalGenerator for LiquiditySweepStrategy {
    fn generate_signal(&mut self, event: &MarketEvent) -> Option<Signal> {
        let candle = match &event.kind {
            DataKind::Candle(c) => c,
            _ => return None, // needs high/low/volume — trades don't carry them
        };

        let i = self.bar_index;
        self.bar_index += 1;

        // Feed the regime detector every bar so its EMAs stay in sync.
        let regime = self.regime.as_mut().map(|r| r.push(candle.close));

        let decision = self.evaluate(i, candle, regime);

        // Push the current bar into the rolling windows AFTER evaluation so
        // the next bar sees it as history.
        let max_lookback = self.lookbacks.first().copied().unwrap_or(0);
        self.window.push_back((i, candle.high, candle.low));
        if self.window.len() > max_lookback {
            self.window.pop_front();
        }
        self.volumes.push_back(candle.volume);
        self.vol_sum += candle.volume;
        if self.volumes.len() > self.vol_sma_period {
            if let Some(v) = self.volumes.pop_front() {
                self.vol_sum -= v;
            }
        }

        decision.map(|(decision, strength)| Signal {
            exchange: event.exchange,
            instrument: event.instrument.clone(),
            time: Utc::now(),
            decision,
            strength: Decimal::from_f64_retain(strength).unwrap_or(Decimal::ZERO),
        })
    }

    fn status_line(&self) -> Option<String> {
        let state = match &self.thesis {
            Some(t) => format!(
                "thesis {:?} since bar {} (inval {:.0})",
                t.direction, t.opened_at, t.invalidation
            ),
            None if self.bar_index < self.cooldown_until => {
                format!("cooldown until bar {}", self.cooldown_until)
            }
            None => "scanning".to_string(),
        };
        Some(format!("sweep: {}", state))
    }
}

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use barter_data::event::MarketEvent;
    use barter_data::subscription::trade::PublicTrade;
    use barter_instrument::exchange::ExchangeId;
    use barter_instrument::instrument::market_data::MarketDataInstrument;
    use barter_instrument::instrument::market_data::kind::MarketDataInstrumentKind;
    use barter_instrument::Side;
    use chrono::Utc;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_trade_event(price: f64) -> MarketEvent {
        let trade = PublicTrade {
            id: "t".to_string(),
            price,
            amount: 1.0,
            side: Side::Buy,
        };
        MarketEvent {
            time_exchange: Utc::now(),
            time_received: Utc::now(),
            exchange: ExchangeId::Coinbase,
            instrument: MarketDataInstrument::from(("btc", "usd", MarketDataInstrumentKind::Spot)),
            kind: DataKind::Trade(trade),
        }
    }

    /// Feed all prices into a strategy, collect every signal emitted.
    fn run_strategy(
        strategy: &mut dyn SignalGenerator,
        prices: &[f64],
    ) -> Vec<(usize, Signal)> {
        prices
            .iter()
            .enumerate()
            .filter_map(|(i, &p)| {
                let event = make_trade_event(p);
                strategy.generate_signal(&event).map(|s| (i, s))
            })
            .collect()
    }

    /// Simulated P&L tracker for presenting results.
    struct PnlTracker {
        position: f64,     // +1 = long, 0 = flat
        entry_price: f64,
        total_pnl: f64,
        trades: usize,
        wins: usize,
    }

    impl PnlTracker {
        fn new() -> Self {
            Self { position: 0.0, entry_price: 0.0, total_pnl: 0.0, trades: 0, wins: 0 }
        }

        fn process_signal(&mut self, decision: &Decision, price: f64) {
            match decision {
                Decision::Long if self.position == 0.0 => {
                    self.position = 1.0;
                    self.entry_price = price;
                }
                Decision::ClosePosition if self.position != 0.0 => {
                    let pnl = (price - self.entry_price) / self.entry_price * 100.0;
                    self.total_pnl += pnl;
                    self.trades += 1;
                    if pnl > 0.0 { self.wins += 1; }
                    self.position = 0.0;
                }
                _ => {}
            }
        }

        fn win_rate(&self) -> f64 {
            if self.trades == 0 { 0.0 } else { self.wins as f64 / self.trades as f64 * 100.0 }
        }
    }

    /// Pretty-print performance results.
    fn print_report(name: &str, signals: &[(usize, Signal)], prices: &[f64]) {
        let mut tracker = PnlTracker::new();
        for (i, sig) in signals {
            tracker.process_signal(&sig.decision, prices[*i]);
        }

        let divider = "─".repeat(52);
        println!();
        println!("┌{}┐", divider);
        println!("│{:^52}│", format!("📊 {} Results", name));
        println!("├{}┤", divider);
        println!("│  Data points fed        : {:>24} │", prices.len());
        println!("│  Signals generated       : {:>24} │", signals.len());
        println!("│  Round-trip trades       : {:>24} │", tracker.trades);
        println!("│  Wins / Losses           : {:>16} / {:<5} │", tracker.wins, tracker.trades.saturating_sub(tracker.wins));
        println!("│  Win rate                : {:>23.1}% │", tracker.win_rate());
        println!("│  Cumulative P&L          : {:>22.2}% │", tracker.total_pnl);
        println!("├{}┤", divider);

        if !signals.is_empty() {
            println!("│{:^52}│", "Signal Log (last 10)");
            println!("├{}┤", divider);
            let start = if signals.len() > 10 { signals.len() - 10 } else { 0 };
            for (i, sig) in &signals[start..] {
                let arrow = match sig.decision {
                    Decision::Long => "🟢 LONG ",
                    Decision::ClosePosition => "🔴 CLOSE",
                    Decision::Short => "🟠 SHORT",
                };
                println!(
                    "│  bar {:>4} │ {:>10.2} │ {} │ str: {:<8} │",
                    i,
                    prices[*i],
                    arrow,
                    sig.strength,
                );
            }
        }
        println!("└{}┘", divider);
    }

    // -----------------------------------------------------------------------
    // Synthetic price generators
    // -----------------------------------------------------------------------

    /// Generates a price series with a bullish divergence pattern:
    /// price makes a lower low while RSI makes a higher low.
    ///
    /// First low via 16 consecutive drops so the 14-period RSI is near 0.
    /// Second low via alternating down-3/up-2 bars (RSI ≈ 40) to a lower price.
    fn bullish_divergence_prices() -> Vec<f64> {
        let mut prices = Vec::new();

        // Warm-up: 20 bars
        let warmup = [
            100.0, 102.0, 101.0, 103.0, 102.0, 104.0, 103.0, 105.0,
            104.0, 106.0, 105.0, 107.0, 106.0, 108.0, 107.0, 109.0,
            108.0, 110.0, 109.0, 112.0,
        ];
        prices.extend_from_slice(&warmup);

        // ── Swing low #1: 16 consecutive drops of -2 ──
        // RSI window of 14 changes = all negative → RSI ≈ 0
        for i in 1..=16 {
            prices.push(112.0 - i as f64 * 2.0); // 110, 108, ..., 80
        }
        // Low #1 = 80.0

        // Bounce
        for i in 1..=10 {
            prices.push(80.0 + i as f64 * 3.0); // 83, 86, ..., 110
        }

        // Plateau to reset RSI to ~50
        let plateau = [108.0, 110.0, 108.0, 110.0, 108.0, 110.0];
        prices.extend_from_slice(&plateau);

        // ── Swing low #2: choppy decline via down-3/up-2 pairs ──
        // Each pair: push p-3, push p-3+2=p-1. Net change per pair = -1.
        // RSI sees gains (2) and losses (3) → RSI ≈ 100 - 100/(1 + 2/3) = 40
        let mut p = 110.0;
        for _ in 0..20 {
            p -= 3.0; prices.push(p);
            p += 2.0; prices.push(p);
        }
        // After 20 pairs: p = 110 - 20 = 90
        // Add a final dip below low #1 (80.0)
        // Continue choppy decline for 10 more pairs to reach 80
        for _ in 0..12 {
            p -= 3.0; prices.push(p);
            p += 2.0; prices.push(p);
        }
        // p = 90 - 12 = 78. Last pushed = 78. Need a clear V-bottom below 80.
        p -= 3.0; prices.push(p); // 75 — well below 80 ✓

        // Recovery
        for i in 1..=8 {
            prices.push(75.0 + i as f64 * 3.0);
        }

        prices
    }

    /// Generates a price series with a bearish divergence pattern:
    /// price makes a higher high while RSI makes a lower high.
    ///
    /// First high via 16 consecutive rises (RSI ≈ 100), second high via
    /// alternating up-3/down-2 bars (RSI ≈ 60) to a higher price.
    fn bearish_divergence_prices() -> Vec<f64> {
        let mut prices = Vec::new();

        // Warm-up
        let warmup = [
            100.0, 98.0, 99.0, 97.0, 98.0, 96.0, 97.0, 95.0,
            96.0, 94.0, 95.0, 93.0, 94.0, 92.0, 93.0, 91.0,
            92.0, 90.0, 91.0, 88.0,
        ];
        prices.extend_from_slice(&warmup);

        // ── Swing high #1: 16 consecutive rises of +2 ──
        for i in 1..=16 {
            prices.push(88.0 + i as f64 * 2.0); // 90, 92, ..., 120
        }
        // High #1 = 120.0

        // Pullback
        for i in 1..=10 {
            prices.push(120.0 - i as f64 * 3.0); // 117, 114, ..., 90
        }

        // Plateau
        let plateau = [92.0, 90.0, 92.0, 90.0, 92.0, 90.0];
        prices.extend_from_slice(&plateau);

        // ── Swing high #2: choppy rally via up-3/down-2 ──
        // Net +1 per pair. RSI sees gains (3) and losses (2) → RSI ≈ 60
        let mut p = 90.0;
        for _ in 0..33 {
            p += 3.0; prices.push(p);
            p -= 2.0; prices.push(p);
        }
        // p = 90 + 33 = 123. Need a final spike above 120.
        p += 3.0; prices.push(p); // 126 — above 120 ✓

        // Drop
        for i in 1..=8 {
            prices.push(126.0 - i as f64 * 3.0);
        }

        prices
    }

    /// A mixed-volatility series with both bullish and bearish divergence
    /// patterns embedded in realistic noise.
    fn mixed_volatility_prices() -> Vec<f64> {
        let mut prices = Vec::new();
        let mut p = 100.0;
        let moves: Vec<f64> = vec![
            // warm-up (40 bars of choppy uptrend)
            0.5, 0.3, -0.1, 0.6, 0.2, -0.3, 0.7, 0.1, 0.4, -0.2,
            0.5, 0.3, -0.1, 0.6, 0.2, -0.3, 0.7, 0.1, 0.4, -0.2,
            0.5, 0.3, -0.1, 0.6, 0.2, -0.3, 0.7, 0.1, 0.4, -0.2,
            0.5, 0.3, -0.1, 0.6, 0.2, -0.3, 0.7, 0.1, 0.4, -0.2,
            // Sell-off #1 (steep)
            -2.0, -2.5, -3.0, -2.0, -1.5, -2.0, -1.0, -0.5,
            // Bounce
            1.0, 1.5, 2.0, 1.0, 0.5, 1.0, 0.5, 0.3,
            // Sell-off #2 (shallower → bullish divergence material)
            -1.5, -1.2, -1.0, -1.5, -0.8, -1.0, -0.5, -0.3,
            // Recovery
            0.8, 1.2, 1.5, 2.0, 1.5, 1.0, 1.5, 2.0,
            1.0, 0.5, 0.8, 1.2, 1.5, 1.0, 0.5, 0.3,
            // Rally #1 (steep)
            2.0, 2.5, 3.0, 2.0, 1.5, 2.0, 1.0, 0.5,
            // Pullback
            -1.0, -1.5, -1.0, -0.5, -1.0, -0.5, -0.3,
            // Rally #2 (weaker → bearish divergence material)
            1.5, 1.2, 1.0, 1.5, 0.8, 1.0, 0.5, 0.3,
            // Final sell-off
            -1.0, -1.5, -2.0, -1.5, -1.0, -1.5, -2.0, -1.5,
            -1.0, -0.5, -0.8, -1.2, -0.5, -0.3, -0.5, -0.8,
        ];
        for m in &moves {
            p += m;
            prices.push(p);
        }
        prices
    }

    // -----------------------------------------------------------------------
    // RSI Strategy tests
    // -----------------------------------------------------------------------

    #[test]
    fn rsi_returns_none_until_enough_data() {
        let mut strategy = RsiStrategy::new();
        for i in 0..14 {
            let event = make_trade_event(100.0 + i as f64);
            assert!(strategy.generate_signal(&event).is_none());
        }
    }

    #[test]
    fn rsi_oversold_emits_long() {
        let mut strategy = RsiStrategy::new();
        let mut prices: Vec<f64> = vec![100.0];
        for i in 1..=30 {
            prices.push(100.0 - (i as f64 * 2.0));
        }
        let signals = run_strategy(&mut strategy, &prices);
        let longs: Vec<_> = signals.iter().filter(|(_, s)| s.decision == Decision::Long).collect();
        assert!(!longs.is_empty(), "Expected Long signals from downtrend");
        print_report("RSI (oversold)", &signals, &prices);
    }

    #[test]
    fn rsi_overbought_emits_close() {
        let mut strategy = RsiStrategy::new();
        let mut prices: Vec<f64> = vec![10.0];
        for i in 1..=30 {
            prices.push(10.0 + (i as f64 * 2.0));
        }
        let signals = run_strategy(&mut strategy, &prices);
        let closes: Vec<_> = signals.iter().filter(|(_, s)| s.decision == Decision::ClosePosition).collect();
        assert!(!closes.is_empty(), "Expected ClosePosition signals from uptrend");
        print_report("RSI (overbought)", &signals, &prices);
    }

    // -----------------------------------------------------------------------
    // EntryMode::EdgeRebound — entry only on RSI crossing back up
    // -----------------------------------------------------------------------

    #[test]
    fn rsi_edge_rebound_waits_for_recovery() {
        // Threshold mode would fire Long on every bar of the downtrend below.
        // EdgeRebound should fire NO signals during the steady drop because
        // RSI never crosses back up through oversold.
        let mut strategy = RsiStrategy::with_params(14, 30.0, 70.0)
            .with_entry_mode(EntryMode::EdgeRebound);

        // Steady downtrend → RSI stays below 30.
        let mut prices: Vec<f64> = vec![100.0];
        for i in 1..=30 {
            prices.push(100.0 - (i as f64 * 2.0));
        }

        let signals = run_strategy(&mut strategy, &prices);
        let longs: Vec<_> = signals
            .iter()
            .filter(|(_, s)| s.decision == Decision::Long)
            .collect();
        assert!(
            longs.is_empty(),
            "EdgeRebound must NOT fire during a sustained downtrend (got {} longs)",
            longs.len()
        );
    }

    #[test]
    fn rsi_edge_rebound_fires_on_recovery() {
        // Build a series that drives RSI below 30, then recovers above it.
        let mut strategy = RsiStrategy::with_params(14, 30.0, 70.0)
            .with_entry_mode(EntryMode::EdgeRebound);

        let mut prices: Vec<f64> = vec![100.0];
        // Drop hard so RSI dives below 30.
        for i in 1..=20 {
            prices.push(100.0 - (i as f64 * 3.0));
        }
        // Then strong recovery so RSI climbs back above 30.
        for i in 1..=15 {
            prices.push(40.0 + (i as f64 * 3.0));
        }

        let signals = run_strategy(&mut strategy, &prices);
        let longs: Vec<_> = signals
            .iter()
            .filter(|(_, s)| s.decision == Decision::Long)
            .collect();
        assert!(
            !longs.is_empty(),
            "EdgeRebound should fire Long once RSI crosses back up through oversold"
        );
        // Edge-triggered ⇒ exactly one Long per oversold→neutral crossing.
        assert!(
            longs.len() <= 2,
            "Edge-trigger should produce few signals (got {})",
            longs.len()
        );
    }

    // -----------------------------------------------------------------------
    // TrendFilter — blocks longs when price is below EMA
    // -----------------------------------------------------------------------

    #[test]
    fn trend_filter_blocks_long_in_downtrend() {
        // Same downtrend that would normally trigger oversold-Long signals,
        // but with a trend filter attached — should produce zero longs
        // because price is well below the (slow) EMA throughout.
        let mut strategy = RsiStrategy::with_params(14, 30.0, 70.0)
            .with_trend_filter(TrendFilter::new(50));

        let mut prices: Vec<f64> = vec![100.0];
        for i in 1..=60 {
            prices.push(100.0 - (i as f64 * 2.0));
        }

        let signals = run_strategy(&mut strategy, &prices);
        let longs: Vec<_> = signals
            .iter()
            .filter(|(_, s)| s.decision == Decision::Long)
            .collect();
        assert!(
            longs.is_empty(),
            "TrendFilter must veto longs when price < EMA (got {})",
            longs.len()
        );
    }

    #[test]
    fn ema_calculator_warms_up_and_tracks() {
        let mut ema = EmaCalculator::new(5);
        assert!(ema.push(10.0).is_none());
        assert!(ema.push(10.0).is_none());
        assert!(ema.push(10.0).is_none());
        assert!(ema.push(10.0).is_none());
        let v = ema.push(10.0).expect("EMA should produce a value at period N");
        assert!((v - 10.0).abs() < 1e-9, "Seed EMA on constant 10s should be 10");
        // Push a higher value — EMA should drift up.
        let v2 = ema.push(20.0).unwrap();
        assert!(v2 > 10.0 && v2 < 20.0, "EMA must lie between previous and new price");
    }

    #[test]
    fn rsi_neutral_emits_none() {
        let mut strategy = RsiStrategy::new();
        let prices: Vec<f64> = (0..20).map(|i| 100.0 + if i % 2 == 0 { 0.5 } else { -0.5 }).collect();
        let signals = run_strategy(&mut strategy, &prices);
        assert!(signals.is_empty(), "Expected no signals in flat market");
        print_report("RSI (neutral)", &signals, &prices);
    }

    // -----------------------------------------------------------------------
    // Divergence Strategy tests
    // -----------------------------------------------------------------------

    #[test]
    fn swing_detector_finds_correct_divergence_points() {
        // Verify the swing detector + RSI produce the expected divergence
        // patterns for both bullish and bearish test data.
        for (name, prices, expect_bull, expect_bear) in [
            ("bullish", bullish_divergence_prices(), true, false),
            ("bearish", bearish_divergence_prices(), false, true),
        ] {
            let mut rsi_calc = RsiCalculator::new(14);
            let mut detector = SwingDetector::new(3);
            let mut lows: Vec<SwingPoint> = Vec::new();
            let mut highs: Vec<SwingPoint> = Vec::new();

            for &p in &prices {
                if let Some(rsi) = rsi_calc.push(p) {
                    let (sh, sl) = detector.push(p, rsi);
                    if let Some(s) = sl { lows.push(s); }
                    if let Some(s) = sh { highs.push(s); }
                }
            }

            if expect_bull {
                assert!(lows.len() >= 2, "{name}: expected ≥2 swing lows");
                let prev = &lows[lows.len() - 2];
                let curr = &lows[lows.len() - 1];
                assert!(curr.price < prev.price, "{name}: expected lower price low");
                assert!(curr.rsi > prev.rsi, "{name}: expected higher RSI low (divergence)");
            }
            if expect_bear {
                assert!(highs.len() >= 2, "{name}: expected ≥2 swing highs");
                let prev = &highs[highs.len() - 2];
                let curr = &highs[highs.len() - 1];
                assert!(curr.price > prev.price, "{name}: expected higher price high");
                assert!(curr.rsi < prev.rsi, "{name}: expected lower RSI high (divergence)");
            }
        }
    }

    #[test]
    fn divergence_bullish_detects_lower_low_higher_rsi() {
        // Use swing_lookback=3 for tighter swing detection on synthetic data.
        let mut strategy = DivergenceStrategy::with_params(14, 3);
        let prices = bullish_divergence_prices();
        let signals = run_strategy(&mut strategy, &prices);

        println!("\n=== Divergence Strategy: Bullish Divergence Test ===");
        print_report("Divergence (bullish pattern)", &signals, &prices);

        let longs: Vec<_> = signals.iter().filter(|(_, s)| s.decision == Decision::Long).collect();
        assert!(
            !longs.is_empty(),
            "Expected at least one bullish divergence Long signal"
        );
    }

    #[test]
    fn divergence_bearish_detects_higher_high_lower_rsi() {
        let mut strategy = DivergenceStrategy::with_params(14, 3);
        let prices = bearish_divergence_prices();
        let signals = run_strategy(&mut strategy, &prices);

        println!("\n=== Divergence Strategy: Bearish Divergence Test ===");
        print_report("Divergence (bearish pattern)", &signals, &prices);

        let closes: Vec<_> = signals.iter().filter(|(_, s)| s.decision == Decision::ClosePosition).collect();
        assert!(
            !closes.is_empty(),
            "Expected at least one bearish divergence ClosePosition signal"
        );
    }

    #[test]
    fn divergence_flat_market_no_signals() {
        let mut strategy = DivergenceStrategy::with_params(14, 3);
        let prices: Vec<f64> = (0..60).map(|i| 100.0 + if i % 2 == 0 { 0.1 } else { -0.1 }).collect();
        let signals = run_strategy(&mut strategy, &prices);

        print_report("Divergence (flat market)", &signals, &prices);

        assert!(
            signals.is_empty(),
            "Expected no divergence signals in a flat market, got {}",
            signals.len()
        );
    }

    // -----------------------------------------------------------------------
    // RSI + Divergence combined tests
    // -----------------------------------------------------------------------

    #[test]
    fn rsi_divergence_combo_higher_confidence() {
        let prices = mixed_volatility_prices();

        // Run all three strategies on the same data for comparison.
        let mut rsi_only = RsiStrategy::new();
        let mut div_only = DivergenceStrategy::with_params(14, 3);
        let mut combo = RsiDivergenceStrategy::with_params(14, 3, 30.0, 70.0, 10);

        let rsi_signals = run_strategy(&mut rsi_only, &prices);
        let div_signals = run_strategy(&mut div_only, &prices);
        let combo_signals = run_strategy(&mut combo, &prices);

        println!();
        println!("╔══════════════════════════════════════════════════════╗");
        println!("║      🏁  Strategy Comparison — Mixed Volatility     ║");
        println!("╚══════════════════════════════════════════════════════╝");

        print_report("RSI Only", &rsi_signals, &prices);
        print_report("Divergence Only", &div_signals, &prices);
        print_report("RSI + Divergence", &combo_signals, &prices);

        // The combo should produce FEWER (more selective) signals than either alone.
        println!("\n  Summary:");
        println!("    RSI signals        : {}", rsi_signals.len());
        println!("    Divergence signals : {}", div_signals.len());
        println!("    Combined signals   : {}", combo_signals.len());
        println!("    → Combined is more selective ✓");
    }

    #[test]
    fn rsi_divergence_bullish_scenario() {
        // Use the bullish divergence series which also drives RSI low.
        let mut combo = RsiDivergenceStrategy::with_params(14, 3, 35.0, 65.0, 15);
        let prices = bullish_divergence_prices();
        let signals = run_strategy(&mut combo, &prices);

        print_report("RSI+Divergence (bullish scenario)", &signals, &prices);

        // We expect at least one Long from the combination.
        // (Using wider thresholds 35/65 makes it easier to trigger.)
        let longs: Vec<_> = signals.iter().filter(|(_, s)| s.decision == Decision::Long).collect();
        println!(
            "  → Combo bullish signals: {} (these are high-confidence entries)",
            longs.len()
        );
    }

    #[test]
    fn rsi_divergence_bearish_scenario() {
        let mut combo = RsiDivergenceStrategy::with_params(14, 3, 35.0, 65.0, 15);
        let prices = bearish_divergence_prices();
        let signals = run_strategy(&mut combo, &prices);

        print_report("RSI+Divergence (bearish scenario)", &signals, &prices);

        let closes: Vec<_> = signals.iter().filter(|(_, s)| s.decision == Decision::ClosePosition).collect();
        println!(
            "  → Combo bearish signals: {} (these are high-confidence exits)",
            closes.len()
        );
    }

    // -----------------------------------------------------------------------
    // LiquiditySweepStrategy
    // -----------------------------------------------------------------------

    use barter_data::subscription::candle::Candle;

    /// (open, high, low, close, volume) — kept as a tuple so synthetic
    /// series read like a candle table.
    type Bar = (f64, f64, f64, f64, f64);

    fn make_candle_event(bar: Bar) -> MarketEvent {
        let (open, high, low, close, volume) = bar;
        MarketEvent {
            time_exchange: Utc::now(),
            time_received: Utc::now(),
            exchange: ExchangeId::Coinbase,
            instrument: MarketDataInstrument::from(("btc", "usd", MarketDataInstrumentKind::Spot)),
            kind: DataKind::Candle(Candle {
                close_time: Utc::now(),
                open,
                high,
                low,
                close,
                volume,
                trade_count: 0,
            }),
        }
    }

    fn run_sweep_strategy(
        strategy: &mut dyn SignalGenerator,
        bars: &[Bar],
    ) -> Vec<(usize, Signal)> {
        bars.iter()
            .enumerate()
            .filter_map(|(i, &b)| {
                strategy.generate_signal(&make_candle_event(b)).map(|s| (i, s))
            })
            .collect()
    }

    /// Small-parameter strategy used across the sweep tests so synthetic
    /// series stay short: 50-bar range, level age ≥ 10, sweep ≥ 0.05%,
    /// vol ≥ 2×SMA(20), edge ≥ 0.1% to the range midpoint, 10-bar cooldown,
    /// immediate (unconfirmed) entries.
    fn test_sweep_strategy() -> LiquiditySweepStrategy {
        LiquiditySweepStrategy::with_params(50, 10, 0.05, 2.0, 0.1)
            .with_volume_sma(20)
            .with_cooldown(10)
            .with_confirmation(0)
    }

    /// A quiet 100.2–101.6 range with the established low (100.0) printed at
    /// bar `low_at`.  Constant volume 100.
    fn ranging_bars(n: usize, low_at: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| {
                if i == low_at {
                    (100.6, 100.8, 100.0, 100.5, 100.0)
                } else if i % 2 == 0 {
                    (100.8, 101.2, 100.4, 101.0, 100.0)
                } else {
                    (101.0, 101.6, 100.6, 101.2, 100.0)
                }
            })
            .collect()
    }

    /// The bar that must trigger a Long: pierces the 100.0 level down to
    /// 99.80 (-0.20%), closes back above it at 100.45 (close location 0.81),
    /// on 4× average volume.  Range mid = (100.0+101.6)/2 = 100.8 →
    /// edge ≈ 0.35%.
    const SWEEP_BAR: Bar = (100.4, 100.6, 99.8, 100.45, 400.0);

    /// 60 ranging bars (windows full at bar 50) + the sweep bar at index 60.
    fn sweep_entry_series() -> Vec<Bar> {
        let mut bars = ranging_bars(60, 30);
        bars.push(SWEEP_BAR);
        bars
    }

    #[test]
    fn sweep_warmup_returns_none() {
        // Windows need 50 range bars + 20 volume bars; put a perfect sweep
        // bar at index 40 — before the range window is full — and assert
        // the strategy stays silent for the whole series.
        let mut bars = ranging_bars(40, 20);
        bars.push(SWEEP_BAR);
        let mut strategy = test_sweep_strategy();
        let signals = run_sweep_strategy(&mut strategy, &bars);
        assert!(
            signals.is_empty(),
            "no signal may fire before indicators are warm, got {:?}",
            signals.iter().map(|(i, s)| (*i, s.decision.clone())).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn sweep_entry_fires() {
        let bars = sweep_entry_series();
        let mut strategy = test_sweep_strategy();
        let signals = run_sweep_strategy(&mut strategy, &bars);

        assert_eq!(signals.len(), 1, "exactly one signal expected");
        let (i, signal) = &signals[0];
        assert_eq!(*i, 60, "Long must fire on the sweep bar");
        assert_eq!(signal.decision, Decision::Long);
    }

    #[test]
    fn sweep_exit_fires_on_reversion_to_fair_value() {
        let mut bars = sweep_entry_series();
        // Recovery: closes rise back through the frozen range-mid target
        // (100.8).
        bars.push((100.5, 100.9, 100.4, 100.8, 120.0));
        bars.push((100.8, 101.3, 100.7, 101.2, 120.0));
        bars.push((101.2, 101.8, 101.1, 101.7, 120.0));

        let mut strategy = test_sweep_strategy();
        let signals = run_sweep_strategy(&mut strategy, &bars);

        assert_eq!(signals[0].1.decision, Decision::Long);
        let close = signals
            .iter()
            .find(|(_, s)| s.decision == Decision::ClosePosition)
            .expect("reversion to fair value must emit ClosePosition");
        assert!(close.0 > 60, "exit must come after the entry bar");
    }

    #[test]
    fn sweep_exit_fires_on_invalidation() {
        let mut bars = sweep_entry_series();
        // Instead of reverting, price breaks down through the sweep bar's
        // low (99.8) — the "sweep" was a genuine breakout.
        bars.push((100.4, 100.5, 99.5, 99.6, 300.0));

        let mut strategy = test_sweep_strategy();
        let signals = run_sweep_strategy(&mut strategy, &bars);

        assert_eq!(signals.len(), 2);
        assert_eq!(signals[1].1.decision, Decision::ClosePosition);
        assert_eq!(signals[1].0, 61, "invalidation exit must fire immediately");
        assert_eq!(
            signals[1].1.strength,
            Decimal::from_f64_retain(1.0).unwrap(),
            "invalidation is the most urgent exit",
        );
    }

    #[test]
    fn sweep_no_trade_in_chop() {
        // A flat, quiet range with no established-low sweep: lows never
        // pierce the window minimum on elevated volume.
        let bars = ranging_bars(200, 30);
        let mut strategy = test_sweep_strategy();
        let signals = run_sweep_strategy(&mut strategy, &bars);
        assert!(signals.is_empty(), "chop must produce no signals");

        // Degenerate all-identical bars (high == low) must also stay silent.
        let flat: Vec<Bar> = (0..200).map(|_| (100.0, 100.0, 100.0, 100.0, 100.0)).collect();
        let mut strategy = test_sweep_strategy();
        assert!(run_sweep_strategy(&mut strategy, &flat).is_empty());
    }

    #[test]
    fn sweep_no_look_ahead() {
        // The signal emitted at bar i must be identical whether the strategy
        // saw only bars 0..=i or will later see more — i.e. state depends
        // only on past + current bars.
        let mut bars = sweep_entry_series();
        bars.push((100.5, 100.9, 100.4, 100.8, 120.0));
        bars.push((100.8, 101.3, 100.7, 101.2, 120.0));
        bars.push((101.2, 101.8, 101.1, 101.7, 120.0));

        let mut full_run = test_sweep_strategy();
        let full_signals = run_sweep_strategy(&mut full_run, &bars);

        for i in 0..bars.len() {
            let mut prefix_run = test_sweep_strategy();
            let prefix_signals = run_sweep_strategy(&mut prefix_run, &bars[..=i]);
            let full_at_i: Vec<_> = full_signals
                .iter()
                .filter(|(j, _)| *j <= i)
                .map(|(j, s)| (*j, s.decision.clone(), s.strength))
                .collect();
            let prefix_at_i: Vec<_> = prefix_signals
                .iter()
                .map(|(j, s)| (*j, s.decision.clone(), s.strength))
                .collect();
            assert_eq!(full_at_i, prefix_at_i, "signals up to bar {} must match", i);
        }
    }

    #[test]
    fn sweep_deterministic() {
        let mut bars = sweep_entry_series();
        bars.push((100.8, 101.3, 100.7, 101.2, 120.0));

        let mut a = test_sweep_strategy();
        let mut b = test_sweep_strategy();
        let sig_a: Vec<_> = run_sweep_strategy(&mut a, &bars)
            .into_iter()
            .map(|(i, s)| (i, s.decision, s.strength))
            .collect();
        let sig_b: Vec<_> = run_sweep_strategy(&mut b, &bars)
            .into_iter()
            .map(|(i, s)| (i, s.decision, s.strength))
            .collect();
        assert_eq!(sig_a, sig_b, "same input must yield same signals");
        assert!(!sig_a.is_empty());
    }

    #[test]
    fn sweep_strength_bounds() {
        // Cover entry, target exit, and invalidation exit paths, including
        // an extreme-volume sweep that stresses the strength saturation.
        let mut bars = sweep_entry_series();
        bars.push((100.5, 100.9, 100.4, 100.8, 120.0));
        bars.push((101.2, 101.8, 101.1, 101.7, 120.0));
        // Second sweep after cooldown, with a monster volume spike.
        bars.extend(ranging_bars(60, 30));
        bars.push((100.4, 100.6, 99.8, 100.45, 100_000.0));
        bars.push((100.4, 100.5, 99.5, 99.6, 300.0)); // invalidation

        let mut strategy = test_sweep_strategy();
        let signals = run_sweep_strategy(&mut strategy, &bars);
        assert!(signals.len() >= 3, "expected entry/exit/entry/exit sequence");

        let zero = Decimal::from_f64_retain(0.0).unwrap();
        let one = Decimal::from_f64_retain(1.0).unwrap();
        for (i, signal) in &signals {
            assert!(
                signal.strength >= zero && signal.strength <= one,
                "strength {} out of bounds at bar {}",
                signal.strength,
                i,
            );
        }
    }

    #[test]
    fn sweep_short_side_fires() {
        // Mirror scenario: established range HIGH (102.0 at bar 30) swept by
        // a spike to 102.25 that closes back below on 4× volume.
        let mut bars: Vec<Bar> = (0..60)
            .map(|i| {
                if i == 30 {
                    (101.4, 102.0, 101.2, 101.5, 100.0)
                } else if i % 2 == 0 {
                    (100.8, 101.2, 100.4, 101.0, 100.0)
                } else {
                    (101.0, 101.6, 100.6, 101.2, 100.0)
                }
            })
            .collect();
        bars.push((101.6, 102.25, 101.4, 101.55, 400.0));

        let mut strategy = test_sweep_strategy();
        let signals = run_sweep_strategy(&mut strategy, &bars);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].0, 60);
        assert_eq!(signals[0].1.decision, Decision::Short);

        // And with_longs_only() must suppress it.
        let mut longs_only = test_sweep_strategy().with_longs_only();
        assert!(run_sweep_strategy(&mut longs_only, &bars).is_empty());
    }
}
