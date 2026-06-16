//! Backtest Module
//!
//! Provides a complete historical backtesting framework:
//! - [`csv_source`] — load OHLCV candles from CSV into barter-data `MarketEvent`s
//! - [`paper_executor`] — simulated order fills with P&L tracking
//! - [`report`] — JSON/CSV report generation
//!
//! ## Quick start
//!
//! ```ignore
//! use crate::backtest::{BacktestEngine, BacktestConfig};
//! use crate::strategy_engine::live::RsiStrategy;
//!
//! let config = BacktestConfig { /* ... */ };
//! let mut engine = BacktestEngine::new(config);
//! let report = engine.run(&mut RsiStrategy::new());
//! report.print_summary();
//! report.write_json("reports/rsi_backtest.json")?;
//! ```

pub mod csv_source;
pub mod csv_adapters;
pub mod paper_executor;
pub mod report;

use barter_data::event::{DataKind, MarketEvent};
use barter_instrument::exchange::ExchangeId;
use barter_instrument::instrument::market_data::kind::MarketDataInstrumentKind;

use crate::strategy_engine::SignalGenerator;
use self::csv_source::CsvSourceConfig;
use self::paper_executor::PaperExecutor;
use self::report::BacktestReport;

/// Configuration for a backtest run.
#[derive(Debug, Clone)]
pub struct BacktestConfig {
    /// Path to OHLCV CSV file.
    pub csv_path: String,
    /// Exchange tag for events.
    pub exchange: ExchangeId,
    /// Base asset (e.g. "btc").
    pub base: String,
    /// Quote asset (e.g. "usd").
    pub quote: String,
    /// Instrument kind.
    pub instrument_kind: MarketDataInstrumentKind,
    /// Starting capital for the paper executor.
    pub initial_capital: f64,
    /// Human-readable strategy name (for reports).
    pub strategy_name: String,
}

impl BacktestConfig {
    /// Convenience constructor for a crypto spot backtest.
    pub fn crypto_spot(
        csv_path: &str,
        exchange: ExchangeId,
        base: &str,
        quote: &str,
        initial_capital: f64,
        strategy_name: &str,
    ) -> Self {
        Self {
            csv_path: csv_path.to_string(),
            exchange,
            base: base.to_string(),
            quote: quote.to_string(),
            instrument_kind: MarketDataInstrumentKind::Spot,
            initial_capital,
            strategy_name: strategy_name.to_string(),
        }
    }
}

/// The backtest engine. Wires CSV data source, strategy, and paper executor.
pub struct BacktestEngine {
    config: BacktestConfig,
}

impl BacktestEngine {
    pub fn new(config: BacktestConfig) -> Self {
        Self { config }
    }

    /// Run the backtest: load CSV data, feed each candle to the strategy,
    /// execute signals on the paper executor, and return a report.
    pub fn run(&self, strategy: &mut dyn SignalGenerator) -> Result<BacktestReport, Box<dyn std::error::Error>> {
        let csv_config = CsvSourceConfig::crypto_spot(
            &self.config.csv_path,
            self.config.exchange,
            &self.config.base,
            &self.config.quote,
        );

        let events = csv_source::load_csv(&csv_config)?;
        let report = Self::run_on_events(&self.config, strategy, &events);
        Ok(report)
    }

    /// Run the backtest using the adaptive CSV loader (auto-detects format).
    /// Use this when you don't know or don't care about the CSV format —
    /// it will try CoinMarketCap, Binance, and Standard adapters.
    pub fn run_adaptive(&self, strategy: &mut dyn SignalGenerator) -> Result<BacktestReport, Box<dyn std::error::Error>> {
        let csv_config = CsvSourceConfig::crypto_spot(
            &self.config.csv_path,
            self.config.exchange,
            &self.config.base,
            &self.config.quote,
        );

        let events = csv_adapters::load_adaptive(&self.config.csv_path, &csv_config)?;
        let report = Self::run_on_events(&self.config, strategy, &events);
        Ok(report)
    }

