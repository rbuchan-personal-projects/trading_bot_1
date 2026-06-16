//! Position-level risk monitoring.
//!
//! This complements [`RiskEvaluator`](super::RiskEvaluator), which gates
//! trades **before** they open (position sizing, daily-loss caps, etc.).
//! Once a position is live, we still need a synchronous, per-bar guard that
//! can decide "exit this trade now" without waiting for the strategy to
//! produce a `ClosePosition` signal.
//!
//! ## Why this exists
//!
//! The RSI strategies only emit `ClosePosition` when RSI prints overbought.
//! If price tops out at RSI≈65 and starts dropping, the strategy will hold
//! the position indefinitely waiting for an overbought print that never
//! comes — riding the full drawdown.  The exit timing is therefore *not*
//! optimal on its own.
//!
//! [`PositionRiskMonitor`] sits underneath the strategy and force-closes
//! the position when any of the configured guard-rails trips, regardless of
//! what RSI is doing:
//!
//! - **Hard stop**  — close on a fixed unrealised-loss threshold.
//! - **Trailing stop** — once a winner, lock in profit if it gives back too
//!   much from its peak.
//! - **Max-hold time-stop** — close any position that overstays its welcome.
//!
//! All three are opt-in via [`RiskControls`].

use crate::types::Decision;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Opt-in position-level guard-rails.  Each `Option<_>` disables when `None`.
#[derive(Debug, Clone, Default)]
pub struct RiskControls {
    /// Close if unrealised PnL drops below `-hard_stop_pct` (in percent).
    /// Example: `Some(1.0)` ⇒ exit when down more than 1%.
    pub hard_stop_pct: Option<f64>,

    /// Trailing-stop retracement (in percent).  Once the trade has gone in
    /// our favour by at least `trailing_arm_pct`, we start tracking the
    /// peak unrealised PnL and close if PnL retraces by more than this
    /// many percentage points from the peak.
    ///
    /// Example: arm=0.3, stop=0.4 ⇒ once up >0.3%, exit if we give back
    /// more than 0.4 percentage points from the running peak.
    pub trailing_stop_pct: Option<f64>,
    pub trailing_arm_pct: Option<f64>,

    /// Close any position that has been open longer than this many bars.
    /// On 1-minute candles, `Some(60)` is a ≈1-hour ceiling.  Prevents the
    /// "RSI 65 forever" stall case from running indefinitely.
    pub max_hold_bars: Option<usize>,
}

impl RiskControls {
    /// Returns true if every field is `None` — the monitor will short-circuit.
    pub fn is_noop(&self) -> bool {
        self.hard_stop_pct.is_none()
            && self.trailing_stop_pct.is_none()
            && self.max_hold_bars.is_none()
    }
}

// ---------------------------------------------------------------------------
// Reasons & monitor
// ---------------------------------------------------------------------------

/// Why the monitor decided to close the position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskExitReason {
    HardStop,
    TrailingStop,
    MaxHold,
}

/// Tracks an open position's worst/best mark-to-market and decides whether
/// any of the [`RiskControls`] should force an exit on the current bar.
///
/// The monitor is *stateful per position*: call [`PositionRiskMonitor::reset`]
/// when a new position opens, [`PositionRiskMonitor::evaluate`] on every
/// closed bar, and check the returned [`RiskExitReason`].
#[derive(Debug, Clone, Default)]
pub struct PositionRiskMonitor {
    controls: RiskControls,
    /// Peak unrealised PnL (%) seen since entry — used by the trailing stop.
    peak_pnl_pct: f64,
    /// Whether the trailing stop has been "armed" (i.e. PnL exceeded
    /// `trailing_arm_pct` at some point).  Until armed, retracement is
    /// ignored — we don't want to trail a position that's still underwater.
    trailing_armed: bool,
}

impl PositionRiskMonitor {
    pub fn new(controls: RiskControls) -> Self {
        Self {
            controls,
            peak_pnl_pct: 0.0,
            trailing_armed: false,
        }
    }

    /// Reset trailing state for a fresh position.
    pub fn reset(&mut self) {
        self.peak_pnl_pct = 0.0;
        self.trailing_armed = false;
    }

    pub fn controls(&self) -> &RiskControls {
        &self.controls
    }

