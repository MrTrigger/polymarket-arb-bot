//! CSV-based backtest to compare with Python implementation.
//!
//! Usage:
//!   cargo run --example csv_backtest -- --data analysis/historical_data/backtest_data.csv
//!
//! This reads the same CSV data used by the Python backtest and runs the
//! Rust directional strategy against it for comparison.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

use clap::Parser;
use csv::ReaderBuilder;
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;

// Import the actual Rust signal module
use poly_bot::strategy::signal::get_signal as rust_get_signal;

/// Command line arguments.
#[derive(Parser, Debug)]
#[command(name = "csv_backtest")]
#[command(about = "Run backtest using CSV data from Python analysis")]
struct Args {
    /// Path to backtest_data.csv
    #[arg(long, default_value = "analysis/historical_data/backtest_data.csv")]
    data: PathBuf,

    /// Budget per market in USD
    #[arg(long, default_value = "1000")]
    budget: f64,

    /// Signal mode: "rust" (use poly_bot signal module), "python" (Python-compatible inline)
    #[arg(long, default_value = "rust")]
    mode: String,
}

/// Row from backtest_data.csv
#[derive(Debug, Deserialize)]
struct BacktestRow {
    timestamp: String,
    market_id: String,
    btc_price: f64,
    #[allow(dead_code)]
    btc_high: f64,
    #[allow(dead_code)]
    btc_low: f64,
    strike_price: f64,
    distance_to_strike: f64,
    #[allow(dead_code)]
    seconds_remaining: f64,
    minutes_remaining: f64,
    winner: String,
}

/// Signal enum matching Rust implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    StrongUp,
    LeanUp,
    Neutral,
    LeanDown,
    StrongDown,
}

impl Signal {
    fn up_ratio(&self) -> Decimal {
        match self {
            Signal::StrongUp => dec!(0.78),
            Signal::LeanUp => dec!(0.60),
            Signal::Neutral => dec!(0.50),
            Signal::LeanDown => dec!(0.40),
            Signal::StrongDown => dec!(0.22),
        }
    }

    fn down_ratio(&self) -> Decimal {
        Decimal::ONE - self.up_ratio()
    }
}

/// Get signal using PYTHON thresholds (absolute USD distance).
fn get_signal_python(distance: f64, mins_remaining: f64) -> Signal {
    let (lean, conviction) = if mins_remaining > 12.0 {
        (30.0, 60.0)
    } else if mins_remaining > 9.0 {
        (25.0, 50.0)
    } else if mins_remaining > 6.0 {
        (15.0, 40.0)
    } else if mins_remaining > 3.0 {
        (12.0, 35.0)
    } else {
        (8.0, 25.0)
    };

    if distance > conviction {
        Signal::StrongUp
    } else if distance > lean {
        Signal::LeanUp
    } else if distance < -conviction {
        Signal::StrongDown
    } else if distance < -lean {
        Signal::LeanDown
    } else {
        Signal::Neutral
    }
}

/// Calculate confidence multiplier (Python-compatible).
fn calculate_confidence(distance: f64, mins_remaining: f64) -> f64 {
    let time_conf: f64 = if mins_remaining > 12.0 {
        0.6
    } else if mins_remaining > 9.0 {
        0.8
    } else if mins_remaining > 6.0 {
        1.0
    } else if mins_remaining > 3.0 {
        1.3
    } else {
        1.6
    };

    let abs_dist = distance.abs();
    let dist_conf: f64 = if abs_dist > 100.0 {
        1.5
    } else if abs_dist > 50.0 {
        1.3
    } else if abs_dist > 30.0 {
        1.1
    } else if abs_dist > 20.0 {
        1.0
    } else if abs_dist > 10.0 {
        0.8
    } else {
        0.6
    };

    // Reduce confidence when close to strike
    let adjusted_time: f64 = if abs_dist < 20.0 {
        time_conf * 0.5
    } else if abs_dist < 50.0 {
        time_conf * 0.8
    } else {
        time_conf
    };

    (adjusted_time * dist_conf).sqrt()
}

