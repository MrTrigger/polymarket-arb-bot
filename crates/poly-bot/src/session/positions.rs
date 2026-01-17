//! Consolidated position tracking across all markets.
//!
//! Single source of truth for inventory/positions, replacing the
//! fragmented tracking previously spread across multiple types.

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

use poly_common::types::{CryptoAsset, Outcome};

/// Position state based on YES/NO imbalance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PositionState {
    /// Balanced position (imbalance < 0.2).
    Balanced,
    /// Skewed position (0.2 <= imbalance < 0.4).
    Skewed,
    /// Exposed position (0.4 <= imbalance < 0.6).
    Exposed,
    /// Crisis position (imbalance >= 0.6).
    Crisis,
    /// Flat - no position.
    Flat,
}

impl PositionState {
    /// Get position state from imbalance ratio.
    pub fn from_imbalance(imbalance: Decimal) -> Self {
        if imbalance < dec!(0.01) {
            PositionState::Flat
        } else if imbalance < dec!(0.2) {
            PositionState::Balanced
        } else if imbalance < dec!(0.4) {
            PositionState::Skewed
        } else if imbalance < dec!(0.6) {
            PositionState::Exposed
        } else {
            PositionState::Crisis
        }
    }

    /// Whether this position state suggests rebalancing.
    pub fn should_rebalance(&self) -> bool {
        matches!(self, PositionState::Exposed | PositionState::Crisis)
    }

    /// Size multiplier for this state (reduce size when imbalanced).
    pub fn size_multiplier(&self) -> Decimal {
        match self {
            PositionState::Flat | PositionState::Balanced => dec!(1.0),
            PositionState::Skewed => dec!(0.75),
            PositionState::Exposed => dec!(0.50),
            PositionState::Crisis => dec!(0.25),
        }
    }
}

impl std::fmt::Display for PositionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PositionState::Flat => write!(f, "flat"),
            PositionState::Balanced => write!(f, "balanced"),
            PositionState::Skewed => write!(f, "skewed"),
            PositionState::Exposed => write!(f, "exposed"),
            PositionState::Crisis => write!(f, "crisis"),
        }
    }
}

/// Position in a single market (event).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketPosition {
    /// Market event ID.
    pub event_id: String,
    /// Asset being traded.
    pub asset: CryptoAsset,
    /// YES token ID.
    pub yes_token_id: String,
    /// NO token ID.
    pub no_token_id: String,
    /// YES shares held.
    pub yes_shares: Decimal,
    /// NO shares held.
    pub no_shares: Decimal,
    /// Total cost basis for YES position.
    pub yes_cost_basis: Decimal,
    /// Total cost basis for NO position.
    pub no_cost_basis: Decimal,
    /// Realized P&L from closed portions of this position.
    pub realized_pnl: Decimal,
    /// Fees paid for this position.
    pub fees_paid: Decimal,
    /// Number of trades in this position.
    pub trade_count: u32,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Strike price (for directional reference).
    pub strike_price: Decimal,
}

impl MarketPosition {
    /// Create a new empty position.
    pub fn new(
        event_id: String,
        asset: CryptoAsset,
        yes_token_id: String,
        no_token_id: String,
        strike_price: Decimal,
    ) -> Self {
        Self {
            event_id,
            asset,
            yes_token_id,
            no_token_id,
            yes_shares: Decimal::ZERO,
            no_shares: Decimal::ZERO,
            yes_cost_basis: Decimal::ZERO,
            no_cost_basis: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
            fees_paid: Decimal::ZERO,
            trade_count: 0,
            updated_at: Utc::now(),
            strike_price,
        }
    }

