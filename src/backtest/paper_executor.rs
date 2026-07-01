//! Paper Executor — simulated order fills for backtesting.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::types::Decision;
use crate::strategy_engine::Signal;
use crate::risk_manager::{PositionRiskMonitor, RiskControls};

fn default_regime() -> String { "N/A".to_string() }

/// A completed round-trip trade (entry + exit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub entry_bar: usize,
    pub exit_bar: usize,
    pub entry_time: DateTime<Utc>,
    pub exit_time: DateTime<Utc>,
    pub entry_price: f64,
    pub exit_price: f64,
    pub pnl_pct: f64,
    pub entry_decision: Decision,
    pub exit_decision: Decision,
    pub entry_strength: Decimal,
    /// Market regime label at the time of entry (e.g. "↑TrendUp", "N/A").
    #[serde(default = "default_regime")]
    pub regime: String,
}

#[derive(Debug, Clone)]
struct OpenPosition {
    bar: usize,
    time: DateTime<Utc>,
    price: f64,
    decision: Decision,
    strength: Decimal,
    regime: String,
}

/// Simulated executor that fills orders instantly at the candle close price.
#[derive(Debug)]
pub struct PaperExecutor {
    position: Option<OpenPosition>,
    pub trades: Vec<TradeRecord>,
    pub initial_capital: f64,
    pub equity: f64,
    pub equity_curve: Vec<f64>,
    peak_equity: f64,
    pub max_drawdown_pct: f64,
    /// Position-level guard rails (see `risk_manager::position_risk`).
    /// `None` ⇒ no stops / no time-cap (original behaviour preserved).
    risk_monitor: Option<PositionRiskMonitor>,
    /// Regime label staged by the caller before the next Long entry.
    /// Consumed by `process_signal` when a position opens.
    pending_regime: Option<String>,
    /// Exchange fee charged per side, as a percentage (e.g. 0.1 for 0.1%).
    /// Deducted as a round-trip cost (2×) on every completed trade.
    fee_pct_per_side: f64,
    /// Estimated slippage per side, as a percentage (e.g. 0.05 for 0.05%).
    /// Deducted alongside fees on every completed trade.
    slippage_pct_per_side: f64,
}

impl PaperExecutor {
    pub fn new(initial_capital: f64) -> Self {
        Self {
            position: None,
            trades: Vec::new(),
            initial_capital,
            equity: initial_capital,
            equity_curve: vec![initial_capital],
            peak_equity: initial_capital,
            max_drawdown_pct: 0.0,
            risk_monitor: None,
            pending_regime: None,
            fee_pct_per_side: 0.0,
            slippage_pct_per_side: 0.0,
        }
    }

    /// Set the per-side exchange fee (e.g. `0.1` for Binance taker 0.1%).
    /// Applied as a round-trip deduction (entry + exit) on every closed trade.
    pub fn with_fees(mut self, fee_pct_per_side: f64) -> Self {
        self.fee_pct_per_side = fee_pct_per_side;
        self
    }

    /// Set the per-side slippage estimate (e.g. `0.05` for half-spread).
    /// Applied alongside fees on every closed trade.
    pub fn with_slippage(mut self, slippage_pct_per_side: f64) -> Self {
        self.slippage_pct_per_side = slippage_pct_per_side;
        self
    }

    fn round_trip_cost(&self) -> f64 {
        2.0 * (self.fee_pct_per_side + self.slippage_pct_per_side)
    }

    /// Stage a regime label to be recorded on the next Long entry.
    /// Call this immediately before `process_signal` for regime-tracked strategies.
    pub fn tag_next_entry(&mut self, regime: String) {
        self.pending_regime = Some(regime);
    }

    /// Construct an executor that consults a [`PositionRiskMonitor`] on each
    /// bar to decide whether to force-close the active position (stop-loss,
    /// trailing-stop, time-stop).  Strategy `ClosePosition` signals still
    /// take priority within a bar — risk only triggers if the strategy
    /// hasn't already closed.
    pub fn with_risk(initial_capital: f64, controls: RiskControls) -> Self {
        let mut e = Self::new(initial_capital);
        e.risk_monitor = Some(PositionRiskMonitor::new(controls));
        e
    }

