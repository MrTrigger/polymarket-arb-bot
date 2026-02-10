//! Trades log for recording all fills and settlements.
//!
//! Always enabled in all modes. Writes a CSV with enough data to
//! reconstruct every position and calculate all trading statistics.

use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use poly_common::CryptoAsset;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeRecordType {
    Fill,
    Settle,
}

impl std::fmt::Display for TradeRecordType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TradeRecordType::Fill => write!(f, "FILL"),
            TradeRecordType::Settle => write!(f, "SETTLE"),
        }
    }
}

// CSV column indices (after adding balance column)
const COL_TYPE: usize = 2;
const COL_EVENT_ID: usize = 3;
const COL_ASSET: usize = 4;
const COL_COST: usize = 9;
const COL_PNL: usize = 20;

const CSV_HEADER: &str = "timestamp,mode,type,event_id,asset,outcome,shares,price,fee,cost,strike_price,spot_price,signal,confidence,balance,yes_wins,yes_shares_after,no_shares_after,cost_basis_after,payout,realized_pnl\n";

fn csv_row(
    timestamp: &DateTime<Utc>,
    mode: &str,
    record_type: TradeRecordType,
    event_id: &str,
    asset: CryptoAsset,
    outcome: &str,
    shares: Decimal,
    price: Decimal,
    fee: Decimal,
    cost: Decimal,
    strike_price: Decimal,
    spot_price: Decimal,
    signal: &str,
    confidence: Decimal,
    balance: Decimal,
    yes_wins: Option<bool>,
    yes_shares_after: Decimal,
    no_shares_after: Decimal,
    cost_basis_after: Decimal,
    payout: Option<Decimal>,
    realized_pnl: Option<Decimal>,
) -> String {
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
        timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
        mode,
        record_type,
        event_id,
        asset,
        outcome,
        shares,
        price,
        fee,
        cost,
        strike_price,
        spot_price,
        signal,
        confidence,
        balance,
        yes_wins.map(|b| if b { "true" } else { "false" }).unwrap_or(""),
        yes_shares_after,
        no_shares_after,
        cost_basis_after,
        payout.map(|p| p.to_string()).unwrap_or_default(),
        realized_pnl.map(|p| p.to_string()).unwrap_or_default(),
    )
}

/// Logger that writes trades to CSV.
pub struct TradesLogger {
    file: Mutex<File>,
    mode: String,
    path: std::path::PathBuf,
    initial_balance: Decimal,
}

impl TradesLogger {
    /// Create a new trades logger.
    pub fn new(path: impl AsRef<Path>, mode: &str, initial_balance: Decimal) -> std::io::Result<Self> {
        let path = path.as_ref();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file_exists = path.exists() && std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        let logger = Self {
            file: Mutex::new(file),
            mode: mode.to_string(),
            path: path.to_path_buf(),
            initial_balance,
        };

        // Write CSV header if new/empty file
        if !file_exists {
            logger.write_header()?;
        }

        Ok(logger)
    }

    fn write_header(&self) -> std::io::Result<()> {
        let mut file = self.file.lock().unwrap();
        file.write_all(CSV_HEADER.as_bytes())
    }