    /// Record a fill on this position.
    pub fn record_fill(
        &mut self,
        outcome: Outcome,
        shares: Decimal,
        cost: Decimal,
        fee: Decimal,
    ) {
        match outcome {
            Outcome::Yes => {
                self.yes_shares += shares;
                self.yes_cost_basis += cost;
            }
            Outcome::No => {
                self.no_shares += shares;
                self.no_cost_basis += cost;
            }
        }
        self.fees_paid += fee;
        self.trade_count += 1;
        self.updated_at = Utc::now();
    }

    /// Total shares held (YES + NO).
    pub fn total_shares(&self) -> Decimal {
        self.yes_shares + self.no_shares
    }

    /// Total cost basis.
    pub fn total_cost(&self) -> Decimal {
        self.yes_cost_basis + self.no_cost_basis
    }

    /// Total exposure (cost basis).
    pub fn total_exposure(&self) -> Decimal {
        self.total_cost()
    }

    /// Calculate YES/NO imbalance ratio (0.0 = perfectly balanced, 1.0 = all one side).
    pub fn imbalance_ratio(&self) -> Decimal {
        let total = self.total_shares();
        if total <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let max_side = self.yes_shares.max(self.no_shares);
        let min_side = self.yes_shares.min(self.no_shares);
        (max_side - min_side) / total
    }

    /// Get the position state based on imbalance.
    pub fn state(&self) -> PositionState {
        PositionState::from_imbalance(self.imbalance_ratio())
    }

    /// Check if position is flat (no shares).
    pub fn is_flat(&self) -> bool {
        self.total_shares() <= Decimal::ZERO
    }

    /// Calculate guaranteed P&L from matched pairs.
    ///
    /// For each matched YES+NO pair, we're guaranteed $1 payout minus cost.
    pub fn guaranteed_pnl(&self) -> Decimal {
        let matched_pairs = self.yes_shares.min(self.no_shares);
        if matched_pairs <= Decimal::ZERO {
            return Decimal::ZERO;
        }

        // Average cost per matched pair
        let yes_avg = if self.yes_shares > Decimal::ZERO {
            self.yes_cost_basis / self.yes_shares
        } else {
            Decimal::ZERO
        };
        let no_avg = if self.no_shares > Decimal::ZERO {
            self.no_cost_basis / self.no_shares
        } else {
            Decimal::ZERO
        };

        // Guaranteed profit = $1 payout - cost per pair
        matched_pairs * (Decimal::ONE - yes_avg - no_avg)
    }

    /// Calculate unrealized P&L given current market prices.
    ///
    /// # Arguments
    /// * `yes_price` - Current YES bid price (what we could sell for)
    /// * `no_price` - Current NO bid price (what we could sell for)
    pub fn unrealized_pnl(&self, yes_price: Decimal, no_price: Decimal) -> Decimal {
        let yes_value = self.yes_shares * yes_price;
        let no_value = self.no_shares * no_price;
        let total_value = yes_value + no_value;
        total_value - self.total_cost()
    }

    /// UP ratio (YES / total shares).
    pub fn up_ratio(&self) -> Decimal {
        let total = self.total_shares();
        if total <= Decimal::ZERO {
            return dec!(0.5);
        }
        self.yes_shares / total
    }

    /// DOWN ratio (NO / total shares).
    pub fn down_ratio(&self) -> Decimal {
        let total = self.total_shares();
        if total <= Decimal::ZERO {
            return dec!(0.5);
        }
        self.no_shares / total
    }
}

/// Centralized position tracking across all markets.
pub struct PositionTracker {
    /// Positions by event ID.
    positions: DashMap<String, MarketPosition>,
}

impl PositionTracker {
    /// Create a new position tracker.
    pub fn new() -> Self {
        Self {
            positions: DashMap::new(),
        }
    }

    /// Get or create a position for a market.
    pub fn get_or_create(
        &self,
        event_id: &str,
        asset: CryptoAsset,
        yes_token_id: &str,
        no_token_id: &str,
        strike_price: Decimal,
    ) -> MarketPosition {
        self.positions
            .entry(event_id.to_string())
            .or_insert_with(|| {
                MarketPosition::new(
                    event_id.to_string(),
                    asset,
                    yes_token_id.to_string(),
                    no_token_id.to_string(),
                    strike_price,
                )
            })
            .clone()
    }

