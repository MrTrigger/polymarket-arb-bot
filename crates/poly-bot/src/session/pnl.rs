//! P&L tracking and performance metrics.
//!
//! Tracks realized P&L (from closed positions), unrealized P&L (mark-to-market),
//! and computes performance metrics like win rate, Sharpe ratio, and drawdown.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// A single P&L snapshot for tracking equity curve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlSnapshot {
    /// Timestamp of this snapshot.
    pub timestamp: DateTime<Utc>,
    /// Realized P&L (from closed positions and settlements).
    pub realized_pnl: Decimal,
    /// Unrealized P&L (mark-to-market).
    pub unrealized_pnl: Decimal,
    /// Total P&L (realized + unrealized).
    pub total_pnl: Decimal,
    /// Total fees paid.
    pub fees: Decimal,
    /// Total exposure at this point.
    pub exposure: Decimal,
    /// Equity (starting balance + total P&L).
    pub equity: Decimal,
}

impl PnlSnapshot {
    /// Create a new P&L snapshot.
    pub fn new(
        realized_pnl: Decimal,
        unrealized_pnl: Decimal,
        fees: Decimal,
        exposure: Decimal,
        starting_balance: Decimal,
    ) -> Self {
        let total_pnl = realized_pnl + unrealized_pnl;
        Self {
            timestamp: Utc::now(),
            realized_pnl,
            unrealized_pnl,
            total_pnl,
            fees,
            exposure,
            equity: starting_balance + total_pnl,
        }
    }
}

/// A completed trade record for win/loss tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedTrade {
    /// Trade ID.
    pub trade_id: String,
    /// Market event ID.
    pub event_id: String,
    /// P&L from this trade (after fees).
    pub pnl: Decimal,
    /// Cost of this trade.
    pub cost: Decimal,
    /// Revenue from this trade.
    pub revenue: Decimal,
    /// Fees paid.
    pub fees: Decimal,
    /// Timestamp when trade was closed.
    pub closed_at: DateTime<Utc>,
}

impl CompletedTrade {
    /// Check if this was a winning trade.
    pub fn is_win(&self) -> bool {
        self.pnl > Decimal::ZERO
    }

    /// Return on investment.
    pub fn roi(&self) -> Decimal {
        if self.cost > Decimal::ZERO {
            self.pnl / self.cost
        } else {
            Decimal::ZERO
        }
    }
}

/// P&L tracking and performance metrics.
pub struct PnlTracker {
    /// Starting balance for this session.
    starting_balance: Decimal,
    /// Realized P&L (atomic for hot path access).
    realized_pnl_cents: AtomicI64,
    /// Fees paid (atomic for hot path access).
    fees_cents: AtomicU64,
    /// Historical P&L snapshots.
    snapshots: parking_lot::RwLock<VecDeque<PnlSnapshot>>,
    /// Completed trades for win/loss tracking.
    completed_trades: parking_lot::RwLock<Vec<CompletedTrade>>,
    /// Maximum equity seen (for drawdown calculation).
    max_equity: parking_lot::RwLock<Decimal>,
    /// Maximum drawdown seen.
    max_drawdown: parking_lot::RwLock<Decimal>,
    /// Maximum number of snapshots to keep.
    max_snapshots: usize,
}

impl PnlTracker {
    /// Create a new P&L tracker with starting balance.
    pub fn new(starting_balance: Decimal) -> Self {
        Self {
            starting_balance,
            realized_pnl_cents: AtomicI64::new(0),
            fees_cents: AtomicU64::new(0),
            snapshots: parking_lot::RwLock::new(VecDeque::new()),
            completed_trades: parking_lot::RwLock::new(Vec::new()),
            max_equity: parking_lot::RwLock::new(starting_balance),
            max_drawdown: parking_lot::RwLock::new(Decimal::ZERO),
            max_snapshots: 1000,
        }
    }

    /// Record realized P&L (from a closed position or settlement).
    pub fn record_realized_pnl(&self, pnl: Decimal) {
        let pnl_cents = (pnl * dec!(100)).to_i64().unwrap_or(0);
        self.realized_pnl_cents.fetch_add(pnl_cents, Ordering::SeqCst);
    }