    /// Log a fill (trade entry).
    #[allow(clippy::too_many_arguments)]
    pub fn log_fill(
        &self,
        timestamp: DateTime<Utc>,
        event_id: &str,
        asset: CryptoAsset,
        outcome: &str,
        shares: Decimal,
        price: Decimal,
        fee: Decimal,
        cost: Decimal,
        strike_price: Decimal,
        spot_price: Decimal,
        signal: &str,
        confidence: Decimal,
        balance: Decimal,
        yes_shares_after: Decimal,
        no_shares_after: Decimal,
        cost_basis_after: Decimal,
    ) {
        let row = csv_row(
            &timestamp, &self.mode, TradeRecordType::Fill,
            event_id, asset, outcome,
            shares, price, fee, cost,
            strike_price, spot_price,
            signal, confidence, balance,
            None,
            yes_shares_after, no_shares_after, cost_basis_after,
            None, None,
        );

        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(row.as_bytes());
        }
    }

    /// Log a settlement (market close).
    #[allow(clippy::too_many_arguments)]
    pub fn log_settlement(
        &self,
        timestamp: DateTime<Utc>,
        event_id: &str,
        asset: CryptoAsset,
        yes_wins: bool,
        strike_price: Decimal,
        final_spot: Decimal,
        yes_shares: Decimal,
        no_shares: Decimal,
        cost_basis: Decimal,
        payout: Decimal,
        realized_pnl: Decimal,
        balance: Decimal,
    ) {
        let row = csv_row(
            &timestamp, &self.mode, TradeRecordType::Settle,
            event_id, asset,
            if yes_wins { "YES_WINS" } else { "NO_WINS" },
            yes_shares + no_shares, Decimal::ZERO, Decimal::ZERO, cost_basis,
            strike_price, final_spot,
            "", Decimal::ZERO, balance,
            Some(yes_wins),
            Decimal::ZERO, Decimal::ZERO, cost_basis,
            Some(payout), Some(realized_pnl),
        );

        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(row.as_bytes());
        }
    }

    /// Write a session summary file based on the trades log.
    ///
    /// Reads back the CSV and computes aggregate statistics including
    /// Sharpe ratio, max drawdown, Sortino ratio, etc.
    pub fn write_session_summary(&self, start_time: DateTime<Utc>, end_time: DateTime<Utc>) {
        // Flush pending writes
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }

        // Read back the trades log
        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read trades log for summary: {}", e);
                return;
            }
        };

        let mut total_fills = 0u32;
        let mut total_settlements = 0u32;
        let mut total_cost = Decimal::ZERO;
        let mut total_pnl = Decimal::ZERO;
        let mut wins = 0u32;
        let mut losses = 0u32;
        let mut gross_profit = Decimal::ZERO;
        let mut gross_loss = Decimal::ZERO;
        let mut assets_traded: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut events_traded: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Track balance at each settlement for drawdown/sharpe
        let initial_bal = self.initial_balance.to_f64().unwrap_or(1000.0);
        let mut balance_series: Vec<f64> = vec![initial_bal];
        let mut pnl_series: Vec<f64> = Vec::new();

        for line in content.lines().skip(1) {
            let cols: Vec<&str> = line.split(',').collect();
            if cols.len() < COL_PNL + 1 { continue; }

            let record_type = cols[COL_TYPE];
            let event_id = cols[COL_EVENT_ID];
            let asset = cols[COL_ASSET];

            match record_type {
                "FILL" => {
                    total_fills += 1;
                    events_traded.insert(event_id.to_string());
                    assets_traded.insert(asset.to_string());
                    if let Ok(cost) = cols[COL_COST].parse::<Decimal>() {
                        total_cost += cost;
                    }
                }
                "SETTLE" => {
                    total_settlements += 1;
                    if let Ok(pnl) = cols[COL_PNL].parse::<Decimal>() {
                        total_pnl += pnl;
                        pnl_series.push(pnl.to_f64().unwrap_or(0.0));
                        if pnl > Decimal::ZERO {
                            wins += 1;
                            gross_profit += pnl;
                        } else if pnl < Decimal::ZERO {
                            losses += 1;
                            gross_loss += pnl.abs();
                        }
                    }
                    // Compute running balance from P&L (executor may not track balance)
                    {
                        let last = *balance_series.last().unwrap_or(&initial_bal);
                        let pnl_val = cols[COL_PNL].parse::<f64>().unwrap_or(0.0);
                        balance_series.push(last + pnl_val);
                    }
                }
                _ => {}
            }
        }

        let final_balance = *balance_series.last().unwrap_or(&self.initial_balance.to_f64().unwrap_or(1000.0));
        let initial = self.initial_balance.to_f64().unwrap_or(1000.0);
        let return_pct = if initial > 0.0 { (final_balance - initial) / initial * 100.0 } else { 0.0 };

        let win_rate = if wins + losses > 0 {
            format!("{:.1}%", Decimal::from(wins) / Decimal::from(wins + losses) * Decimal::ONE_HUNDRED)
        } else {
            "N/A".to_string()
        };

        let profit_factor = if gross_loss > Decimal::ZERO {
            format!("{:.2}", gross_profit / gross_loss)
        } else if gross_profit > Decimal::ZERO {
            "inf".to_string()
        } else {
            "N/A".to_string()
        };

        // Compute risk metrics from PnL series
        let sharpe = compute_sharpe(&pnl_series);
        let sortino = compute_sortino(&pnl_series);
        let (max_dd_pct, max_dd_str) = compute_max_drawdown(&balance_series);
        let calmar = if max_dd_pct > 0.0 && initial > 0.0 {
            Some((final_balance - initial) / initial / (max_dd_pct / 100.0))
        } else {
            None
        };

        // Build summary
        let mut summary = format!(
            "\
Session Summary
===============
Mode:              {}
Start:             {}
End:               {}
Duration:          {}

Performance
-----------
Initial Balance:   ${:.2}
Final Balance:     ${:.2}
Total P&L:         ${:.2}
Return:            {:.2}%

Risk Metrics
------------
Win Rate:          {} ({} wins / {} losses)
Profit Factor:     {}
Gross Profit:      ${:.2}
Gross Loss:        ${:.2}
",
            self.mode,
            start_time.format("%Y-%m-%d %H:%M:%S UTC"),
            end_time.format("%Y-%m-%d %H:%M:%S UTC"),
            format_duration(end_time - start_time),
            self.initial_balance,
            Decimal::from_f64_retain(final_balance).unwrap_or(dec!(0)),
            total_pnl,
            return_pct,
            win_rate, wins, losses,
            profit_factor,
            gross_profit,
            gross_loss,
        );

        // Add Sharpe
        match sharpe {
            Some(s) => summary.push_str(&format!("Sharpe Ratio:      {:.2}\n", s)),
            None => summary.push_str("Sharpe Ratio:      N/A\n"),
        }
        match sortino {
            Some(s) => summary.push_str(&format!("Sortino Ratio:     {:.2}\n", s)),
            None => summary.push_str("Sortino Ratio:     N/A\n"),
        }
        summary.push_str(&format!("Max Drawdown:      {}\n", max_dd_str));
        match calmar {
            Some(c) => summary.push_str(&format!("Calmar Ratio:      {:.2}\n", c)),
            None => summary.push_str("Calmar Ratio:      N/A\n"),
        }

        summary.push_str(&format!(
            "\nTrading Activity\n\
             ----------------\n\
             Fill Events:       {}\n\
             Markets Settled:   {}\n\
             Unique Markets:    {}\n\
             Assets Traded:     {}\n\
             Total Cost:        ${:.2}\n\
             \n\
             Trades Log:        {}\n",
            total_fills,
            total_settlements,
            events_traded.len(),
            {
                let mut sorted: Vec<_> = assets_traded.iter().cloned().collect();
                sorted.sort();
                sorted.join(", ")
            },
            total_cost,
            self.path.display(),
        ));

        // Write summary to sibling file
        let summary_path = self.path.with_file_name(
            format!("session_summary_{}.txt", self.mode)
        );

        match std::fs::write(&summary_path, &summary) {
            Ok(()) => {
                tracing::info!("Session summary written to {}", summary_path.display());
                println!("\n{}", summary);
            }
            Err(e) => {
                tracing::warn!("Failed to write session summary: {}", e);
                println!("\n{}", summary);
            }
        }
    }
}