/// Market backtest state.
struct MarketState {
    up_shares: Decimal,
    down_shares: Decimal,
    total_cost: Decimal,
    trades: u32,
    winner: String,
    strike: Decimal,
}

/// Backtest results for a single market.
#[derive(Debug)]
#[allow(dead_code)]
struct MarketResult {
    market_id: String,
    strike_price: Decimal,
    winner: String,
    trades: u32,
    up_shares: Decimal,
    down_shares: Decimal,
    total_cost: Decimal,
    payout: Decimal,
    pnl: Decimal,
    roi: Decimal,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    println!("============================================================");
    println!("CSV BACKTEST - RUST VS PYTHON COMPARISON");
    println!("============================================================");
    println!("Data file: {:?}", args.data);
    println!("Budget per market: ${:.2}", args.budget);
    println!(
        "Signal mode: {}",
        match args.mode.as_str() {
            "rust" => "Rust (poly_bot::strategy::signal module)",
            "python" => "Python-compatible (inline thresholds)",
            _ => "Unknown",
        }
    );
    println!();

    // Read CSV
    let file = File::open(&args.data)?;
    let reader = BufReader::new(file);
    let mut csv_reader = ReaderBuilder::new().has_headers(true).from_reader(reader);

    // Group data by market
    let mut markets: HashMap<String, Vec<BacktestRow>> = HashMap::new();

    for result in csv_reader.deserialize() {
        let row: BacktestRow = result?;
        markets
            .entry(row.market_id.clone())
            .or_default()
            .push(row);
    }

    println!("Loaded {} markets from CSV", markets.len());

    // Run backtest
    let budget = Decimal::from_f64(args.budget).unwrap_or(dec!(1000));
    let base_size = budget / dec!(200);
    let mut results: Vec<MarketResult> = Vec::new();

    // Track signal distribution
    let mut signal_counts: HashMap<String, u32> = HashMap::new();

    for (market_id, mut rows) in markets {
        // Sort by timestamp
        rows.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

        if rows.len() < 5 {
            continue;
        }

        let winner = rows[0].winner.clone();
        let strike = Decimal::from_f64(rows[0].strike_price).unwrap_or(Decimal::ZERO);

        let mut state = MarketState {
            up_shares: Decimal::ZERO,
            down_shares: Decimal::ZERO,
            total_cost: Decimal::ZERO,
            trades: 0,
            winner: winner.clone(),
            strike,
        };

        for row in &rows {
            // Stop if budget exhausted
            if state.total_cost >= budget * dec!(0.9) {
                break;
            }

            // Get signal and ratios based on mode
            let (signal_name, up_ratio, down_ratio) = if args.mode == "rust" {
                // Use the actual Rust signal module
                let rust_signal = rust_get_signal(
                    Decimal::from_f64(row.btc_price).unwrap_or(Decimal::ZERO),
                    Decimal::from_f64(row.strike_price).unwrap_or(Decimal::ZERO),
                    Decimal::from_f64(row.minutes_remaining).unwrap_or(Decimal::ZERO),
                );
                let name = format!("{}", rust_signal);
                let up = rust_signal.up_ratio();
                let down = rust_signal.down_ratio();
                (name, up, down)
            } else {
                // Use Python-compatible inline thresholds
                let signal = get_signal_python(row.distance_to_strike, row.minutes_remaining);
                let name = format!("{:?}", signal);
                let up = signal.up_ratio();
                let down = signal.down_ratio();
                (name, up, down)
            };

            // Track signal distribution
            *signal_counts.entry(signal_name).or_insert(0) += 1;

            let confidence = calculate_confidence(row.distance_to_strike, row.minutes_remaining);
            let size_mult = confidence.clamp(0.5, 2.0);
            let trade_size =
                Decimal::from_f64(base_size.to_f64().unwrap_or(5.0) * size_mult).unwrap_or(base_size);
            let trade_size = trade_size.min(budget - state.total_cost);

            // Assume 50/50 pricing (conservative)
            let up_price = dec!(0.50);
            let down_price = dec!(0.50);

            let up_cost = trade_size * up_ratio;
            let down_cost = trade_size * down_ratio;

            state.up_shares += up_cost / up_price;
            state.down_shares += down_cost / down_price;
            state.total_cost += up_cost + down_cost;
            state.trades += 1;
        }

        // Calculate P&L
        let payout = if state.winner.to_lowercase() == "up" {
            state.up_shares
        } else {
            state.down_shares
        };

        let pnl = payout - state.total_cost;
        let roi = if state.total_cost > Decimal::ZERO {
            (pnl / state.total_cost) * dec!(100)
        } else {
            Decimal::ZERO
        };

        results.push(MarketResult {
            market_id,
            strike_price: state.strike,
            winner: state.winner,
            trades: state.trades,
            up_shares: state.up_shares,
            down_shares: state.down_shares,
            total_cost: state.total_cost,
            payout,
            pnl,
            roi,
        });
    }

