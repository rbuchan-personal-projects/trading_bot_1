//! Execution Module (The Hand)
//!
//! Sends actual orders to the exchange.

pub mod live;

use async_trait::async_trait;
use crate::types::Order;
use crate::types::OrderResponse;

/// Trait for order execution.
///
/// Implement this trait to execute orders on any exchange,
/// or use a mock executor for backtesting and paper trading.
#[async_trait]
pub trait OrderExecutor: Send + Sync {
    /// Execute an order on the exchange.
    async fn execute(&self, order: Order) -> Result<OrderResponse, Box<dyn std::error::Error>>;

    /// Check the status of an existing order.
    async fn check_order_status(&self, order_id: &str) -> Result<OrderResponse, Box<dyn std::error::Error>>;

    /// Cancel an existing order.
    async fn cancel_order(&self, order_id: &str) -> Result<(), Box<dyn std::error::Error>>;
}