    /// Run the backtest on pre-loaded events (useful for tests with in-memory data).
    pub fn run_on_events(
        config: &BacktestConfig,
        strategy: &mut dyn SignalGenerator,
        events: &[MarketEvent],
    ) -> BacktestReport {
        let mut executor = PaperExecutor::new(config.initial_capital);
        let total_bars = events.len();

        for (i, event) in events.iter().enumerate() {
            // Extract close price for the executor.
            let price = match &event.kind {
                DataKind::Candle(c) => c.close,
                DataKind::Trade(t) => t.price,
                _ => continue,
            };

            // Run strategy.
            if let Some(signal) = strategy.generate_signal(event) {
                executor.process_signal(i, price, &signal);
            }

            // Sample equity each bar.
            executor.sample_equity(price);
        }

        // Force-close any open position at the last price.
        if executor.is_in_position() {
            if let Some(last) = events.last() {
                let price = match &last.kind {
                    DataKind::Candle(c) => c.close,
                    DataKind::Trade(t) => t.price,
                    _ => 0.0,
                };
                executor.force_close(total_bars - 1, price, last.time_exchange);
            }
        }

        BacktestReport::from_executor(
            &executor,
            &config.strategy_name,
            &config.csv_path,
            total_bars,
        )
    }
}

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use super::csv_source::{CsvSourceConfig, load_csv_from_str};
    use crate::strategy_engine::live::{RsiStrategy, DivergenceStrategy, RsiDivergenceStrategy};

    /// 100 bars of synthetic BTC-USD 1h candles with a trend reversal.
    fn sample_csv_data() -> &'static str {
        // Starts at 42000, trends up to ~44000, crashes to ~39000, recovers to ~41000
        r#"timestamp,open,high,low,close,volume
