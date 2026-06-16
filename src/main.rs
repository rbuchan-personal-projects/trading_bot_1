mod types;
mod market_data;
mod strategy_engine;
mod risk_manager;
mod execution;
mod backtest;

use rust_decimal::Decimal;

use types::{MarketTick, TradingSignal};
use market_data::live::LiveMarketProvider;
use execution::live::LiveOrderExecutor;
use risk_manager::live::{LiveRiskEvaluator, RiskParameters};
use strategy_engine::live::SimpleStrategy;
use strategy_engine::StrategyEngine;

#[tokio::main]
async fn main() {
    // --- Demo: shared types still work ---
    let tick = MarketTick {
        symbol: "BTC-USD".to_string(),
        timestamp: "2026-04-29T12:00:00Z".to_string(),
        bid: Decimal::from_str_exact("65000.25").unwrap(),
        ask: Decimal::from_str_exact("65001.10").unwrap(),
        last_price: Decimal::from_str_exact("65000.80").unwrap(),
        volume: Decimal::from_str_exact("12.5").unwrap(),
    };

    let signal = TradingSignal::Hold;

    let tick_json = serde_json::to_string_pretty(&tick).expect("failed to serialize tick");
    let signal_json = serde_json::to_string(&signal).expect("failed to serialize signal");

    println!("MarketTick JSON:\n{tick_json}");
    println!("TradingSignal JSON: {signal_json}");

    // --- Wire up all components with dependency injection ---
    let market = LiveMarketProvider::new();
    let executor = LiveOrderExecutor::new();
    let risk = LiveRiskEvaluator::new(RiskParameters {
        max_position_size: Decimal::from_str_exact("10.0").unwrap(),
        daily_loss_limit: Decimal::from_str_exact("5000.0").unwrap(),
        risk_per_trade_percent: Decimal::from_str_exact("2.0").unwrap(),
        max_leverage: Decimal::from_str_exact("3.0").unwrap(),
    });
    let strategy = SimpleStrategy::new();

    let _engine = StrategyEngine::new(market, executor, risk, Box::new(strategy));

    println!("\n✓ Trading bot initialized with trait-based architecture:");
    println!("  - Market Data   (The Ear)   → LiveMarketProvider");
    println!("  - Strategy      (The Brain) → SimpleStrategy");
    println!("  - Risk Manager  (The Guard) → LiveRiskEvaluator");
    println!("  - Execution     (The Hand)  → LiveOrderExecutor");
    println!("\n  Engine is generic: swap in mocks for backtesting!");
}
