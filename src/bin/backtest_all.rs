//! Run all seven trading strategies against the full historical BTC/USD 1-minute
//! dataset and print a side-by-side comparison table.
//!
//! Usage:
//!   cargo run --bin backtest_all

use barter_data::event::{DataKind, MarketEvent};
use barter_instrument::exchange::ExchangeId;
use chrono::Utc;

use trading_bot_1::backtest::{
    csv_adapters,
    csv_source::CsvSourceConfig,
    paper_executor::PaperExecutor,
    report::BacktestReport,
};
use trading_bot_1::risk_manager::RiskControls;
use trading_bot_1::strategy_engine::{
    SignalGenerator,
    live::{
        BollingerRsiStrategy, DivergenceStrategy, EntryMode, LiquiditySweepStrategy,
        RegimeDetector, RegimeGatedStrategy, RsiDivergenceStrategy, RsiStrategy, TrendFilter,
    },
};

const INITIAL_CAPITAL: f64 = 10_000.0;
/// Binance standard taker fee.
const FEE_PCT_PER_SIDE: f64 = 0.10;
/// Conservative half-spread / market-impact estimate for 1-min bars.
const SLIPPAGE_PCT_PER_SIDE: f64 = 0.05;
const GLOB_PATTERN: &str = "data/BTCUSD-1m-*.csv";

// ---------------------------------------------------------------------------
// Custom event loop — calls check_risk_exit every bar (BacktestEngine::run
// does not do this; we need it to match live_paper.rs behaviour).
// ---------------------------------------------------------------------------

