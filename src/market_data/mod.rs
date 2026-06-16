//! Market Data Module (The Ear)
//!
//! Connects to WebSockets/REST APIs to fetch price "ticks" or "candles."

pub mod live;

use async_trait::async_trait;
use crate::types::MarketTick;

/// Trait for market data providers.
///
/// Implement this trait to supply market data from any source:
/// live exchange feeds, historical replay, or mock data for backtesting.
#[async_trait]
pub trait MarketProvider: Send + Sync {
    /// Subscribe to market data for a given symbol.
    async fn subscribe(&mut self, symbol: &str) -> Result<(), Box<dyn std::error::Error>>;

    /// Fetch the latest market tick for a given symbol.
    async fn get_tick(&self, symbol: &str) -> Result<MarketTick, Box<dyn std::error::Error>>;
}

