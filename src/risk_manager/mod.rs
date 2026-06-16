//! Risk Manager Module (The Guard)
//!
//! Validates trades against position limits and risk thresholds.
//! Never skip this — it's critical for capital preservation.

pub mod live;
pub mod position_risk;

pub use position_risk::{PositionRiskMonitor, RiskControls, RiskExitReason};

use async_trait::async_trait;
use rust_decimal::Decimal;

/// Trait for risk evaluation.
///
/// Implement this trait to enforce risk rules before any trade.
/// Swap in a permissive mock for backtesting or a strict guard for production.
#[async_trait]
pub trait RiskEvaluator: Send + Sync {
    /// Validate whether a trade is within acceptable risk limits.
    /// Returns Ok(()) if approved, or Err with a reason string if rejected.
    async fn validate_trade(
        &self,
        symbol: &str,
        position_size: Decimal,
        entry_price: Decimal,
    ) -> Result<(), String>;

    /// Record a realized loss to track daily P&L.
    async fn record_loss(&mut self, amount: Decimal);

    /// Reset daily risk metrics (call at start of each trading day).
    async fn reset_daily_metrics(&mut self);
}