    /// Record a fill for a market position.
    pub fn record_fill(
        &self,
        event_id: &str,
        outcome: Outcome,
        shares: Decimal,
        cost: Decimal,
        fee: Decimal,
    ) {
        if let Some(mut pos) = self.positions.get_mut(event_id) {
            pos.record_fill(outcome, shares, cost, fee);
        }
    }

    /// Get a position by event ID.
    pub fn get_position(&self, event_id: &str) -> Option<MarketPosition> {
        self.positions.get(event_id).map(|r| r.clone())
    }

    /// Get all positions.
    pub fn all_positions(&self) -> Vec<MarketPosition> {
        self.positions.iter().map(|r| r.clone()).collect()
    }

    /// Get all non-flat positions.
    pub fn active_positions(&self) -> Vec<MarketPosition> {
        self.positions
            .iter()
            .filter(|r| !r.is_flat())
            .map(|r| r.clone())
            .collect()
    }

    /// Get total exposure across all positions.
    pub fn total_exposure(&self) -> Decimal {
        self.positions.iter().map(|r| r.total_exposure()).sum()
    }

    /// Get total guaranteed P&L across all positions.
    pub fn total_guaranteed_pnl(&self) -> Decimal {
        self.positions.iter().map(|r| r.guaranteed_pnl()).sum()
    }

    /// Get total realized P&L across all positions.
    pub fn total_realized_pnl(&self) -> Decimal {
        self.positions.iter().map(|r| r.realized_pnl).sum()
    }

    /// Get total fees paid across all positions.
    pub fn total_fees(&self) -> Decimal {
        self.positions.iter().map(|r| r.fees_paid).sum()
    }

    /// Get total trade count across all positions.
    pub fn total_trades(&self) -> u32 {
        self.positions.iter().map(|r| r.trade_count).sum()
    }

    /// Remove a position (after market settlement).
    pub fn remove_position(&self, event_id: &str) -> Option<MarketPosition> {
        self.positions.remove(event_id).map(|(_, pos)| pos)
    }

    /// Clear all positions (for session reset).
    pub fn clear(&self) {
        self.positions.clear();
    }

    /// Get summary statistics.
    pub fn summary(&self) -> PositionSummary {
        let mut total_markets = 0u32;
        let mut active_markets = 0u32;
        let mut total_yes_shares = Decimal::ZERO;
        let mut total_no_shares = Decimal::ZERO;
        let mut total_exposure = Decimal::ZERO;
        let mut total_guaranteed_pnl = Decimal::ZERO;
        let mut total_realized_pnl = Decimal::ZERO;
        let mut total_fees = Decimal::ZERO;

        for pos in self.positions.iter() {
            total_markets += 1;
            if !pos.is_flat() {
                active_markets += 1;
            }
            total_yes_shares += pos.yes_shares;
            total_no_shares += pos.no_shares;
            total_exposure += pos.total_exposure();
            total_guaranteed_pnl += pos.guaranteed_pnl();
            total_realized_pnl += pos.realized_pnl;
            total_fees += pos.fees_paid;
        }

        PositionSummary {
            total_markets,
            active_markets,
            total_yes_shares,
            total_no_shares,
            total_exposure,
            total_guaranteed_pnl,
            total_realized_pnl,
            total_fees,
        }
    }
}