fn run_strategy(
    events: &[MarketEvent],
    strategy: &mut dyn SignalGenerator,
    name: &str,
    risk: RiskControls,
) -> BacktestReport {
    let mut executor = PaperExecutor::with_risk(INITIAL_CAPITAL, risk)
        .with_fees(FEE_PCT_PER_SIDE)
        .with_slippage(SLIPPAGE_PCT_PER_SIDE);

    let total_bars = events.len();

    for (i, event) in events.iter().enumerate() {
        let price = match &event.kind {
            DataKind::Candle(c) => c.close,
            DataKind::Trade(t) => t.price,
            _ => continue,
        };

        // 1. Risk exits evaluated first — mirrors resting stop orders that
        //    fill before new signals are processed.
        executor.check_risk_exit(i, price, event.time_exchange);

        // 2. Strategy signal.
        if let Some(signal) = strategy.generate_signal(event) {
            executor.process_signal(i, price, &signal);
        }

        // 3. Sample mark-to-market equity for drawdown tracking.
        executor.sample_equity(price);
    }

    // Force-close any open position at the final bar's price.
    if executor.is_in_position() {
        if let Some(last) = events.last() {
            let price = match &last.kind {
                DataKind::Candle(c) => c.close,
                _ => 0.0,
            };
            executor.force_close(total_bars - 1, price, last.time_exchange);
        }
    }

    BacktestReport::from_executor(&executor, name, GLOB_PATTERN, total_bars)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║              BACKTEST ALL STRATEGIES                            ║");
    println!("║       BTC/USD 1-minute candles  •  full local dataset          ║");
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!();
    println!(
        "  Fees      : {:.2}%/side (Binance taker)",
        FEE_PCT_PER_SIDE
    );
    println!(
        "  Slippage  : {:.2}%/side (half-spread estimate)",
        SLIPPAGE_PCT_PER_SIDE
    );
    println!(
        "  Round-trip cost: {:.2}% per trade",
        2.0 * (FEE_PCT_PER_SIDE + SLIPPAGE_PCT_PER_SIDE)
    );
    println!("  Risk controls : matching live_paper.rs per-slot config");
    println!("  Initial capital: ${:.0}", INITIAL_CAPITAL);
    println!();

    // -- Load data --------------------------------------------------------------
    println!("Loading historical data from '{}'...", GLOB_PATTERN);
    let csv_cfg = CsvSourceConfig::crypto_spot(
        GLOB_PATTERN,
        ExchangeId::BinanceSpot,
        "btc",
        "usd",
    );
    let events = csv_adapters::load_adaptive_glob(GLOB_PATTERN, &csv_cfg)?;
    println!("  Total 1-minute bars: {}", events.len());

    // -- Market context ---------------------------------------------------------
    if let (Some(first), Some(last)) = (events.first(), events.last()) {
        let (f_close, l_close) = match (&first.kind, &last.kind) {
            (DataKind::Candle(f), DataKind::Candle(l)) => (f.close, l.close),
            _ => (0.0, 0.0),
        };
        let period_pct = (l_close - f_close) / f_close * 100.0;

        println!(
            "  Period     : {} → {}",
            first.time_exchange.format("%Y-%m-%d"),
            last.time_exchange.format("%Y-%m-%d"),
        );
        println!(
            "  BTC/USD    : ${:.0} → ${:.0}  ({:+.1}% net move)",
            f_close, l_close, period_pct,
        );

        let (high, low) = events.iter().filter_map(|e| {
            if let DataKind::Candle(c) = &e.kind { Some((c.high, c.low)) } else { None }
        }).fold((f64::NEG_INFINITY, f64::INFINITY), |(mh, ml), (h, l)| (mh.max(h), ml.min(l)));
        println!(
            "  Range      : ${:.0} high  •  ${:.0} low  •  {:.1}% swing",
            high, low, (high - low) / low * 100.0,
        );
    }

    println!();
    println!("  Monthly price progression:");
    monthly_summary(&events);
    println!();

    // -- Risk presets (mirrors live_paper.rs) -----------------------------------
    let baseline_risk = RiskControls {
        hard_stop_pct: Some(1.0),
        trailing_stop_pct: Some(0.4),
        trailing_arm_pct: Some(0.5),
        max_hold_bars: Some(120),
    };
    let v2_controls = RiskControls {
        hard_stop_pct: Some(1.0),
        trailing_stop_pct: Some(0.5),
        trailing_arm_pct: Some(0.6),
        max_hold_bars: Some(90),
    };
    let regime_risk = RiskControls {
        hard_stop_pct: Some(1.0),
        trailing_stop_pct: Some(0.4),
        trailing_arm_pct: Some(0.5),
        max_hold_bars: Some(90),
    };
    // Daily-level sweep reversions play out over hours: wide hard stop
    // backstopping the strategy's invalidation exit, trailing armed only
    // after a real move, day-scale max hold.
    let sweep_risk = RiskControls {
        hard_stop_pct: Some(2.0),
        trailing_stop_pct: Some(1.5),
        trailing_arm_pct: Some(2.0),
        max_hold_bars: Some(1440),
    };

    // -- Strategy definitions ---------------------------------------------------
    // Strategies 5 & 6 (Divergence, RSI+Divergence) are not currently wired
    // into live_paper.rs; we use baseline_risk so they're fairly comparable.
    type StratEntry = (&'static str, Box<dyn SignalGenerator>, RiskControls);
    let mut strategies: Vec<StratEntry> = vec![
        (
            "v1 RSI(14,30/70)+stop",
            Box::new(RsiStrategy::new()),
            baseline_risk.clone(),
        ),
        (
            "v1 BB+RSI+stop",
            Box::new(BollingerRsiStrategy::new()),
            baseline_risk.clone(),
        ),
        (
            "v2 EMA20+EdgeRebound+stop",
            Box::new(
                RsiStrategy::with_params(14, 30.0, 70.0)
                    .with_entry_mode(EntryMode::EdgeRebound)
                    .with_trend_filter(TrendFilter::new(20)),
            ),
            v2_controls,
        ),
        (
            "v3 Regime(20/100)+RSI+stop",
            Box::new(RegimeGatedStrategy::new(
                Box::new(RsiStrategy::new()),
                RegimeDetector::new(20, 100, 0.3).with_slow_early_seed(20),
            )),
            regime_risk,
        ),
        (
            "Divergence(14,swing=5)",
            Box::new(DivergenceStrategy::with_params(14, 5)),
            baseline_risk.clone(),
        ),
        (
            "RSI+Div(14,5,30/70,w=10)",
            Box::new(RsiDivergenceStrategy::with_params(14, 5, 30.0, 70.0, 10)),
            baseline_risk.clone(),
        ),
        (
            "Sweep(1d,fade,regime)",
            Box::new(LiquiditySweepStrategy::new()),
            sweep_risk,
        ),
    ];

    // -- Run each strategy ------------------------------------------------------
    println!("Running strategies...");
    let mut reports: Vec<BacktestReport> = Vec::new();

    for (name, strategy, risk) in strategies.iter_mut() {
        print!("  {:.<40} ", name);
        let report = run_strategy(&events, strategy.as_mut(), name, risk.clone());
        println!("{} trades", report.summary.total_trades);
        reports.push(report);
    }

    // -- Per-strategy detail boxes ----------------------------------------------
    println!();
    for report in &reports {
        report.print_summary();
    }

    // -- Side-by-side comparison table -----------------------------------------
    print_comparison_table(&reports);

    // -- Save reports to disk ---------------------------------------------------
    let date = Utc::now().format("%Y-%m-%d");
    println!("\nSaving reports...");
    for report in &reports {
        let slug: String = report
            .summary
            .strategy_name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
            .collect();
        let base = format!("backtesting/reports/backtest_{}_{}", date, slug);
        report.write_json(&format!("{}.json", base))?;
        report.write_trades_csv(&format!("{}_trades.csv", base))?;
        println!("  {}.json + _trades.csv", base);
    }

    println!("\nDone.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn monthly_summary(events: &[MarketEvent]) {
    use std::collections::BTreeMap;
    let mut months: BTreeMap<String, (f64, f64)> = BTreeMap::new(); // month → (first_close, last_close)

    for event in events {
        if let DataKind::Candle(c) = &event.kind {
            let key = event.time_exchange.format("%Y-%m").to_string();
            months
                .entry(key)
                .and_modify(|e| e.1 = c.close)
                .or_insert((c.close, c.close));
        }
    }

    for (month, (open, close)) in &months {
        let chg = (close - open) / open * 100.0;
        let bar = if chg > 10.0 {
            "▲▲▲"
        } else if chg > 3.0 {
            "▲▲ "
        } else if chg > 0.0 {
            "▲  "
        } else if chg > -3.0 {
            "▼  "
        } else if chg > -10.0 {
            "▼▼ "
        } else {
            "▼▼▼"
        };
        println!(
            "    {} : ${:>8.0} → ${:>8.0}  {:>+7.1}%  {}",
            month, open, close, chg, bar
        );
    }
}

fn print_comparison_table(reports: &[BacktestReport]) {
    let w = 125;
    println!();
    println!("{}", "═".repeat(w));
    println!("{:^width$}", "STRATEGY COMPARISON (net of fees + slippage)", width = w);
    println!("{}", "═".repeat(w));
    println!(
        "{:<28} {:>6} {:>8} {:>10} {:>8} {:>9} {:>8} {:>8} {:>7} {:>9}",
        "Strategy", "Trades", "Win%", "TotalPnL%", "MaxDD%", "AvgTrd%",
        "Best%", "Worst%", "Sharpe", "ProfFact",
    );
    println!("{}", "─".repeat(w));

    for r in reports {
        let s = &r.summary;
        let pf_str = if s.profit_factor == f64::INFINITY {
            "     inf".to_string()
        } else {
            format!("{:>9.2}", s.profit_factor)
        };
        println!(
            "{:<28} {:>6} {:>8.1} {:>10.2} {:>8.2} {:>9.2} {:>8.2} {:>8.2} {:>7.2} {}",
            s.strategy_name,
            s.total_trades,
            s.win_rate_pct,
            s.total_pnl_pct,
            s.max_drawdown_pct,
            s.average_pnl_pct,
            s.best_trade_pct,
            s.worst_trade_pct,
            s.sharpe_ratio,
            pf_str,
        );
    }
    println!("{}", "═".repeat(w));
}