/// Compute annualized Sharpe ratio from a series of per-trade PnL values.
fn compute_sharpe(pnl: &[f64]) -> Option<f64> {
    if pnl.len() < 2 { return None; }
    let n = pnl.len() as f64;
    let mean = pnl.iter().sum::<f64>() / n;
    let variance = pnl.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let std_dev = variance.sqrt();
    if std_dev < 1e-12 { return None; }
    // Annualize: assume ~96 trades/day (4 assets * 24 windows)
    let trades_per_year: f64 = 96.0 * 365.0;
    Some((mean / std_dev) * (trades_per_year.min(n)).sqrt())
}

/// Compute Sortino ratio (only penalizes downside).
fn compute_sortino(pnl: &[f64]) -> Option<f64> {
    if pnl.len() < 2 { return None; }
    let n = pnl.len() as f64;
    let mean = pnl.iter().sum::<f64>() / n;
    let downside_variance = pnl.iter()
        .filter(|&&x| x < 0.0)
        .map(|x| x.powi(2))
        .sum::<f64>() / n;
    let downside_dev = downside_variance.sqrt();
    if downside_dev < 1e-12 { return None; }
    let trades_per_year: f64 = 96.0 * 365.0;
    Some((mean / downside_dev) * (trades_per_year.min(n)).sqrt())
}

/// Compute max drawdown from balance series. Returns (pct, formatted string).
fn compute_max_drawdown(balances: &[f64]) -> (f64, String) {
    if balances.len() < 2 {
        return (0.0, "N/A".to_string());
    }

    let mut peak = balances[0];
    let mut max_dd = 0.0f64;

    for &bal in &balances[1..] {
        if bal > peak { peak = bal; }
        let dd = (peak - bal) / peak * 100.0;
        if dd > max_dd { max_dd = dd; }
    }

    (max_dd, format!("{:.2}%", max_dd))
}

fn format_duration(d: chrono::Duration) -> String {
    let total_secs = d.num_seconds();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{}h {}m {}s", hours, mins, secs)
    } else if mins > 0 {
        format!("{}m {}s", mins, secs)
    } else {
        format!("{}s", secs)
    }
}