impl Default for PositionTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Summary of position statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionSummary {
    pub total_markets: u32,
    pub active_markets: u32,
    pub total_yes_shares: Decimal,
    pub total_no_shares: Decimal,
    pub total_exposure: Decimal,
    pub total_guaranteed_pnl: Decimal,
    pub total_realized_pnl: Decimal,
    pub total_fees: Decimal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_fill() {
        let mut pos = MarketPosition::new(
            "event1".to_string(),
            CryptoAsset::Btc,
            "yes_token".to_string(),
            "no_token".to_string(),
            dec!(100000),
        );

        // Add YES position
        pos.record_fill(Outcome::Yes, dec!(100), dec!(45), dec!(0.225));
        assert_eq!(pos.yes_shares, dec!(100));
        assert_eq!(pos.yes_cost_basis, dec!(45));

        // Add NO position
        pos.record_fill(Outcome::No, dec!(80), dec!(44), dec!(0.22));
        assert_eq!(pos.no_shares, dec!(80));
        assert_eq!(pos.no_cost_basis, dec!(44));

        // Check totals
        assert_eq!(pos.total_shares(), dec!(180));
        assert_eq!(pos.total_cost(), dec!(89));
        assert_eq!(pos.fees_paid, dec!(0.445));
        assert_eq!(pos.trade_count, 2);
    }

    #[test]
    fn test_guaranteed_pnl() {
        let mut pos = MarketPosition::new(
            "event1".to_string(),
            CryptoAsset::Btc,
            "yes_token".to_string(),
            "no_token".to_string(),
            dec!(100000),
        );

        // Buy 100 YES at $0.45 each
        pos.record_fill(Outcome::Yes, dec!(100), dec!(45), dec!(0));
        // Buy 100 NO at $0.52 each
        pos.record_fill(Outcome::No, dec!(100), dec!(52), dec!(0));

        // Matched pairs = 100
        // Cost per pair = 0.45 + 0.52 = 0.97
        // Guaranteed profit per pair = 1.00 - 0.97 = 0.03
        // Total guaranteed = 100 * 0.03 = 3.00
        assert_eq!(pos.guaranteed_pnl(), dec!(3));
    }

    #[test]
    fn test_imbalance_ratio() {
        let mut pos = MarketPosition::new(
            "event1".to_string(),
            CryptoAsset::Btc,
            "yes_token".to_string(),
            "no_token".to_string(),
            dec!(100000),
        );

        // Perfectly balanced
        pos.yes_shares = dec!(50);
        pos.no_shares = dec!(50);
        assert_eq!(pos.imbalance_ratio(), dec!(0));
        assert_eq!(pos.state(), PositionState::Flat); // 0 < 0.01

        // Slightly imbalanced
        pos.yes_shares = dec!(60);
        pos.no_shares = dec!(40);
        // imbalance = (60-40)/100 = 0.2
        assert_eq!(pos.imbalance_ratio(), dec!(0.2));
        assert_eq!(pos.state(), PositionState::Skewed);

        // Very imbalanced
        pos.yes_shares = dec!(90);
        pos.no_shares = dec!(10);
        // imbalance = (90-10)/100 = 0.8
        assert_eq!(pos.imbalance_ratio(), dec!(0.8));
        assert_eq!(pos.state(), PositionState::Crisis);
    }

    #[test]
    fn test_position_tracker() {
        let tracker = PositionTracker::new();

        // Create position
        let pos = tracker.get_or_create(
            "event1",
            CryptoAsset::Btc,
            "yes_token",
            "no_token",
            dec!(100000),
        );
        assert!(pos.is_flat());

        // Record fills
        tracker.record_fill("event1", Outcome::Yes, dec!(100), dec!(45), dec!(0.225));
        tracker.record_fill("event1", Outcome::No, dec!(100), dec!(52), dec!(0.26));

        let pos = tracker.get_position("event1").unwrap();
        assert_eq!(pos.yes_shares, dec!(100));
        assert_eq!(pos.no_shares, dec!(100));

        // Check summary
        let summary = tracker.summary();
        assert_eq!(summary.total_markets, 1);
        assert_eq!(summary.active_markets, 1);
        assert_eq!(summary.total_exposure, dec!(97));
    }
}