    /// Record fees paid.
    pub fn record_fees(&self, fees: Decimal) {
        let fee_cents = (fees * dec!(100)).to_u64().unwrap_or(0);
        self.fees_cents.fetch_add(fee_cents, Ordering::SeqCst);
    }

    /// Record a completed trade for win/loss tracking.
    pub fn record_completed_trade(&self, trade: CompletedTrade) {
        // Update realized P&L
        self.record_realized_pnl(trade.pnl);
        self.record_fees(trade.fees);

        // Store trade
        let mut trades = self.completed_trades.write();
        trades.push(trade);
    }

    /// Get current realized P&L.
    pub fn realized_pnl(&self) -> Decimal {
        let cents = self.realized_pnl_cents.load(Ordering::SeqCst);
        Decimal::new(cents, 2)
    }

    /// Get total fees paid.
    pub fn fees(&self) -> Decimal {
        let cents = self.fees_cents.load(Ordering::SeqCst) as i64;
        Decimal::new(cents, 2)
    }

    /// Take a P&L snapshot with current unrealized P&L.
    pub fn take_snapshot(&self, unrealized_pnl: Decimal, exposure: Decimal) {
        let snapshot = PnlSnapshot::new(
            self.realized_pnl(),
            unrealized_pnl,
            self.fees(),
            exposure,
            self.starting_balance,
        );

        // Update max equity and drawdown
        {
            let mut max_eq = self.max_equity.write();
            let mut max_dd = self.max_drawdown.write();

            if snapshot.equity > *max_eq {
                *max_eq = snapshot.equity;
            }

            let drawdown = *max_eq - snapshot.equity;
            if drawdown > *max_dd {
                *max_dd = drawdown;
            }
        }

        // Store snapshot
        let mut snapshots = self.snapshots.write();
        snapshots.push_back(snapshot);
        while snapshots.len() > self.max_snapshots {
            snapshots.pop_front();
        }
    }

    /// Get all snapshots.
    pub fn snapshots(&self) -> Vec<PnlSnapshot> {
        self.snapshots.read().iter().cloned().collect()
    }

    /// Get the latest snapshot.
    pub fn latest_snapshot(&self) -> Option<PnlSnapshot> {
        self.snapshots.read().back().cloned()
    }

    /// Get maximum equity seen.
    pub fn max_equity(&self) -> Decimal {
        *self.max_equity.read()
    }

    /// Get maximum drawdown seen.
    pub fn max_drawdown(&self) -> Decimal {
        *self.max_drawdown.read()
    }

    /// Get current drawdown.
    pub fn current_drawdown(&self) -> Decimal {
        let max_eq = *self.max_equity.read();
        let current = self.starting_balance + self.realized_pnl();
        (max_eq - current).max(Decimal::ZERO)
    }

    /// Get current drawdown as percentage.
    pub fn drawdown_pct(&self) -> Decimal {
        let max_eq = *self.max_equity.read();
        if max_eq <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        (self.current_drawdown() / max_eq) * dec!(100)
    }

    /// Calculate win rate (percentage of winning trades).
    pub fn win_rate(&self) -> Decimal {
        let trades = self.completed_trades.read();
        if trades.is_empty() {
            return Decimal::ZERO;
        }
        let wins = trades.iter().filter(|t| t.is_win()).count();
        Decimal::new(wins as i64, 0) / Decimal::new(trades.len() as i64, 0) * dec!(100)
    }

    /// Calculate profit factor (gross profit / gross loss).
    pub fn profit_factor(&self) -> Decimal {
        let trades = self.completed_trades.read();
        let gross_profit: Decimal = trades.iter().filter(|t| t.pnl > Decimal::ZERO).map(|t| t.pnl).sum();
        let gross_loss: Decimal = trades.iter().filter(|t| t.pnl < Decimal::ZERO).map(|t| t.pnl.abs()).sum();

        if gross_loss <= Decimal::ZERO {
            return if gross_profit > Decimal::ZERO {
                dec!(999.99) // Cap at a large value
            } else {
                Decimal::ZERO
            };
        }
        gross_profit / gross_loss
    }

