//! Common position and risk management for all executor types.
//!
//! This module provides a single source of truth for position tracking and
//! risk limit enforcement. Both live and simulated executors use this common
//! implementation to ensure consistent behavior.

use std::collections::HashMap;

use rust_decimal::Decimal;
use tracing::{debug, info};

/// Configuration for position and risk limits.
#[derive(Debug, Clone)]
pub struct RiskConfig {
    /// Maximum exposure per market in USDC (0 = unlimited).
    pub max_market_exposure: Decimal,
    /// Maximum total exposure across all markets in USDC (0 = unlimited).
    pub max_total_exposure: Decimal,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_market_exposure: Decimal::ZERO,
            max_total_exposure: Decimal::ZERO,
        }
    }
}

/// A position in a single market.
#[derive(Debug, Clone, Default)]
pub struct Position {
    /// Number of YES shares held.
    pub yes_shares: Decimal,
    /// Number of NO shares held.
    pub no_shares: Decimal,
    /// Total cost basis (USDC spent to acquire position).
    pub cost_basis: Decimal,
    /// Realized PnL from partial exits or settlements.
    pub realized_pnl: Decimal,
}

impl Position {
    /// Check if position is empty (no shares).
    pub fn is_empty(&self) -> bool {
        self.yes_shares.is_zero() && self.no_shares.is_zero()
    }

    /// Total shares held.
    pub fn total_shares(&self) -> Decimal {
        self.yes_shares + self.no_shares
    }
}

/// Result of a limit check.
#[derive(Debug, Clone)]
pub enum LimitCheckResult {
    /// Order is allowed at full size.
    Allowed,
    /// Order exceeds limits - contains max allowed size (may be zero).
    ExceedsLimit {
        max_allowed: Decimal,
        reason: String,
    },
}

/// Manages positions and enforces risk limits.
///
/// This is the single source of truth for position tracking. Both live and
/// simulated executors delegate position management to this common component.
#[derive(Debug)]
pub struct PositionManager {
    /// Risk configuration.
    config: RiskConfig,
    /// Positions by event_id.
    positions: HashMap<String, Position>,
    /// Statistics.
    stats: PositionStats,
}

/// Statistics tracked by the position manager.
#[derive(Debug, Default, Clone)]
pub struct PositionStats {
    /// Number of orders checked.
    pub orders_checked: u64,
    /// Number of orders rejected due to limits.
    pub orders_rejected: u64,
    /// Number of markets settled.
    pub markets_settled: u64,
    /// Gross profit from winning positions.
    pub gross_profit: Decimal,
    /// Gross loss from losing positions.
    pub gross_loss: Decimal,
}

impl PositionManager {
    /// Create a new position manager with the given risk config.
    pub fn new(config: RiskConfig) -> Self {
        info!(
            max_market_exposure = %config.max_market_exposure,
            max_total_exposure = %config.max_total_exposure,
            "PositionManager initialized"
        );
        Self {
            config,
            positions: HashMap::new(),
            stats: PositionStats::default(),
        }
    }

    /// Get current exposure for a specific market.
    pub fn market_exposure(&self, event_id: &str) -> Decimal {
        self.positions
            .get(event_id)
            .map(|p| p.cost_basis)
            .unwrap_or(Decimal::ZERO)
    }

    /// Get total exposure across all positions.
    pub fn total_exposure(&self) -> Decimal {
        self.positions.values().map(|p| p.cost_basis).sum()
    }

    /// Get remaining capacity before hitting total exposure limit.
    pub fn remaining_capacity(&self) -> Decimal {
        if self.config.max_total_exposure.is_zero() {
            Decimal::MAX // Unlimited
        } else {
            (self.config.max_total_exposure - self.total_exposure()).max(Decimal::ZERO)
        }
    }

    /// Get a position by event_id.
    pub fn get_position(&self, event_id: &str) -> Option<&Position> {
        self.positions.get(event_id)
    }

    /// Get all positions.
    pub fn positions(&self) -> &HashMap<String, Position> {
        &self.positions
    }

    /// Get statistics.
    pub fn stats(&self) -> &PositionStats {
        &self.stats
    }