    /// Decide whether to exit on this bar.
    ///
    /// `unrealised_pnl_pct` is the current mark-to-market PnL of the open
    /// position in percent (positive = winning).  `bars_held` is the number
    /// of bars elapsed since entry.  Returns `Some(reason)` if any rule
    /// trips, otherwise `None`.
    pub fn evaluate(
        &mut self,
        unrealised_pnl_pct: f64,
        bars_held: usize,
        _decision: &Decision,
    ) -> Option<RiskExitReason> {
        if self.controls.is_noop() {
            return None;
        }

        // 1. Hard stop — checked first because it bounds the worst case.
        if let Some(limit) = self.controls.hard_stop_pct {
            if unrealised_pnl_pct <= -limit {
                return Some(RiskExitReason::HardStop);
            }
        }

        // 2. Trailing stop — only meaningful once we've actually been in
        // profit by `arm` percent.  Two-stage to avoid clipping early noise.
        if let (Some(stop), Some(arm)) =
            (self.controls.trailing_stop_pct, self.controls.trailing_arm_pct)
        {
            if !self.trailing_armed && unrealised_pnl_pct >= arm {
                self.trailing_armed = true;
                self.peak_pnl_pct = unrealised_pnl_pct;
            }
            if self.trailing_armed {
                if unrealised_pnl_pct > self.peak_pnl_pct {
                    self.peak_pnl_pct = unrealised_pnl_pct;
                }
                if self.peak_pnl_pct - unrealised_pnl_pct >= stop {
                    return Some(RiskExitReason::TrailingStop);
                }
            }
        }

        // 3. Time-stop — cap how long a stalled position can hang around.
        if let Some(max) = self.controls.max_hold_bars {
            if bars_held >= max {
                return Some(RiskExitReason::MaxHold);
            }
        }

        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn long() -> Decision { Decision::Long }

    #[test]
    fn noop_controls_never_exit() {
        let mut m = PositionRiskMonitor::new(RiskControls::default());
        assert!(m.evaluate(-50.0, 1000, &long()).is_none());
    }

    #[test]
    fn hard_stop_triggers_at_threshold() {
        let mut m = PositionRiskMonitor::new(RiskControls {
            hard_stop_pct: Some(1.0),
            ..Default::default()
        });
        assert!(m.evaluate(-0.5, 5, &long()).is_none());
        assert_eq!(m.evaluate(-1.5, 6, &long()), Some(RiskExitReason::HardStop));
    }

    #[test]
    fn trailing_stop_arms_then_trails() {
        let mut m = PositionRiskMonitor::new(RiskControls {
            trailing_stop_pct: Some(0.4),
            trailing_arm_pct: Some(0.3),
            ..Default::default()
        });
        // Below arm threshold — should not fire even on a "give-back" move.
        assert!(m.evaluate(0.2, 1, &long()).is_none());
        assert!(m.evaluate(-0.1, 2, &long()).is_none());

        // Cross arm threshold — peak tracking begins.
        assert!(m.evaluate(0.5, 3, &long()).is_none());
        // Push higher — peak should update to 0.9.
        assert!(m.evaluate(0.9, 4, &long()).is_none());
        // Retrace 0.3pp — not enough to trip.
        assert!(m.evaluate(0.6, 5, &long()).is_none());
        // Retrace > 0.4pp from peak — trip.
        assert_eq!(
            m.evaluate(0.4, 6, &long()),
            Some(RiskExitReason::TrailingStop)
        );
    }

    #[test]
    fn max_hold_bars_caps_position_age() {
        let mut m = PositionRiskMonitor::new(RiskControls {
            max_hold_bars: Some(10),
            ..Default::default()
        });
        assert!(m.evaluate(0.0, 9, &long()).is_none());
        assert_eq!(m.evaluate(0.0, 10, &long()), Some(RiskExitReason::MaxHold));
    }

    #[test]
    fn reset_clears_trailing_state() {
        let mut m = PositionRiskMonitor::new(RiskControls {
            trailing_stop_pct: Some(0.4),
            trailing_arm_pct: Some(0.3),
            ..Default::default()
        });
        m.evaluate(0.5, 1, &long()); // arms
        m.evaluate(1.0, 2, &long()); // peak = 1.0
        m.reset();
        // After reset, a tiny positive PnL should not cause a trail-trigger.
        assert!(m.evaluate(0.1, 3, &long()).is_none());
    }
}
