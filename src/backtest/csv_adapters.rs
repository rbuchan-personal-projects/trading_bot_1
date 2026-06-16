//! CSV Format Adapters
//!
//! Different exchanges and data providers export CSV data in wildly different
//! formats. This module provides a trait-based adapter system that normalises
//! any source into our standard [`OhlcvRow`](super::csv_source::OhlcvRow).
//!
//! ## Supported formats
//!
//! | Adapter                    | Source             | Delimiter | Notes                              |
//! |----------------------------|--------------------|-----------|-------------------------------------|
//! | [`StandardAdapter`]        | Generic OHLCV      | `,`       | `timestamp,open,high,low,close,vol` |
//! | [`CoinMarketCapAdapter`]   | CoinMarketCap      | `;`       | BOM, quoted, reverse-chrono         |
//! | [`BinanceHeaderAdapter`]   | Binance w/ header  | `,`       | `open_time,...` header, ms ts       |
//! | [`BinanceRawAdapter`]      | Binance raw klines | `,`       | No header, 12 cols, μs timestamps   |
//!
//! ## Multi-file loading
//!
//! Use [`load_adaptive_glob`] to load and merge many files matching a glob
//! pattern (e.g. `data/BTCUSD-1m-*.csv`), sorted chronologically.

use std::fs;
use std::path::Path;

use barter_data::event::{DataKind, MarketEvent};
use barter_data::subscription::candle::Candle;
use barter_instrument::instrument::market_data::MarketDataInstrument;
use chrono::{DateTime, NaiveDateTime, Utc};

use super::csv_source::{CsvSourceConfig, OhlcvRow};

// ---------------------------------------------------------------------------
// Adapter trait
// ---------------------------------------------------------------------------

/// Trait for converting a specific CSV format into a `Vec<OhlcvRow>`.
pub trait CsvAdapter: Send + Sync {
    /// Human-readable name of this format.
    fn name(&self) -> &str;

    /// Return `true` if the first data line matches this format.
    fn matches(&self, first_line: &str) -> bool;

    /// Parse the raw file content into normalised OHLCV rows, sorted
    /// chronologically (oldest first).
    fn parse(&self, content: &str) -> Result<Vec<OhlcvRow>, Box<dyn std::error::Error>>;
}

// ---------------------------------------------------------------------------
// StandardAdapter — comma-delimited OHLCV with header
// ---------------------------------------------------------------------------

pub struct StandardAdapter;

impl CsvAdapter for StandardAdapter {
    fn name(&self) -> &str { "Standard OHLCV (comma)" }

    fn matches(&self, first_line: &str) -> bool {
        let h = first_line.to_lowercase();
        h.contains("timestamp") && h.contains("open") && h.contains(',')
    }