    /// Check if an order would exceed limits.
    ///
    /// Returns `Allowed` if the full order size is permitted, or `ExceedsLimit`
    /// with the maximum allowed size if limits would be exceeded.
    pub fn check_limits(&mut self, event_id: &str, order_cost: Decimal) -> LimitCheckResult {
        self.stats.orders_checked += 1;

        let current_market = self.market_exposure(event_id);
        let current_total = self.total_exposure();

        // Check per-market limit
        if self.config.max_market_exposure > Decimal::ZERO {
            let new_market = current_market + order_cost;
            if new_market > self.config.max_market_exposure {
                let max_allowed = (self.config.max_market_exposure - current_market).max(Decimal::ZERO);
                debug!(
                    event_id = %event_id,
                    current = %current_market,
                    order = %order_cost,
                    max = %self.config.max_market_exposure,
                    allowed = %max_allowed,
                    "Per-market limit exceeded"
                );
                self.stats.orders_rejected += 1;
                return LimitCheckResult::ExceedsLimit {
                    max_allowed,
                    reason: format!(
                        "Market exposure limit: current=${:.2} + order=${:.2} > max=${:.2}",
                        current_market, order_cost, self.config.max_market_exposure
                    ),
                };
            }
        }

        // Check total exposure limit
        if self.config.max_total_exposure > Decimal::ZERO {
            let new_total = current_total + order_cost;
            if new_total > self.config.max_total_exposure {
                // Calculate max allowed considering both limits
                let remaining_total = (self.config.max_total_exposure - current_total).max(Decimal::ZERO);
                let remaining_market = if self.config.max_market_exposure > Decimal::ZERO {
                    (self.config.max_market_exposure - current_market).max(Decimal::ZERO)
                } else {
                    Decimal::MAX
                };
                let max_allowed = remaining_total.min(remaining_market);

                debug!(
                    event_id = %event_id,
                    current_total = %current_total,
                    order = %order_cost,
                    max_total = %self.config.max_total_exposure,
                    allowed = %max_allowed,
                    "Total exposure limit exceeded"
                );
                self.stats.orders_rejected += 1;
                return LimitCheckResult::ExceedsLimit {
                    max_allowed,
                    reason: format!(
                        "Total exposure limit: current=${:.2} + order=${:.2} > max=${:.2}",
                        current_total, order_cost, self.config.max_total_exposure
                    ),
                };
            }
        }

        LimitCheckResult::Allowed
    }

    /// Record a fill, updating the position.
    ///
    /// Call this after an order is filled (or partially filled).
    pub fn record_fill(
        &mut self,
        event_id: &str,
        is_yes: bool,
        shares: Decimal,
        cost: Decimal,
    ) {
        let position = self.positions.entry(event_id.to_string()).or_default();

        if is_yes {
            position.yes_shares += shares;
        } else {
            position.no_shares += shares;
        }
        position.cost_basis += cost;

        debug!(
            event_id = %event_id,
            is_yes = %is_yes,
            shares = %shares,
            cost = %cost,
            new_yes = %position.yes_shares,
            new_no = %position.no_shares,
            new_cost_basis = %position.cost_basis,
            "Position updated"
        );
    }

    /// Settle a market when it expires.
    ///
    /// Returns the realized PnL from the settlement.
    pub fn settle_market(&mut self, event_id: &str, yes_wins: bool) -> Decimal {
        let Some(position) = self.positions.remove(event_id) else {
            return Decimal::ZERO;
        };

        // Calculate payout: winning side pays $1 per share
        let payout = if yes_wins {
            position.yes_shares
        } else {
            position.no_shares
        };

        let realized_pnl = payout - position.cost_basis;

        // Track stats
        self.stats.markets_settled += 1;
        if realized_pnl > Decimal::ZERO {
            self.stats.gross_profit += realized_pnl;
        } else {
            self.stats.gross_loss += realized_pnl.abs();
        }

        info!(
            event_id = %event_id,
            yes_wins = %yes_wins,
            yes_shares = %position.yes_shares,
            no_shares = %position.no_shares,
            cost_basis = %position.cost_basis,
            payout = %payout,
            realized_pnl = %realized_pnl,
            "Market settled"
        );

        realized_pnl
    }

    /// Clear all positions (for testing or reset).
    pub fn clear(&mut self) {
        self.positions.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_market_limit() {
        let config = RiskConfig {
            max_market_exposure: dec!(100),
            max_total_exposure: dec!(1000),
        };
        let mut pm = PositionManager::new(config);

        // First order should be allowed
        assert!(matches!(pm.check_limits("market1", dec!(50)), LimitCheckResult::Allowed));
        pm.record_fill("market1", true, dec!(50), dec!(50));

        // Second order up to limit should be allowed
        assert!(matches!(pm.check_limits("market1", dec!(50)), LimitCheckResult::Allowed));
        pm.record_fill("market1", true, dec!(50), dec!(50));

        // Third order should exceed limit
        match pm.check_limits("market1", dec!(10)) {
            LimitCheckResult::ExceedsLimit { max_allowed, .. } => {
                assert_eq!(max_allowed, dec!(0));
            }
            _ => panic!("Expected ExceedsLimit"),
        }
    }

    #[test]
    fn test_total_limit() {
        let config = RiskConfig {
            max_market_exposure: dec!(100),
            max_total_exposure: dec!(150),
        };
        let mut pm = PositionManager::new(config);

        // Fill market1 to $100
        pm.record_fill("market1", true, dec!(100), dec!(100));

        // market2 should only allow $50 more
        match pm.check_limits("market2", dec!(60)) {
            LimitCheckResult::ExceedsLimit { max_allowed, .. } => {
                assert_eq!(max_allowed, dec!(50));
            }
            _ => panic!("Expected ExceedsLimit"),
        }
    }

    #[test]
    fn test_settlement() {
        let config = RiskConfig::default();
        let mut pm = PositionManager::new(config);

        // Buy 100 YES shares at $0.40 each = $40 cost
        pm.record_fill("market1", true, dec!(100), dec!(40));

        // YES wins: payout = 100 * $1 = $100, PnL = $100 - $40 = $60
        let pnl = pm.settle_market("market1", true);
        assert_eq!(pnl, dec!(60));

        // Position should be cleared
        assert!(pm.get_position("market1").is_none());
        assert_eq!(pm.total_exposure(), dec!(0));
    }
}