    /// Calculate average trade P&L.
    pub fn avg_trade_pnl(&self) -> Decimal {
        let trades = self.completed_trades.read();
        if trades.is_empty() {
            return Decimal::ZERO;
        }
        let total: Decimal = trades.iter().map(|t| t.pnl).sum();
        total / Decimal::new(trades.len() as i64, 0)
    }

    /// Calculate average winning trade.
    pub fn avg_win(&self) -> Decimal {
        let trades = self.completed_trades.read();
        let wins: Vec<_> = trades.iter().filter(|t| t.is_win()).collect();
        if wins.is_empty() {
            return Decimal::ZERO;
        }
        let total: Decimal = wins.iter().map(|t| t.pnl).sum();
        total / Decimal::new(wins.len() as i64, 0)
    }

    /// Calculate average losing trade.
    pub fn avg_loss(&self) -> Decimal {
        let trades = self.completed_trades.read();
        let losses: Vec<_> = trades.iter().filter(|t| !t.is_win()).collect();
        if losses.is_empty() {
            return Decimal::ZERO;
        }
        let total: Decimal = losses.iter().map(|t| t.pnl).sum();
        total / Decimal::new(losses.len() as i64, 0)
    }

    /// Calculate Sharpe ratio (annualized, assuming 252 trading days).
    ///
    /// Sharpe = (mean return - risk_free_rate) / std_dev_return
    /// For simplicity, we use 0 risk-free rate and daily returns.
    pub fn sharpe_ratio(&self) -> Decimal {
        let snapshots = self.snapshots.read();
        if snapshots.len() < 2 {
            return Decimal::ZERO;
        }

        // Calculate returns between snapshots
        let returns: Vec<f64> = snapshots
            .iter()
            .zip(snapshots.iter().skip(1))
            .filter_map(|(prev, curr)| {
                if prev.equity > Decimal::ZERO {
                    let ret = (curr.equity - prev.equity) / prev.equity;
                    ret.to_f64()
                } else {
                    None
                }
            })
            .collect();

        if returns.is_empty() {
            return Decimal::ZERO;
        }

        let mean: f64 = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance: f64 = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / returns.len() as f64;
        let std_dev = variance.sqrt();

        if std_dev <= 0.0 {
            return Decimal::ZERO;
        }

        // Annualize (assuming snapshots are taken at regular intervals)
        // For 15-minute markets, we might take many snapshots per day
        let annualization_factor = (252.0_f64).sqrt(); // Standard for daily
        let sharpe = (mean / std_dev) * annualization_factor;

        Decimal::try_from(sharpe).unwrap_or(Decimal::ZERO)
    }

    /// Get total trade count.
    pub fn trade_count(&self) -> usize {
        self.completed_trades.read().len()
    }

    /// Get performance summary.
    pub fn summary(&self) -> PnlSummary {
        PnlSummary {
            starting_balance: self.starting_balance,
            realized_pnl: self.realized_pnl(),
            fees: self.fees(),
            net_pnl: self.realized_pnl() - self.fees(),
            max_equity: self.max_equity(),
            max_drawdown: self.max_drawdown(),
            current_drawdown: self.current_drawdown(),
            drawdown_pct: self.drawdown_pct(),
            trade_count: self.trade_count() as u64,
            win_rate: self.win_rate(),
            profit_factor: self.profit_factor(),
            avg_trade_pnl: self.avg_trade_pnl(),
            avg_win: self.avg_win(),
            avg_loss: self.avg_loss(),
            sharpe_ratio: self.sharpe_ratio(),
        }
    }

    /// Reset the tracker (for new session).
    pub fn reset(&self) {
        self.realized_pnl_cents.store(0, Ordering::SeqCst);
        self.fees_cents.store(0, Ordering::SeqCst);
        self.snapshots.write().clear();
        self.completed_trades.write().clear();
        *self.max_equity.write() = self.starting_balance;
        *self.max_drawdown.write() = Decimal::ZERO;
    }
}

