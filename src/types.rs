use serde::{Deserialize, Serialize};
use rust_decimal::Decimal;

// ---------------------------------------------------------------------------
// Market Data Types
// ---------------------------------------------------------------------------

/// Market tick data received from exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketTick {
    pub symbol: String,
    pub timestamp: String,
    pub bid: Decimal,
    pub ask: Decimal,
    pub last_price: Decimal,
    pub volume: Decimal,
}

// ---------------------------------------------------------------------------
// Signal Types
// ---------------------------------------------------------------------------

/// Trading signal decision from the strategy engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TradingSignal {
    Buy,
    Sell,
    Hold,
}

/// Richer signal decision used by [`SignalGenerator`](crate::strategy_engine::SignalGenerator)
/// strategies that interact with barter-data [`MarketEvent`]s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// Open / add to a long position.
    Long,
    /// Open / add to a short position.
    Short,
    /// Close an existing position (long or short).
    ClosePosition,
}

// ---------------------------------------------------------------------------
// Order Types
// ---------------------------------------------------------------------------

/// Order types supported by the exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit,
    StopLoss,
}

/// Buy/Sell direction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderSide {
    Buy,
    Sell,
}

/// TODO these will need refactoring once we decide on an exchange
/// An order to be sent to the exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub symbol: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub quantity: Decimal,
    pub price: Option<Decimal>,
}

/// Response received after submitting an order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResponse {
    pub order_id: String,
    pub status: String,
    pub filled_quantity: Decimal,
    pub average_price: Option<Decimal>,
}

