//! CSV Market Data Source
//!
//! Reads historical OHLCV candle data from CSV files and produces a stream
//! of barter-data [`MarketEvent`]s that can be fed directly into any
//! [`SignalGenerator`](crate::strategy_engine::SignalGenerator) strategy.
//!
//! ## Expected CSV format
//!
//! ```csv
//! timestamp,open,high,low,close,volume
//! 2025-01-01T00:00:00Z,42000.0,42500.0,41800.0,42300.0,150.5
//! ```
//!
//! - `timestamp`: ISO 8601 string (parsed via chrono)
//! - `open`, `high`, `low`, `close`, `volume`: f64

use std::path::Path;

use barter_data::event::{DataKind, MarketEvent};
use barter_data::subscription::candle::Candle;
use barter_instrument::exchange::ExchangeId;
use barter_instrument::instrument::market_data::kind::MarketDataInstrumentKind;
use barter_instrument::instrument::market_data::MarketDataInstrument;
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// A single row from the OHLCV CSV file.
#[derive(Debug, Clone, Deserialize)]
pub struct OhlcvRow {
    pub timestamp: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// Configuration for the CSV market data source.
#[derive(Debug, Clone)]
pub struct CsvSourceConfig {
    /// Path to the CSV file.
    pub file_path: String,
    /// Exchange to tag events with (for Signal metadata).
    pub exchange: ExchangeId,
    /// Base asset (e.g. "btc").
    pub base: String,
    /// Quote asset (e.g. "usd").
    pub quote: String,
    /// Instrument kind.
    pub instrument_kind: MarketDataInstrumentKind,
}

impl CsvSourceConfig {
    /// Shorthand for a spot crypto pair.
    pub fn crypto_spot(file_path: &str, exchange: ExchangeId, base: &str, quote: &str) -> Self {
        Self {
            file_path: file_path.to_string(),
            exchange,
            base: base.to_string(),
            quote: quote.to_string(),
            instrument_kind: MarketDataInstrumentKind::Spot,
        }
    }
}

/// Reads an OHLCV CSV and returns a `Vec<MarketEvent>` ready for backtesting.
///
/// Each row becomes a `MarketEvent` with `DataKind::Candle`.
pub fn load_csv(config: &CsvSourceConfig) -> Result<Vec<MarketEvent>, Box<dyn std::error::Error>> {
    let path = Path::new(&config.file_path);
    let mut reader = csv::Reader::from_path(path)?;

    let instrument = MarketDataInstrument::from((
        config.base.as_str(),
        config.quote.as_str(),
        config.instrument_kind.clone(),
    ));

    let mut events = Vec::new();

    for result in reader.deserialize() {
        let row: OhlcvRow = result?;
        let ts: DateTime<Utc> = row.timestamp.parse()?;

        let candle = Candle {
            close_time: ts,
            open: row.open,
            high: row.high,
            low: row.low,
            close: row.close,
            volume: row.volume,
            trade_count: 0,
        };

        let event = MarketEvent {
            time_exchange: ts,
            time_received: ts,
            exchange: config.exchange,
            instrument: instrument.clone(),
            kind: DataKind::Candle(candle),
        };

        events.push(event);
    }

    Ok(events)
}

/// Load OHLCV data from an in-memory string (useful for tests).
pub fn load_csv_from_str(
    csv_data: &str,
    config: &CsvSourceConfig,
) -> Result<Vec<MarketEvent>, Box<dyn std::error::Error>> {
    let mut reader = csv::Reader::from_reader(csv_data.as_bytes());

    let instrument = MarketDataInstrument::from((
        config.base.as_str(),
        config.quote.as_str(),
        config.instrument_kind.clone(),
    ));

    let mut events = Vec::new();

    for result in reader.deserialize() {
        let row: OhlcvRow = result?;
        let ts: DateTime<Utc> = row.timestamp.parse()?;

        let candle = Candle {
            close_time: ts,
            open: row.open,
            high: row.high,
            low: row.low,
            close: row.close,
            volume: row.volume,
            trade_count: 0,
        };

        let event = MarketEvent {
            time_exchange: ts,
            time_received: ts,
            exchange: config.exchange,
            instrument: instrument.clone(),
            kind: DataKind::Candle(candle),
        };

        events.push(event);
    }

    Ok(events)
}