    pub fn process_signal(&mut self, bar: usize, price: f64, signal: &Signal) {
        match &signal.decision {
            Decision::Long | Decision::Short => {
                if self.position.is_none() {
                    self.position = Some(OpenPosition {
                        bar,
                        time: signal.time,
                        price,
                        decision: signal.decision.clone(),
                        strength: signal.strength,
                        regime: self.pending_regime.take().unwrap_or_else(|| "N/A".to_string()),
                    });
                    if let Some(m) = self.risk_monitor.as_mut() {
                        m.reset();
                    }
                }
            }
            Decision::ClosePosition => {
                if let Some(pos) = self.position.take() {
                    let gross_pnl = match &pos.decision {
                        Decision::Long => (price - pos.price) / pos.price * 100.0,
                        Decision::Short => (pos.price - price) / pos.price * 100.0,
                        _ => 0.0,
                    };
                    let pnl_pct = gross_pnl - self.round_trip_cost();
                    self.equity *= 1.0 + pnl_pct / 100.0;
                    self.trades.push(TradeRecord {
                        entry_bar: pos.bar,
                        exit_bar: bar,
                        entry_time: pos.time,
                        exit_time: signal.time,
                        entry_price: pos.price,
                        exit_price: price,
                        pnl_pct,
                        entry_decision: pos.decision,
                        exit_decision: Decision::ClosePosition,
                        entry_strength: pos.strength,
                        regime: pos.regime,
                    });
                }
            }
        }
    }

    pub fn sample_equity(&mut self, current_price: f64) {
        let mtm_equity = if let Some(ref pos) = self.position {
            let unrealized = match &pos.decision {
                Decision::Long => (current_price - pos.price) / pos.price,
                Decision::Short => (pos.price - current_price) / pos.price,
                _ => 0.0,
            };
            self.equity * (1.0 + unrealized)
        } else {
            self.equity
        };
        self.equity_curve.push(mtm_equity);
        if mtm_equity > self.peak_equity {
            self.peak_equity = mtm_equity;
        }
        if self.peak_equity > 0.0 {
            let dd = (self.peak_equity - mtm_equity) / self.peak_equity * 100.0;
            if dd > self.max_drawdown_pct {
                self.max_drawdown_pct = dd;
            }
        }
    }

    /// Consult the [`PositionRiskMonitor`] (if any) and close the position
    /// if any risk rule trips.  Returns the realised PnL% if a close
    /// happened, otherwise `None`.
    ///
    /// This is the answer to "is the RSI selling time optimal?": call this
    /// every bar AFTER the strategy signal so that even a quiet RSI can't
    /// hold a fading position indefinitely.
    pub fn check_risk_exit(
        &mut self,
        bar: usize,
        price: f64,
        time: DateTime<Utc>,
    ) -> Option<f64> {
        let pos = self.position.as_ref()?;
        let monitor = self.risk_monitor.as_mut()?;

        let unrealised_pct = match &pos.decision {
            Decision::Long => (price - pos.price) / pos.price * 100.0,
            Decision::Short => (pos.price - price) / pos.price * 100.0,
            _ => 0.0,
        };
        let bars_held = bar.saturating_sub(pos.bar);

        let reason = monitor.evaluate(unrealised_pct, bars_held, &pos.decision)?;
        // Trigger → close at current price.  Risk evaluation uses gross PnL
        // (stop levels are in gross terms); recorded PnL is net of fees.
        let _ = reason;
        let pos = self.position.take().unwrap();
        let net_pnl = unrealised_pct - self.round_trip_cost();
        self.equity *= 1.0 + net_pnl / 100.0;
        self.trades.push(TradeRecord {
            entry_bar: pos.bar,
            exit_bar: bar,
            entry_time: pos.time,
            exit_time: time,
            entry_price: pos.price,
            exit_price: price,
            pnl_pct: net_pnl,
            entry_decision: pos.decision,
            exit_decision: Decision::ClosePosition,
            entry_strength: pos.strength,
            regime: pos.regime,
        });
        Some(unrealised_pct)
    }