    // Print signal distribution
    println!("\n--- SIGNAL DISTRIBUTION ---");
    for (signal, count) in &signal_counts {
        println!("  {}: {} ({:.1}%)", signal, count, *count as f64 / signal_counts.values().sum::<u32>() as f64 * 100.0);
    }

    // Print summary
    println!("\n============================================================");
    println!("BACKTEST RESULTS");
    println!("============================================================");

    let total_markets = results.len();
    let profitable_markets = results.iter().filter(|r| r.pnl > Decimal::ZERO).count();
    let win_rate = profitable_markets as f64 / total_markets as f64 * 100.0;

    let total_cost: Decimal = results.iter().map(|r| r.total_cost).sum();
    let total_payout: Decimal = results.iter().map(|r| r.payout).sum();
    let total_pnl: Decimal = results.iter().map(|r| r.pnl).sum();
    let overall_roi = if total_cost > Decimal::ZERO {
        (total_pnl / total_cost) * dec!(100)
    } else {
        Decimal::ZERO
    };

    println!("\nMarkets tested: {}", total_markets);
    println!(
        "Markets profitable: {} ({:.1}%)",
        profitable_markets, win_rate
    );

    println!("\nTotal invested: ${:.2}", total_cost);
    println!("Total payout: ${:.2}", total_payout);
    println!("Total P&L: ${:.2}", total_pnl);
    println!("Overall ROI: {:.2}%", overall_roi);

    // Per-market stats
    let pnls: Vec<Decimal> = results.iter().map(|r| r.pnl).collect();
    let avg_pnl = total_pnl / Decimal::from(total_markets as i64);
    let mut sorted_pnls = pnls.clone();
    sorted_pnls.sort();
    let median_pnl = sorted_pnls[sorted_pnls.len() / 2];
    let max_pnl = sorted_pnls.last().copied().unwrap_or(Decimal::ZERO);
    let min_pnl = sorted_pnls.first().copied().unwrap_or(Decimal::ZERO);

    println!("\nPer-market stats:");
    println!("  Average P&L: ${:.2}", avg_pnl);
    println!("  Median P&L: ${:.2}", median_pnl);
    println!("  Best market: ${:.2}", max_pnl);
    println!("  Worst market: ${:.2}", min_pnl);

    // By outcome
    println!("\nBy Outcome:");
    for outcome in ["Up", "Down"] {
        let subset: Vec<&MarketResult> = results
            .iter()
            .filter(|r| r.winner.to_lowercase() == outcome.to_lowercase())
            .collect();
        if !subset.is_empty() {
            let count = subset.len();
            let wins = subset.iter().filter(|r| r.pnl > Decimal::ZERO).count();
            let win_pct = wins as f64 / count as f64 * 100.0;
            let avg: Decimal = subset.iter().map(|r| r.pnl).sum::<Decimal>() / Decimal::from(count as i64);
            println!(
                "  {}: {} markets, {:.0}% profitable, avg ${:.2}",
                outcome, count, win_pct, avg
            );
        }
    }

    println!("\n============================================================");

    Ok(())
}
