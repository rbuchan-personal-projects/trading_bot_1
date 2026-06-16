//! Trading Bot 1 — library crate
//!
//! Re-exports all modules so they can be used by both the default binary
//! (`src/main.rs`) and additional binaries (e.g. `src/bin/live_paper.rs`).

pub mod types;
pub mod market_data;
pub mod strategy_engine;
pub mod risk_manager;
pub mod execution;
pub mod backtest;

