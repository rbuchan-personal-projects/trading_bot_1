//! Report generator — writes JSON backtest reports to disk.

use std::fs;
use std::path::Path;

use chrono::Utc;
use serde::Serialize;

use super::paper_executor::{PaperExecutor, TradeRecord};

/// Summary section of the backtest report.
#[derive(Debug, Clone, Serialize)]
pub struct BacktestSummary {
    pub strategy_name: String,
    pub data_source: String,
    pub initial_capital: f64,
    pub final_equity: f64,
    pub total_bars: usize,
    pub total_trades: usize,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub win_rate_pct: f64,
    pub total_pnl_pct: f64,
    pub average_pnl_pct: f64,
    pub best_trade_pct: f64,
    pub worst_trade_pct: f64,
    pub max_drawdown_pct: f64,
    pub sharpe_ratio: f64,
    pub profit_factor: f64,
    pub generated_at: String,
}

/// Full backtest report with summary + individual trades.
#[derive(Debug, Clone, Serialize)]
pub struct BacktestReport {
    pub summary: BacktestSummary,
    pub trades: Vec<TradeRecord>,
}

impl BacktestReport {
    /// Build a report from a completed PaperExecutor.
    pub fn from_executor(
        executor: &PaperExecutor,
        strategy_name: &str,
        data_source: &str,
        total_bars: usize,
    ) -> Self {
        let summary = BacktestSummary {
            strategy_name: strategy_name.to_string(),
            data_source: data_source.to_string(),
            initial_capital: executor.initial_capital,
            final_equity: executor.equity,
            total_bars,
            total_trades: executor.total_trades(),
            winning_trades: executor.winning_trades(),
            losing_trades: executor.losing_trades(),
            win_rate_pct: executor.win_rate(),
            total_pnl_pct: executor.total_pnl_pct(),
            average_pnl_pct: executor.average_pnl_pct(),
            best_trade_pct: if executor.trades.is_empty() { 0.0 } else { executor.best_trade_pct() },
            worst_trade_pct: if executor.trades.is_empty() { 0.0 } else { executor.worst_trade_pct() },
            max_drawdown_pct: executor.max_drawdown_pct,
            sharpe_ratio: executor.sharpe_ratio(),
            profit_factor: executor.profit_factor(),
            generated_at: Utc::now().to_rfc3339(),
        };

        Self {
            summary,
            trades: executor.trades.clone(),
        }
    }

    /// Write the report as pretty-printed JSON to a file.
    pub fn write_json(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let dir = Path::new(path).parent();
        if let Some(d) = dir {
            fs::create_dir_all(d)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, &json)?;
        Ok(())
    }

    /// Write trade log as CSV.
    pub fn write_trades_csv(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let dir = Path::new(path).parent();
        if let Some(d) = dir {
            fs::create_dir_all(d)?;
        }
        let mut wtr = csv::Writer::from_path(path)?;
        // Header
        wtr.write_record(&[
            "entry_bar", "exit_bar", "entry_time", "exit_time",
            "entry_price", "exit_price", "pnl_pct",
            "entry_decision", "exit_decision", "entry_strength", "regime",
        ])?;
        for t in &self.trades {
            wtr.write_record(&[
                t.entry_bar.to_string(),
                t.exit_bar.to_string(),
                t.entry_time.to_rfc3339(),
                t.exit_time.to_rfc3339(),
                format!("{:.2}", t.entry_price),
                format!("{:.2}", t.exit_price),
                format!("{:.4}", t.pnl_pct),
                format!("{:?}", t.entry_decision),
                format!("{:?}", t.exit_decision),
                t.entry_strength.to_string(),
                t.regime.clone(),
            ])?;
        }
        wtr.flush()?;
        Ok(())
    }

    /// Pretty-print the summary to stdout.
    pub fn print_summary(&self) {
        let s = &self.summary;
        let divider = "\u{2500}".repeat(58);
        println!();
        println!("\u{250c}{}\u{2510}", divider);
        println!("\u{2502}{:^58}\u{2502}", format!("\u{1f4ca} Backtest: {}", s.strategy_name));
        println!("\u{251c}{}\u{2524}", divider);
        println!("\u{2502}  Data source           : {:>30} \u{2502}", s.data_source);
        println!("\u{2502}  Total bars            : {:>30} \u{2502}", s.total_bars);
        println!("\u{2502}  Initial capital       : {:>28.2}$ \u{2502}", s.initial_capital);
        println!("\u{2502}  Final equity          : {:>28.2}$ \u{2502}", s.final_equity);
        println!("\u{251c}{}\u{2524}", divider);
        println!("\u{2502}  Total trades          : {:>30} \u{2502}", s.total_trades);
        println!("\u{2502}  Wins / Losses         : {:>20} / {:<7} \u{2502}", s.winning_trades, s.losing_trades);
        println!("\u{2502}  Win rate              : {:>29.1}% \u{2502}", s.win_rate_pct);
        println!("\u{2502}  Total P&L             : {:>29.2}% \u{2502}", s.total_pnl_pct);
        println!("\u{2502}  Average trade P&L     : {:>29.2}% \u{2502}", s.average_pnl_pct);
        println!("\u{2502}  Best trade            : {:>29.2}% \u{2502}", s.best_trade_pct);
        println!("\u{2502}  Worst trade           : {:>29.2}% \u{2502}", s.worst_trade_pct);
        println!("\u{251c}{}\u{2524}", divider);
        println!("\u{2502}  Max drawdown          : {:>29.2}% \u{2502}", s.max_drawdown_pct);
        println!("\u{2502}  Sharpe ratio          : {:>30.2} \u{2502}", s.sharpe_ratio);
        println!("\u{2502}  Profit factor         : {:>30.2} \u{2502}", s.profit_factor);
        println!("\u{2514}{}\u{2518}", divider);
    }
}