2025-01-01T00:00:00Z,42000.0,42150.0,41900.0,42100.0,100.0
2025-01-01T01:00:00Z,42100.0,42250.0,42050.0,42200.0,110.0
2025-01-01T02:00:00Z,42200.0,42400.0,42150.0,42350.0,120.0
2025-01-01T03:00:00Z,42350.0,42500.0,42300.0,42450.0,95.0
2025-01-01T04:00:00Z,42450.0,42650.0,42400.0,42600.0,130.0
2025-01-01T05:00:00Z,42600.0,42750.0,42550.0,42700.0,105.0
2025-01-01T06:00:00Z,42700.0,42850.0,42650.0,42800.0,115.0
2025-01-01T07:00:00Z,42800.0,42950.0,42750.0,42900.0,108.0
2025-01-01T08:00:00Z,42900.0,43100.0,42850.0,43050.0,140.0
2025-01-01T09:00:00Z,43050.0,43200.0,43000.0,43150.0,125.0
2025-01-01T10:00:00Z,43150.0,43350.0,43100.0,43300.0,135.0
2025-01-01T11:00:00Z,43300.0,43450.0,43250.0,43400.0,110.0
2025-01-01T12:00:00Z,43400.0,43600.0,43350.0,43550.0,145.0
2025-01-01T13:00:00Z,43550.0,43700.0,43500.0,43650.0,120.0
2025-01-01T14:00:00Z,43650.0,43850.0,43600.0,43800.0,155.0
2025-01-01T15:00:00Z,43800.0,43950.0,43750.0,43900.0,130.0
2025-01-01T16:00:00Z,43900.0,44050.0,43800.0,43850.0,125.0
2025-01-01T17:00:00Z,43850.0,43900.0,43700.0,43750.0,140.0
2025-01-01T18:00:00Z,43750.0,43800.0,43500.0,43550.0,160.0
2025-01-01T19:00:00Z,43550.0,43600.0,43300.0,43350.0,175.0
2025-01-01T20:00:00Z,43350.0,43400.0,43100.0,43150.0,180.0
2025-01-01T21:00:00Z,43150.0,43200.0,42900.0,42950.0,190.0
2025-01-01T22:00:00Z,42950.0,43000.0,42700.0,42750.0,200.0
2025-01-01T23:00:00Z,42750.0,42800.0,42500.0,42550.0,210.0
2025-01-02T00:00:00Z,42550.0,42600.0,42300.0,42350.0,195.0
2025-01-02T01:00:00Z,42350.0,42400.0,42100.0,42150.0,185.0
2025-01-02T02:00:00Z,42150.0,42200.0,41900.0,41950.0,200.0
2025-01-02T03:00:00Z,41950.0,42000.0,41700.0,41750.0,215.0
2025-01-02T04:00:00Z,41750.0,41800.0,41500.0,41550.0,220.0
2025-01-02T05:00:00Z,41550.0,41600.0,41300.0,41350.0,225.0
2025-01-02T06:00:00Z,41350.0,41400.0,41100.0,41150.0,230.0
2025-01-02T07:00:00Z,41150.0,41200.0,40900.0,40950.0,235.0
2025-01-02T08:00:00Z,40950.0,41000.0,40700.0,40750.0,240.0
2025-01-02T09:00:00Z,40750.0,40800.0,40500.0,40550.0,245.0
2025-01-02T10:00:00Z,40550.0,40600.0,40300.0,40350.0,250.0
2025-01-02T11:00:00Z,40350.0,40400.0,40100.0,40150.0,240.0
2025-01-02T12:00:00Z,40150.0,40200.0,39900.0,39950.0,235.0
2025-01-02T13:00:00Z,39950.0,40000.0,39700.0,39750.0,230.0
2025-01-02T14:00:00Z,39750.0,39800.0,39500.0,39550.0,250.0
2025-01-02T15:00:00Z,39550.0,39600.0,39300.0,39350.0,260.0
2025-01-02T16:00:00Z,39350.0,39400.0,39200.0,39250.0,240.0
2025-01-02T17:00:00Z,39250.0,39350.0,39150.0,39300.0,220.0
2025-01-02T18:00:00Z,39300.0,39450.0,39250.0,39400.0,210.0
2025-01-02T19:00:00Z,39400.0,39550.0,39350.0,39500.0,200.0
2025-01-02T20:00:00Z,39500.0,39700.0,39450.0,39650.0,195.0
2025-01-02T21:00:00Z,39650.0,39800.0,39600.0,39750.0,190.0
2025-01-02T22:00:00Z,39750.0,39900.0,39700.0,39850.0,185.0
2025-01-02T23:00:00Z,39850.0,40000.0,39800.0,39950.0,180.0
2025-01-03T00:00:00Z,39950.0,40150.0,39900.0,40100.0,175.0
2025-01-03T01:00:00Z,40100.0,40250.0,40050.0,40200.0,170.0
2025-01-03T02:00:00Z,40200.0,40350.0,40150.0,40300.0,165.0
2025-01-03T03:00:00Z,40300.0,40500.0,40250.0,40450.0,175.0
2025-01-03T04:00:00Z,40450.0,40600.0,40400.0,40550.0,160.0
2025-01-03T05:00:00Z,40550.0,40700.0,40500.0,40650.0,155.0
2025-01-03T06:00:00Z,40650.0,40800.0,40600.0,40750.0,150.0
2025-01-03T07:00:00Z,40750.0,40900.0,40700.0,40850.0,145.0
2025-01-03T08:00:00Z,40850.0,41000.0,40800.0,40950.0,150.0
2025-01-03T09:00:00Z,40950.0,41100.0,40900.0,41050.0,155.0
2025-01-03T10:00:00Z,41050.0,41150.0,41000.0,41100.0,140.0
2025-01-03T11:00:00Z,41100.0,41200.0,41050.0,41150.0,135.0"#
    }

    fn csv_config() -> CsvSourceConfig {
        CsvSourceConfig::crypto_spot(
            "inline",
            ExchangeId::BinanceSpot,
            "btc",
            "usd",
        )
    }

    #[test]
    fn csv_source_parses_correctly() {
        let events = load_csv_from_str(sample_csv_data(), &csv_config()).unwrap();
        assert_eq!(events.len(), 60, "Expected 60 candles from sample CSV");
        // First candle close should be 42100
        if let DataKind::Candle(c) = &events[0].kind {
            assert!((c.close - 42100.0).abs() < 0.01);
        } else {
            panic!("Expected Candle event");
        }
        println!("CSV source parsed {} events", events.len());
    }

    #[test]
    fn backtest_rsi_strategy_on_csv() {
        let events = load_csv_from_str(sample_csv_data(), &csv_config()).unwrap();
        let config = BacktestConfig {
            csv_path: "sample_btc_1h.csv".to_string(),
            exchange: ExchangeId::BinanceSpot,
            base: "btc".to_string(),
            quote: "usd".to_string(),
            instrument_kind: MarketDataInstrumentKind::Spot,
            initial_capital: 10000.0,
            strategy_name: "RSI(14, 30/70)".to_string(),
        };

        let mut strategy = RsiStrategy::new();
        let report = BacktestEngine::run_on_events(&config, &mut strategy, &events);

        report.print_summary();
        assert!(report.summary.total_bars == 60);
        println!("RSI backtest completed: {} trades", report.summary.total_trades);
    }

    #[test]
    fn backtest_divergence_strategy_on_csv() {
        let events = load_csv_from_str(sample_csv_data(), &csv_config()).unwrap();
        let config = BacktestConfig {
            csv_path: "sample_btc_1h.csv".to_string(),
            exchange: ExchangeId::BinanceSpot,
            base: "btc".to_string(),
            quote: "usd".to_string(),
            instrument_kind: MarketDataInstrumentKind::Spot,
            initial_capital: 10000.0,
            strategy_name: "Divergence(14, swing=3)".to_string(),
        };

        let mut strategy = DivergenceStrategy::with_params(14, 3);
        let report = BacktestEngine::run_on_events(&config, &mut strategy, &events);

        report.print_summary();
        println!("Divergence backtest completed: {} trades", report.summary.total_trades);
    }

    #[test]
    fn backtest_rsi_divergence_combo_on_csv() {
        let events = load_csv_from_str(sample_csv_data(), &csv_config()).unwrap();
        let config = BacktestConfig {
            csv_path: "sample_btc_1h.csv".to_string(),
            exchange: ExchangeId::BinanceSpot,
            base: "btc".to_string(),
            quote: "usd".to_string(),
            instrument_kind: MarketDataInstrumentKind::Spot,
            initial_capital: 10000.0,
            strategy_name: "RSI+Divergence(14, 3, 30/70, win=10)".to_string(),
        };

        let mut strategy = RsiDivergenceStrategy::with_params(14, 3, 30.0, 70.0, 10);
        let report = BacktestEngine::run_on_events(&config, &mut strategy, &events);

        report.print_summary();
        println!("RSI+Divergence backtest completed: {} trades", report.summary.total_trades);
    }

    #[test]
    fn backtest_strategy_comparison() {
        let events = load_csv_from_str(sample_csv_data(), &csv_config()).unwrap();

        let strategies: Vec<(&str, Box<dyn SignalGenerator>)> = vec![
            ("RSI(14, 30/70)", Box::new(RsiStrategy::new())),
            ("Divergence(14, 3)", Box::new(DivergenceStrategy::with_params(14, 3))),
            ("RSI+Div(14, 3, 30/70)", Box::new(RsiDivergenceStrategy::with_params(14, 3, 30.0, 70.0, 10))),
        ];

        println!();
        println!("{}", "=".repeat(62));
        println!("{:^62}", "STRATEGY COMPARISON — BTC-USD 1H (60 bars)");
        println!("{}", "=".repeat(62));

        for (name, mut strat) in strategies {
            let config = BacktestConfig {
                csv_path: "sample_btc_1h.csv".to_string(),
                exchange: ExchangeId::BinanceSpot,
                base: "btc".to_string(),
                quote: "usd".to_string(),
                instrument_kind: MarketDataInstrumentKind::Spot,
                initial_capital: 10000.0,
                strategy_name: name.to_string(),
            };
            let report = BacktestEngine::run_on_events(&config, strat.as_mut(), &events);
            report.print_summary();
        }
    }

    #[test]
    fn report_serializes_to_json() {
        let events = load_csv_from_str(sample_csv_data(), &csv_config()).unwrap();
        let config = BacktestConfig {
            csv_path: "sample_btc_1h.csv".to_string(),
            exchange: ExchangeId::BinanceSpot,
            base: "btc".to_string(),
            quote: "usd".to_string(),
            instrument_kind: MarketDataInstrumentKind::Spot,
            initial_capital: 10000.0,
            strategy_name: "RSI(14)".to_string(),
        };

        let mut strategy = RsiStrategy::new();
        let report = BacktestEngine::run_on_events(&config, &mut strategy, &events);
        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("strategy_name"));
        assert!(json.contains("total_trades"));
        println!("Report JSON length: {} bytes", json.len());
        // Print first 500 chars as preview
        println!("{}", &json[..json.len().min(500)]);
    }

    // -------------------------------------------------------------------
    // CoinMarketCap real data tests
    // -------------------------------------------------------------------

    const CMC_CSV_PATH: &str = "data/Bitcoin_15_02_2026-16_05_2026_historical_data_coinmarketcap.csv";

    fn cmc_config(strategy_name: &str) -> BacktestConfig {
        BacktestConfig::crypto_spot(
            CMC_CSV_PATH,
            ExchangeId::Coinbase,
            "btc",
            "usd",
            10000.0,
            strategy_name,
        )
    }

    #[test]
    fn coinmarketcap_adapter_parses_real_file() {
        use super::csv_adapters;
        use super::csv_source::CsvSourceConfig;

        let csv_cfg = CsvSourceConfig::crypto_spot(
            CMC_CSV_PATH,
            ExchangeId::Coinbase,
            "btc",
            "usd",
        );
        let events = csv_adapters::load_adaptive(CMC_CSV_PATH, &csv_cfg).unwrap();

        assert!(events.len() > 100, "Expected 100+ monthly bars, got {}", events.len());
        println!("\nCoinMarketCap CSV parsed: {} monthly bars", events.len());

        // Should be sorted chronologically — first bar should be older
        if let (DataKind::Candle(first), DataKind::Candle(last)) =
            (&events[0].kind, &events[events.len() - 1].kind)
        {
            println!("  First bar: close = {:.2} ({})", first.close, events[0].time_exchange);
            println!("  Last bar:  close = {:.2} ({})", last.close, events[events.len()-1].time_exchange);
            assert!(events[0].time_exchange < events[events.len()-1].time_exchange,
                "Data should be sorted chronologically (oldest first)");
        }
    }

    #[test]
    fn backtest_rsi_on_real_btc_data() {
        use super::csv_adapters;
        use super::csv_source::CsvSourceConfig;

        let csv_cfg = CsvSourceConfig::crypto_spot(CMC_CSV_PATH, ExchangeId::Coinbase, "btc", "usd");
        let events = csv_adapters::load_adaptive(CMC_CSV_PATH, &csv_cfg).unwrap();

        let config = cmc_config("RSI(14, 30/70) — BTC Monthly 2013-2026");
        let mut strategy = RsiStrategy::new();
        let report = BacktestEngine::run_on_events(&config, &mut strategy, &events);
        report.print_summary();

        assert!(report.summary.total_bars > 100);
        println!("  RSI on real BTC: {} trades, {:.2}% P&L",
            report.summary.total_trades, report.summary.total_pnl_pct);
    }

    #[test]
    fn backtest_all_strategies_on_real_btc_data() {
        use super::csv_adapters;
        use super::csv_source::CsvSourceConfig;

        let csv_cfg = CsvSourceConfig::crypto_spot(CMC_CSV_PATH, ExchangeId::Coinbase, "btc", "usd");
        let events = csv_adapters::load_adaptive(CMC_CSV_PATH, &csv_cfg).unwrap();

        println!();
        println!("{}", "=".repeat(62));
        println!("{:^62}", "BTC-USD MONTHLY — REAL DATA (2013-2026)");
        println!("{:^62}", format!("{} bars from CoinMarketCap", events.len()));
        println!("{}", "=".repeat(62));

        let strategies: Vec<(&str, Box<dyn SignalGenerator>)> = vec![
            ("RSI(14, 30/70)", Box::new(RsiStrategy::new())),
            ("RSI(14, 35/65)", Box::new(RsiStrategy::with_params(14, 35.0, 65.0))),
            ("Divergence(14, 3)", Box::new(DivergenceStrategy::with_params(14, 3))),
            ("Divergence(14, 5)", Box::new(DivergenceStrategy::with_params(14, 5))),
            ("RSI+Div(14, 3, 30/70, w=10)", Box::new(RsiDivergenceStrategy::with_params(14, 3, 30.0, 70.0, 10))),
            ("RSI+Div(14, 3, 35/65, w=15)", Box::new(RsiDivergenceStrategy::with_params(14, 3, 35.0, 65.0, 15))),
        ];

        for (name, mut strat) in strategies {
            let config = cmc_config(name);
            let report = BacktestEngine::run_on_events(&config, strat.as_mut(), &events);
            report.print_summary();
        }
    }
}