impl Default for PnlTracker {
    fn default() -> Self {
        Self::new(Decimal::ZERO)
    }
}

/// Summary of P&L performance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlSummary {
    pub starting_balance: Decimal,
    pub realized_pnl: Decimal,
    pub fees: Decimal,
    pub net_pnl: Decimal,
    pub max_equity: Decimal,
    pub max_drawdown: Decimal,
    pub current_drawdown: Decimal,
    pub drawdown_pct: Decimal,
    pub trade_count: u64,
    pub win_rate: Decimal,
    pub profit_factor: Decimal,
    pub avg_trade_pnl: Decimal,
    pub avg_win: Decimal,
    pub avg_loss: Decimal,
    pub sharpe_ratio: Decimal,
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pnl_tracking() {
        let tracker = PnlTracker::new(dec!(1000));

        // Record some P&L
        tracker.record_realized_pnl(dec!(50));
        tracker.record_realized_pnl(dec!(-20));
        tracker.record_fees(dec!(5));

        assert_eq!(tracker.realized_pnl(), dec!(30));
        assert_eq!(tracker.fees(), dec!(5));
    }

    #[test]
    fn test_completed_trades() {
        let tracker = PnlTracker::new(dec!(1000));

        // Record winning trade
        tracker.record_completed_trade(CompletedTrade {
            trade_id: "1".to_string(),
            event_id: "event1".to_string(),
            pnl: dec!(10),
            cost: dec!(100),
            revenue: dec!(110),
            fees: dec!(1),
            closed_at: Utc::now(),
        });

        // Record losing trade
        tracker.record_completed_trade(CompletedTrade {
            trade_id: "2".to_string(),
            event_id: "event2".to_string(),
            pnl: dec!(-5),
            cost: dec!(100),
            revenue: dec!(95),
            fees: dec!(1),
            closed_at: Utc::now(),
        });

        assert_eq!(tracker.trade_count(), 2);
        assert_eq!(tracker.win_rate(), dec!(50)); // 1 win out of 2
        assert_eq!(tracker.realized_pnl(), dec!(5)); // 10 - 5
    }

    #[test]
    fn test_drawdown() {
        let tracker = PnlTracker::new(dec!(1000));

        // Take snapshots with varying equity
        tracker.take_snapshot(dec!(0), dec!(100));  // equity = 1000
        tracker.record_realized_pnl(dec!(100));
        tracker.take_snapshot(dec!(0), dec!(100));  // equity = 1100 (new high)
        tracker.record_realized_pnl(dec!(-50));
        tracker.take_snapshot(dec!(0), dec!(100));  // equity = 1050

        assert_eq!(tracker.max_equity(), dec!(1100));
        assert_eq!(tracker.current_drawdown(), dec!(50));
    }

    #[test]
    fn test_profit_factor() {
        let tracker = PnlTracker::new(dec!(1000));

        // Two wins of $10 each
        tracker.record_completed_trade(CompletedTrade {
            trade_id: "1".to_string(),
            event_id: "event1".to_string(),
            pnl: dec!(10),
            cost: dec!(100),
            revenue: dec!(110),
            fees: dec!(0),
            closed_at: Utc::now(),
        });
        tracker.record_completed_trade(CompletedTrade {
            trade_id: "2".to_string(),
            event_id: "event2".to_string(),
            pnl: dec!(10),
            cost: dec!(100),
            revenue: dec!(110),
            fees: dec!(0),
            closed_at: Utc::now(),
        });

        // One loss of $5
        tracker.record_completed_trade(CompletedTrade {
            trade_id: "3".to_string(),
            event_id: "event3".to_string(),
            pnl: dec!(-5),
            cost: dec!(100),
            revenue: dec!(95),
            fees: dec!(0),
            closed_at: Utc::now(),
        });

        // Profit factor = 20 / 5 = 4.0
        assert_eq!(tracker.profit_factor(), dec!(4));
    }
}
