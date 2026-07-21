//! Diagnostic: run LiquiditySweepStrategy over the dataset and print its
//! pipeline counters — how many candidate sweeps the price/volume gates
//! produce and where the rest of the pipeline consumes them.
//!
//! Usage:
//!   cargo run --release --bin sweep_funnel

use barter_instrument::exchange::ExchangeId;

use trading_bot_1::backtest::{csv_adapters, csv_source::CsvSourceConfig};
use trading_bot_1::strategy_engine::{
    SignalGenerator,
    live::{LiquiditySweepStrategy, RegimeDetector},
};

const GLOB_PATTERN: &str = "data/BTCUSD-1m-*.csv";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let csv_cfg = CsvSourceConfig::crypto_spot(GLOB_PATTERN, ExchangeId::BinanceSpot, "btc", "usd");
    let events = csv_adapters::load_adaptive_glob(GLOB_PATTERN, &csv_cfg)?;

    let variants: Vec<(&str, LiquiditySweepStrategy)> = vec![
        (
            "lb120 age10 v2.0 e0.5 cf1 rg1",
            LiquiditySweepStrategy::with_params(120, 10, 0.05, 2.0, 0.5)
                .with_close_location(0.5)
                .with_confirmation(1)
                .with_invalidation_buffer(0.15)
                .with_cooldown(15)
                .with_regime_filter(RegimeDetector::new(20, 100, 0.3).with_slow_early_seed(20)),
        ),
        (
            "same but rg0",
            LiquiditySweepStrategy::with_params(120, 10, 0.05, 2.0, 0.5)
                .with_close_location(0.5)
                .with_confirmation(1)
                .with_invalidation_buffer(0.15)
                .with_cooldown(15),
        ),
        (
            "same but age0 rg0",
            LiquiditySweepStrategy::with_params(120, 0, 0.05, 2.0, 0.5)
                .with_close_location(0.5)
                .with_confirmation(1)
                .with_invalidation_buffer(0.15)
                .with_cooldown(15),
        ),
    ];

    for (name, mut strategy) in variants {
        let mut signals = 0usize;
        for event in &events {
            if strategy.generate_signal(event).is_some() {
                signals += 1;
            }
        }
        let s = strategy.stats();
        println!("\n{name}  ({} signals emitted)", signals);
        println!("  candidates long/short : {} / {}", s.candidates_long, s.candidates_short);
        println!("  blocked lockout       : {}", s.blocked_lockout);
        println!("  blocked cooldown      : {}", s.blocked_cooldown);
        println!("  blocked regime        : {}", s.blocked_regime);
        println!("  pending set/confirmed : {} / {}", s.pending_set, s.pending_confirmed);
        println!("  pending cancel/expire : {} / {}", s.pending_cancelled, s.pending_expired);
        println!("  entries               : {}", s.entries);
        println!("  exits tgt/inval/tmo   : {} / {} / {}", s.exits_target, s.exits_invalidated, s.exits_timeout);
    }
    Ok(())
}
