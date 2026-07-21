//! Parameter-grid tuner for [`LiquiditySweepStrategy`].
//!
//! Loads the 1-minute dataset once, runs every parameter combination through
//! the same fee/slippage/risk pipeline as backtest_all, and prints the
//! survivors ranked by net PnL.  Each row also shows PnL split across the
//! first and second halves of the dataset (a down-market and a recovery,
//! respectively) — a combo is only trustworthy if it doesn't owe its result
//! to a single regime.
//!
//! Usage:
//!   cargo run --release --bin sweep_tune

use barter_data::event::{DataKind, MarketEvent};
use barter_instrument::exchange::ExchangeId;

use trading_bot_1::backtest::{
    csv_adapters,
    csv_source::CsvSourceConfig,
    paper_executor::PaperExecutor,
    report::BacktestReport,
};
use trading_bot_1::risk_manager::RiskControls;
use trading_bot_1::strategy_engine::{
    SignalGenerator,
    live::{LiquiditySweepStrategy, RegimeDetector, SweepStyle},
};

const INITIAL_CAPITAL: f64 = 10_000.0;
const FEE_PCT_PER_SIDE: f64 = 0.10;
const SLIPPAGE_PCT_PER_SIDE: f64 = 0.05;
const GLOB_PATTERN: &str = "data/BTCUSD-1m-*.csv";

#[derive(Debug, Clone)]
struct Combo {
    lookbacks: Vec<usize>,
    age: usize,
    sweep_min: f64,
    vol_mult: f64,
    min_edge: f64,
    close_loc: f64,
    confirm: usize,
    inval_buffer: f64,
    cooldown: usize,
    target_frac: f64,
    timeout: usize,
    style: SweepStyle,
    regime: bool,
    shorts: bool,
}

impl Combo {
    fn build(&self) -> LiquiditySweepStrategy {
        let mut s = LiquiditySweepStrategy::with_params(
            self.lookbacks[0],
            self.age,
            self.sweep_min,
            self.vol_mult,
            self.min_edge,
        )
        .with_lookbacks(&self.lookbacks)
        .with_close_location(self.close_loc)
        .with_confirmation(self.confirm)
        .with_invalidation_buffer(self.inval_buffer)
        .with_cooldown(self.cooldown)
        .with_target_frac(self.target_frac)
        .with_thesis_timeout(self.timeout)
        .with_style(self.style);
        if self.regime {
            s = s.with_regime_filter(RegimeDetector::new(20, 100, 0.3).with_slow_early_seed(20));
        }
        if !self.shorts {
            s = s.with_longs_only();
        }
        s
    }

    fn label(&self) -> String {
        let lbs: Vec<String> = self.lookbacks.iter().map(|l| l.to_string()).collect();
        let style = match self.style {
            SweepStyle::Fade => "F",
            SweepStyle::Breakout => "B",
            SweepStyle::Both => "FB",
        };
        format!(
            "{} lb{} age{} sw{:.2} v{:.1} e{:.1} cl{:.1} cf{} tf{:.1} rg{}",
            style,
            lbs.join("/"),
            self.age,
            self.sweep_min,
            self.vol_mult,
            self.min_edge,
            self.close_loc,
            self.confirm,
            self.target_frac,
            self.regime as u8,
        )
    }
}