    fn parse(&self, content: &str) -> Result<Vec<OhlcvRow>, Box<dyn std::error::Error>> {
        let clean = content.trim_start_matches('\u{feff}');
        let mut reader = csv::Reader::from_reader(clean.as_bytes());
        let mut rows = Vec::new();
        for result in reader.deserialize() {
            let row: OhlcvRow = result?;
            rows.push(row);
        }
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// CoinMarketCapAdapter
// ---------------------------------------------------------------------------

pub struct CoinMarketCapAdapter;

impl CsvAdapter for CoinMarketCapAdapter {
    fn name(&self) -> &str { "CoinMarketCap (semicolon, monthly)" }

    fn matches(&self, first_line: &str) -> bool {
        let h = first_line.to_lowercase();
        h.contains("timeopen") && h.contains("marketcap") && h.contains(';')
    }

    fn parse(&self, content: &str) -> Result<Vec<OhlcvRow>, Box<dyn std::error::Error>> {
        let clean = content.trim_start_matches('\u{feff}');

        let mut reader = csv::ReaderBuilder::new()
            .delimiter(b';')
            .quoting(true)
            .from_reader(clean.as_bytes());

        let headers = reader.headers()?.clone();

        let idx = |name: &str| -> Option<usize> {
            headers.iter().position(|h| h.trim().eq_ignore_ascii_case(name))
        };

        let i_ts    = idx("timeOpen").ok_or("Missing 'timeOpen' column")?;
        let i_open  = idx("open").ok_or("Missing 'open' column")?;
        let i_high  = idx("high").ok_or("Missing 'high' column")?;
        let i_low   = idx("low").ok_or("Missing 'low' column")?;
        let i_close = idx("close").ok_or("Missing 'close' column")?;
        let i_vol   = idx("volume").ok_or("Missing 'volume' column")?;

        let mut rows = Vec::new();

        for result in reader.records() {
            let record = result?;

            let ts_raw = record.get(i_ts).unwrap_or("").trim().trim_matches('"');
            let open:   f64 = record.get(i_open).unwrap_or("0").trim().trim_matches('"').parse()?;
            let high:   f64 = record.get(i_high).unwrap_or("0").trim().trim_matches('"').parse()?;
            let low:    f64 = record.get(i_low).unwrap_or("0").trim().trim_matches('"').parse()?;
            let close:  f64 = record.get(i_close).unwrap_or("0").trim().trim_matches('"').parse()?;
            let volume: f64 = record.get(i_vol).unwrap_or("0").trim().trim_matches('"').parse()?;

            let timestamp = parse_flexible_timestamp(ts_raw)?;

            rows.push(OhlcvRow { timestamp, open, high, low, close, volume });
        }

        rows.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// BinanceHeaderAdapter — Binance klines WITH a header row (ms timestamps)
// ---------------------------------------------------------------------------

pub struct BinanceHeaderAdapter;

impl CsvAdapter for BinanceHeaderAdapter {
    fn name(&self) -> &str { "Binance Klines (with header, unix-ms)" }

    fn matches(&self, first_line: &str) -> bool {
        let h = first_line.to_lowercase();
        h.contains("open_time") && h.contains("quote_volume")
    }

    fn parse(&self, content: &str) -> Result<Vec<OhlcvRow>, Box<dyn std::error::Error>> {
        let clean = content.trim_start_matches('\u{feff}');
        let mut reader = csv::Reader::from_reader(clean.as_bytes());
        let mut rows = Vec::new();

        for result in reader.records() {
            let record = result?;
            let ts_ms: i64 = record.get(0).unwrap_or("0").parse()?;
            let open:  f64 = record.get(1).unwrap_or("0").parse()?;
            let high:  f64 = record.get(2).unwrap_or("0").parse()?;
            let low:   f64 = record.get(3).unwrap_or("0").parse()?;
            let close: f64 = record.get(4).unwrap_or("0").parse()?;
            let vol:   f64 = record.get(5).unwrap_or("0").parse()?;

            let dt = unix_to_datetime(ts_ms)?;
            rows.push(OhlcvRow {
                timestamp: dt.to_rfc3339(),
                open, high, low, close, volume: vol,
            });
        }

        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// BinanceRawAdapter — Binance klines WITHOUT header (μs or ms timestamps)
// ---------------------------------------------------------------------------

/// Parses raw Binance kline CSVs (no header row).
///
/// These files have 12 comma-separated columns and the first column is a
/// Unix timestamp that can be in **milliseconds** (13 digits) or
/// **microseconds** (16 digits). The adapter auto-detects which.
///
/// ```text
/// 1764835200000000,90990.23,99000.00,90990.23,99000.00,0.01015,...
/// ```
pub struct BinanceRawAdapter;

impl CsvAdapter for BinanceRawAdapter {
    fn name(&self) -> &str { "Binance Raw Klines (no header, auto μs/ms)" }

    fn matches(&self, first_line: &str) -> bool {
        let first_line = first_line.trim_start_matches('\u{feff}');
        let fields: Vec<&str> = first_line.split(',').collect();
        // Must have 12 columns, first column must be a large integer (timestamp)
        if fields.len() != 12 {
            return false;
        }
        match fields[0].trim().parse::<i64>() {
            Ok(v) => v > 1_000_000_000_000, // > year 2001 in ms
            Err(_) => false,
        }
    }

    fn parse(&self, content: &str) -> Result<Vec<OhlcvRow>, Box<dyn std::error::Error>> {
        let clean = content.trim_start_matches('\u{feff}');
        // No header — tell csv crate not to expect one
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(clean.as_bytes());

        let mut rows = Vec::new();

        for result in reader.records() {
            let record = result?;
            let ts_raw: i64 = record.get(0).unwrap_or("0").trim().parse()?;
            let open:   f64 = record.get(1).unwrap_or("0").trim().parse()?;
            let high:   f64 = record.get(2).unwrap_or("0").trim().parse()?;
            let low:    f64 = record.get(3).unwrap_or("0").trim().parse()?;
            let close:  f64 = record.get(4).unwrap_or("0").trim().parse()?;
            let vol:    f64 = record.get(5).unwrap_or("0").trim().parse()?;

            let dt = unix_to_datetime(ts_raw)?;
            rows.push(OhlcvRow {
                timestamp: dt.to_rfc3339(),
                open, high, low, close, volume: vol,
            });
        }

        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// Auto-detection
// ---------------------------------------------------------------------------

/// All known adapters, checked in order.
fn all_adapters() -> Vec<Box<dyn CsvAdapter>> {
    vec![
        Box::new(CoinMarketCapAdapter),
        Box::new(BinanceHeaderAdapter),
        Box::new(BinanceRawAdapter),
        Box::new(StandardAdapter), // fallback
    ]
}

/// Auto-detect the CSV format by inspecting the first line.
pub fn detect_format(content: &str) -> Option<Box<dyn CsvAdapter>> {
    let clean = content.trim_start_matches('\u{feff}');
    let first_line = clean.lines().next()?;
    for adapter in all_adapters() {
        if adapter.matches(first_line) {
            return Some(adapter);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Single-file loader
// ---------------------------------------------------------------------------

/// Load a CSV file of **any supported format**, auto-detecting the layout.
pub fn load_adaptive(
    file_path: &str,
    config: &CsvSourceConfig,
) -> Result<Vec<MarketEvent>, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(Path::new(file_path))?;
    let adapter = detect_format(&content)
        .ok_or_else(|| format!("Could not detect CSV format for: {}", file_path))?;

    println!("  Detected format: {}", adapter.name());

    let rows = adapter.parse(&content)?;
    rows_to_events(&rows, config)
}

/// Load CSV data from an in-memory string using auto-detection.
pub fn load_adaptive_from_str(
    csv_data: &str,
    config: &CsvSourceConfig,
) -> Result<Vec<MarketEvent>, Box<dyn std::error::Error>> {
    let adapter = detect_format(csv_data)
        .ok_or("Could not detect CSV format from string")?;
    let rows = adapter.parse(csv_data)?;
    rows_to_events(&rows, config)
}

// ---------------------------------------------------------------------------
// Multi-file loader (glob)
// ---------------------------------------------------------------------------

/// Load and merge multiple CSV files matching a **glob pattern**.
///
/// Files are sorted by filename (which works for date-stamped names like
/// `BTCUSD-1m-2025-12-04.csv`), then all rows are concatenated and sorted
/// by timestamp to produce one continuous chronological stream.
///
/// # Example
/// ```ignore
/// let events = load_adaptive_glob("data/BTCUSD-1m-*.csv", &config)?;
/// // → 150,000+ events from 158 daily files
/// ```
pub fn load_adaptive_glob(
    pattern: &str,
    config: &CsvSourceConfig,
) -> Result<Vec<MarketEvent>, Box<dyn std::error::Error>> {
    let mut paths: Vec<std::path::PathBuf> = glob::glob(pattern)?
        .filter_map(|r| r.ok())
        .collect();

    if paths.is_empty() {
        return Err(format!("No files matched glob pattern: {}", pattern).into());
    }

    // Sort by filename for deterministic ordering
    paths.sort();

    println!("  Loading {} files matching '{}'", paths.len(), pattern);

    let mut all_rows: Vec<OhlcvRow> = Vec::new();
    let mut adapter_name: Option<String> = None;

    for (i, path) in paths.iter().enumerate() {
        let content = fs::read_to_string(path)?;
        let adapter = detect_format(&content)
            .ok_or_else(|| format!("Could not detect format: {}", path.display()))?;

        if i == 0 {
            println!("  Detected format: {}", adapter.name());
            adapter_name = Some(adapter.name().to_string());
        }

        let mut rows = adapter.parse(&content)?;
        all_rows.append(&mut rows);
    }

    // Sort all rows chronologically (handles overlapping files or out-of-order)
    all_rows.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    // Deduplicate by timestamp (some files may have overlapping edge rows)
    all_rows.dedup_by(|a, b| a.timestamp == b.timestamp);

    println!(
        "  Loaded {} total candles from {} files ({})",
        all_rows.len(),
        paths.len(),
        adapter_name.unwrap_or_default(),
    );

    rows_to_events(&all_rows, config)
}

// ---------------------------------------------------------------------------
// Row → MarketEvent conversion
// ---------------------------------------------------------------------------

/// Convert parsed OHLCV rows into barter-data MarketEvents.
fn rows_to_events(
    rows: &[OhlcvRow],
    config: &CsvSourceConfig,
) -> Result<Vec<MarketEvent>, Box<dyn std::error::Error>> {
    let instrument = MarketDataInstrument::from((
        config.base.as_str(),
        config.quote.as_str(),
        config.instrument_kind.clone(),
    ));

    let mut events = Vec::with_capacity(rows.len());

    for row in rows {
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

        events.push(MarketEvent {
            time_exchange: ts,
            time_received: ts,
            exchange: config.exchange,
            instrument: instrument.clone(),
            kind: DataKind::Candle(candle),
        });
    }

    Ok(events)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a Unix timestamp (auto-detects seconds, milliseconds, or microseconds)
/// into a `DateTime<Utc>`.
fn unix_to_datetime(raw: i64) -> Result<DateTime<Utc>, Box<dyn std::error::Error>> {
    let (secs, nanos) = if raw > 1_000_000_000_000_000 {
        // Microseconds (16 digits)
        (raw / 1_000_000, ((raw % 1_000_000) * 1_000) as u32)
    } else if raw > 1_000_000_000_000 {
        // Milliseconds (13 digits)
        (raw / 1_000, ((raw % 1_000) * 1_000_000) as u32)
    } else {
        // Seconds (10 digits)
        (raw, 0u32)
    };

    DateTime::from_timestamp(secs, nanos)
        .ok_or_else(|| format!("Invalid unix timestamp: {}", raw).into())
}

/// Parse timestamps flexibly — handles ISO 8601 with or without fractional
/// seconds, and with or without trailing `Z`.
fn parse_flexible_timestamp(s: &str) -> Result<String, Box<dyn std::error::Error>> {
    if let Ok(dt) = s.parse::<DateTime<Utc>>() {
        return Ok(dt.to_rfc3339());
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Ok(naive.and_utc().to_rfc3339());
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(naive.and_utc().to_rfc3339());
    }
    if let Ok(naive) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(naive.and_hms_opt(0, 0, 0).unwrap().and_utc().to_rfc3339());
    }
    Err(format!("Cannot parse timestamp: '{}'", s).into())
}