    /// Discard an open position without booking a trade.  Used by the live
    /// binary on Ctrl-C when we have no real market price to close at —
    /// preferable to inventing one (which is what the old code did,
    /// producing the bogus "BTC @ $10,017" exits).
    pub fn discard_open_position(&mut self) {
        self.position = None;
        if let Some(m) = self.risk_monitor.as_mut() {
            m.reset();
        }
    }

    pub fn force_close(&mut self, bar: usize, price: f64, time: DateTime<Utc>) {
        if let Some(pos) = self.position.take() {
            let gross_pnl = match &pos.decision {
                Decision::Long => (price - pos.price) / pos.price * 100.0,
                Decision::Short => (pos.price - price) / pos.price * 100.0,
                _ => 0.0,
            };
            let pnl_pct = gross_pnl - self.round_trip_cost();
            self.equity *= 1.0 + pnl_pct / 100.0;
            self.trades.push(TradeRecord {
                entry_bar: pos.bar,
                exit_bar: bar,
                entry_time: pos.time,
                exit_time: time,
                entry_price: pos.price,
                exit_price: price,
                pnl_pct,
                entry_decision: pos.decision,
                exit_decision: Decision::ClosePosition,
                entry_strength: pos.strength,
                regime: pos.regime,
            });
        }
    }

    pub fn is_in_position(&self) -> bool { self.position.is_some() }
    pub fn total_trades(&self) -> usize { self.trades.len() }

    pub fn winning_trades(&self) -> usize {
        self.trades.iter().filter(|t| t.pnl_pct > 0.0).count()
    }

    pub fn losing_trades(&self) -> usize {
        self.trades.iter().filter(|t| t.pnl_pct <= 0.0).count()
    }

    pub fn win_rate(&self) -> f64 {
        if self.trades.is_empty() { 0.0 }
        else { self.winning_trades() as f64 / self.trades.len() as f64 * 100.0 }
    }

    pub fn total_pnl_pct(&self) -> f64 {
        (self.equity - self.initial_capital) / self.initial_capital * 100.0
    }

    pub fn average_pnl_pct(&self) -> f64 {
        if self.trades.is_empty() { 0.0 }
        else { self.trades.iter().map(|t| t.pnl_pct).sum::<f64>() / self.trades.len() as f64 }
    }

    pub fn best_trade_pct(&self) -> f64 {
        self.trades.iter().map(|t| t.pnl_pct).fold(f64::NEG_INFINITY, f64::max)
    }

    pub fn worst_trade_pct(&self) -> f64 {
        self.trades.iter().map(|t| t.pnl_pct).fold(f64::INFINITY, f64::min)
    }

    /// Simplified Sharpe ratio (risk-free = 0).
    pub fn sharpe_ratio(&self) -> f64 {
        if self.trades.len() < 2 { return 0.0; }
        let r: Vec<f64> = self.trades.iter().map(|t| t.pnl_pct).collect();
        let m = r.iter().sum::<f64>() / r.len() as f64;
        let v = r.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (r.len() - 1) as f64;
        let s = v.sqrt();
        if s == 0.0 { 0.0 } else { m / s }
    }

    /// Profit factor = gross profit / gross loss.
    pub fn profit_factor(&self) -> f64 {
        let gp: f64 = self.trades.iter().filter(|t| t.pnl_pct > 0.0).map(|t| t.pnl_pct).sum();
        let gl: f64 = self.trades.iter().filter(|t| t.pnl_pct < 0.0).map(|t| t.pnl_pct.abs()).sum();
        if gl == 0.0 { f64::INFINITY } else { gp / gl }
    }
}