/// Same event loop as backtest_all::run_strategy.
fn run(events: &[MarketEvent], strategy: &mut dyn SignalGenerator, risk: RiskControls) -> BacktestReport {
    let mut executor = PaperExecutor::with_risk(INITIAL_CAPITAL, risk)
        .with_fees(FEE_PCT_PER_SIDE)
        .with_slippage(SLIPPAGE_PCT_PER_SIDE);

    for (i, event) in events.iter().enumerate() {
        let price = match &event.kind {
            DataKind::Candle(c) => c.close,
            DataKind::Trade(t) => t.price,
            _ => continue,
        };
        executor.check_risk_exit(i, price, event.time_exchange);
        if let Some(signal) = strategy.generate_signal(event) {
            executor.process_signal(i, price, &signal);
        }
        executor.sample_equity(price);
    }
    if executor.is_in_position() {
        if let Some(last) = events.last() {
            let price = match &last.kind {
                DataKind::Candle(c) => c.close,
                _ => 0.0,
            };
            executor.force_close(events.len() - 1, price, last.time_exchange);
        }
    }
    BacktestReport::from_executor(&executor, "tune", GLOB_PATTERN, events.len())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let csv_cfg = CsvSourceConfig::crypto_spot(GLOB_PATTERN, ExchangeId::BinanceSpot, "btc", "usd");
    let events = csv_adapters::load_adaptive_glob(GLOB_PATTERN, &csv_cfg)?;
    println!("Loaded {} bars", events.len());

    // Daily-level sweeps revert over hours, not minutes: wider stops,
    // longer max hold, trailing armed only after a real move.
    let risk = RiskControls {
        hard_stop_pct: Some(2.0),
        trailing_stop_pct: Some(1.5),
        trailing_arm_pct: Some(2.0),
        max_hold_bars: Some(1440),
    };

    // Round 8: daily-level fades survived the 29-month test on a plateau
    // (+2.87%, PF 1.75, both halves positive).  Search the honest frequency
    // levers at this scale: regime gate on/off, multiple daily-ish horizons
    // (12h/1d/2d liquidity pools), pierce depth, rejection looseness, and
    // target placement.
    let mut combos = Vec::new();
    let lookback_sets: Vec<Vec<usize>> = vec![vec![1440], vec![2880, 1440, 720]];
    for lookbacks in &lookback_sets {
        for &sweep_min in &[0.10f64, 0.15] {
            for &close_loc in &[0.4f64, 0.5] {
                for &target_frac in &[0.5f64, 0.75] {
                    for &regime in &[false, true] {
                        combos.push(Combo {
                            lookbacks: lookbacks.clone(),
                            age: 60,
                            sweep_min,
                            vol_mult: 1.5,
                            min_edge: 0.8,
                            close_loc,
                            confirm: 1,
                            inval_buffer: 0.30,
                            cooldown: 60,
                            target_frac,
                            timeout: 1440,
                            style: SweepStyle::Fade,
                            regime,
                            shorts: true,
                        });
                    }
                }
            }
        }
    }
    println!("Grid size: {} combos", combos.len());

    let half = events.len() / 2;
    let mut rows = Vec::new();

    for (n, combo) in combos.iter().enumerate() {
        let full = run(&events, &mut combo.build(), risk.clone());
        // Halves only for combos that are at least breakeven overall.
        let (h1, h2) = if full.summary.total_pnl_pct > 0.0 {
            let h1 = run(&events[..half], &mut combo.build(), risk.clone());
            let h2 = run(&events[half..], &mut combo.build(), risk.clone());
            (h1.summary.total_pnl_pct, h2.summary.total_pnl_pct)
        } else {
            (f64::NAN, f64::NAN)
        };
        rows.push((combo.clone(), full, h1, h2));
        if (n + 1) % 50 == 0 {
            println!("  ... {}/{}", n + 1, combos.len());
        }
    }

    rows.sort_by(|a, b| {
        b.1.summary
            .total_pnl_pct
            .partial_cmp(&a.1.summary.total_pnl_pct)
            .unwrap()
    });

    println!();
    println!(
        "{:<52} {:>6} {:>7} {:>8} {:>7} {:>7} {:>8} {:>8}",
        "Combo", "Trades", "Win%", "PnL%", "DD%", "PF", "H1 PnL%", "H2 PnL%",
    );
    println!("{}", "─".repeat(110));
    for (combo, report, h1, h2) in rows.iter().take(40) {
        let s = &report.summary;
        println!(
            "{:<52} {:>6} {:>7.1} {:>8.2} {:>7.2} {:>7.2} {:>8.2} {:>8.2}",
            combo.label(),
            s.total_trades,
            s.win_rate_pct,
            s.total_pnl_pct,
            s.max_drawdown_pct,
            s.profit_factor,
            h1,
            h2,
        );
    }

    Ok(())
}
