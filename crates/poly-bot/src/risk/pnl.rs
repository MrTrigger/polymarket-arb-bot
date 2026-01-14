//! P&L-based risk management for the trading bot.
//!
//! Tracks daily P&L, consecutive losses, and position hedge ratios to make
//! risk-aware trade decisions. Works alongside the circuit breaker for
//! comprehensive risk control.
//!
//! ## Risk Rules
//!
//! 1. **Daily loss limit**: Stop trading after losing 10% of available balance
//! 2. **Consecutive losses**: Stop trading after 3 consecutive losing trades
//! 3. **Hedge ratio**: Require rebalancing when min_side_ratio < 20%
//!
//! ## Usage
//!
//! ```ignore
//! let config = PnlRiskConfig::new(dec!(5000)); // $5000 balance
//! let mut manager = PnlRiskManager::new(config);
//!
//! // Before each trade
//! let position = Position { ... };
//! match manager.check_trade(&position, proposed_size) {
//!     TradeDecision::Approve => { /* proceed */ }
//!     TradeDecision::Reject { reason } => { /* skip trade */ }
//!     TradeDecision::ReduceSize { max_allowed } => { /* reduce size */ }
//!     TradeDecision::RebalanceRequired { .. } => { /* hedge first */ }
//! }
//!
//! // After each trade settles
//! manager.record_trade(pnl);
//! ```

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::types::{Position, TradeDecision};

/// Configuration for P&L-based risk management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlRiskConfig {
    /// Available balance for trading (used to calculate daily loss limit).
    pub available_balance: Decimal,

    /// Maximum daily loss as a ratio of available_balance (default: 0.10 = 10%).
    pub max_daily_loss_ratio: Decimal,

    /// Maximum consecutive losing trades before stopping (default: 3).
    pub max_consecutive_losses: u32,

    /// Minimum hedge ratio - requires rebalancing below this (default: 0.20 = 20%).
    pub min_hedge_ratio: Decimal,

    /// Size reduction factor when approaching limits (default: 0.50 = 50%).
    pub warning_size_reduction: Decimal,

    /// Ratio of daily loss limit to trigger size reduction (default: 0.75 = 75%).
    pub warning_threshold: Decimal,
}

impl PnlRiskConfig {
    /// Create a new config with the given available balance.
    /// Uses sensible defaults for risk parameters.
    pub fn new(available_balance: Decimal) -> Self {
        Self {
            available_balance,
            max_daily_loss_ratio: Decimal::new(10, 2),   // 0.10 = 10%
            max_consecutive_losses: 3,
            min_hedge_ratio: Decimal::new(20, 2),        // 0.20 = 20%
            warning_size_reduction: Decimal::new(50, 2), // 0.50 = 50%
            warning_threshold: Decimal::new(75, 2),      // 0.75 = 75%
        }
    }

    /// Create config with custom parameters.
    #[allow(clippy::too_many_arguments)]
    pub fn with_params(
        available_balance: Decimal,
        max_daily_loss_ratio: Decimal,
        max_consecutive_losses: u32,
        min_hedge_ratio: Decimal,
    ) -> Self {
        Self {
            available_balance,
            max_daily_loss_ratio,
            max_consecutive_losses,
            min_hedge_ratio,
            warning_size_reduction: Decimal::new(50, 2),
            warning_threshold: Decimal::new(75, 2),
        }
    }

    /// Calculate the absolute daily loss limit in USDC.
    #[inline]
    pub fn daily_loss_limit(&self) -> Decimal {
        self.available_balance * self.max_daily_loss_ratio
    }
}

impl Default for PnlRiskConfig {
    fn default() -> Self {
        Self::new(Decimal::new(5000, 0)) // $5000 default
    }
}

/// Reason for a trade rejection from PnlRiskManager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PnlRejectionReason {
    /// Daily loss limit has been reached.
    DailyLossLimit {
        /// Current daily P&L (negative = loss).
        current_pnl: Decimal,
        /// Maximum allowed loss.
        limit: Decimal,
    },

    /// Too many consecutive losing trades.
    ConsecutiveLosses {
        /// Number of consecutive losses.
        count: u32,
        /// Maximum allowed.
        max: u32,
    },

    /// Trading has been manually disabled.
    TradingDisabled,
}

impl std::fmt::Display for PnlRejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PnlRejectionReason::DailyLossLimit { current_pnl, limit } => {
                write!(
                    f,
                    "Daily loss limit reached: ${} loss >= ${} limit",
                    current_pnl.abs(),
                    limit
                )
            }
            PnlRejectionReason::ConsecutiveLosses { count, max } => {
                write!(
                    f,
                    "Consecutive loss limit: {} losses >= {} max",
                    count, max
                )
            }
            PnlRejectionReason::TradingDisabled => {
                write!(f, "Trading is disabled")
            }
        }
    }
}

/// P&L-based risk manager.
///
/// Tracks trading performance and makes risk decisions based on:
/// - Daily P&L vs configured loss limit
/// - Consecutive losing trades
/// - Position hedge ratio
///
/// Thread-safety: This struct is NOT thread-safe. Wrap in appropriate
/// synchronization if shared across tasks.
#[derive(Debug, Clone)]
pub struct PnlRiskManager {
    /// Configuration.
    config: PnlRiskConfig,

    /// Daily P&L (negative = loss).
    daily_pnl: Decimal,

    /// Number of consecutive losing trades.
    consecutive_losses: u32,

    /// Whether trading is enabled.
    trading_enabled: bool,

    /// Total number of trades today.
    trade_count: u32,

    /// Number of winning trades today.
    winning_trades: u32,

    /// Number of losing trades today.
    losing_trades: u32,
}

impl PnlRiskManager {
    /// Create a new P&L risk manager with the given config.
    pub fn new(config: PnlRiskConfig) -> Self {
        Self {
            config,
            daily_pnl: Decimal::ZERO,
            consecutive_losses: 0,
            trading_enabled: true,
            trade_count: 0,
            winning_trades: 0,
            losing_trades: 0,
        }
    }

    /// Create with a simple balance (uses default risk params).
    pub fn with_balance(available_balance: Decimal) -> Self {
        Self::new(PnlRiskConfig::new(available_balance))
    }

    /// Get the current configuration.
    pub fn config(&self) -> &PnlRiskConfig {
        &self.config
    }

    /// Get current daily P&L.
    #[inline]
    pub fn daily_pnl(&self) -> Decimal {
        self.daily_pnl
    }

    /// Get consecutive losses count.
    #[inline]
    pub fn consecutive_losses(&self) -> u32 {
        self.consecutive_losses
    }

    /// Check if trading is enabled.
    #[inline]
    pub fn is_trading_enabled(&self) -> bool {
        self.trading_enabled
    }

    /// Get total trade count for the day.
    #[inline]
    pub fn trade_count(&self) -> u32 {
        self.trade_count
    }

    /// Get winning trades count.
    #[inline]
    pub fn winning_trades(&self) -> u32 {
        self.winning_trades
    }

    /// Get losing trades count.
    #[inline]
    pub fn losing_trades(&self) -> u32 {
        self.losing_trades
    }

    /// Get win rate as a ratio (0.0 to 1.0).
    pub fn win_rate(&self) -> Decimal {
        if self.trade_count == 0 {
            return Decimal::ZERO;
        }
        Decimal::from(self.winning_trades) / Decimal::from(self.trade_count)
    }

    /// Get daily loss limit (absolute USDC value).
    #[inline]
    pub fn daily_loss_limit(&self) -> Decimal {
        self.config.daily_loss_limit()
    }

    /// Get remaining loss budget before hitting the limit.
    #[inline]
    pub fn remaining_loss_budget(&self) -> Decimal {
        let limit = self.daily_loss_limit();
        // If daily_pnl is -100 and limit is 500, we have 400 left
        // remaining = limit - abs(min(daily_pnl, 0))
        if self.daily_pnl < Decimal::ZERO {
            (limit - self.daily_pnl.abs()).max(Decimal::ZERO)
        } else {
            limit + self.daily_pnl // If positive, we have even more room
        }
    }

    /// Check if we're approaching the daily loss limit.
    ///
    /// Returns true if current loss >= warning_threshold × limit.
    pub fn is_approaching_limit(&self) -> bool {
        if self.daily_pnl >= Decimal::ZERO {
            return false;
        }
        let loss = self.daily_pnl.abs();
        let limit = self.daily_loss_limit();
        let warning_level = limit * self.config.warning_threshold;
        loss >= warning_level
    }

    /// Enable trading.
    pub fn enable_trading(&mut self) {
        self.trading_enabled = true;
    }

    /// Disable trading.
    pub fn disable_trading(&mut self) {
        self.trading_enabled = false;
    }

    /// Check if a trade should be allowed.
    ///
    /// Evaluates:
    /// 1. Whether trading is enabled
    /// 2. Daily loss limit
    /// 3. Consecutive losses
    /// 4. Position hedge ratio (if position provided)
    ///
    /// # Arguments
    ///
    /// * `position` - Current position (optional, for hedge ratio check)
    /// * `proposed_size` - Size of the proposed trade in USDC
    ///
    /// # Returns
    ///
    /// A `TradeDecision` indicating whether to proceed, reduce size, or reject.
    pub fn check_trade(
        &self,
        position: Option<&Position>,
        proposed_size: Decimal,
    ) -> TradeDecision {
        // 1. Check if trading is disabled
        if !self.trading_enabled {
            return TradeDecision::reject(
                PnlRejectionReason::TradingDisabled.to_string()
            );
        }

        // 2. Check daily loss limit
        let limit = self.daily_loss_limit();
        if self.daily_pnl <= -limit {
            return TradeDecision::reject(
                PnlRejectionReason::DailyLossLimit {
                    current_pnl: self.daily_pnl,
                    limit,
                }.to_string()
            );
        }

        // 3. Check consecutive losses
        if self.consecutive_losses >= self.config.max_consecutive_losses {
            return TradeDecision::reject(
                PnlRejectionReason::ConsecutiveLosses {
                    count: self.consecutive_losses,
                    max: self.config.max_consecutive_losses,
                }.to_string()
            );
        }

        // 4. Check position hedge ratio (if position provided)
        if let Some(pos) = position
            && !pos.is_empty()
        {
            let hedge_ratio = pos.min_side_ratio();
            if hedge_ratio < self.config.min_hedge_ratio {
                return TradeDecision::RebalanceRequired {
                    current_ratio: hedge_ratio,
                    target_ratio: self.config.min_hedge_ratio,
                };
            }
        }

        // 5. Check if approaching limit - reduce size
        if self.is_approaching_limit() {
            let max_allowed = proposed_size * self.config.warning_size_reduction;
            if max_allowed < proposed_size {
                return TradeDecision::ReduceSize { max_allowed };
            }
        }

        TradeDecision::Approve
    }

    /// Quick check if any trade can proceed (fast path).
    ///
    /// Returns false if:
    /// - Trading is disabled
    /// - Daily loss limit exceeded
    /// - Consecutive losses exceeded
    #[inline]
    pub fn can_trade(&self) -> bool {
        self.trading_enabled
            && self.daily_pnl > -self.daily_loss_limit()
            && self.consecutive_losses < self.config.max_consecutive_losses
    }

    /// Record a completed trade's P&L.
    ///
    /// Updates:
    /// - Daily P&L
    /// - Trade counts
    /// - Consecutive losses
    /// - Trading enabled state (if limits exceeded)
    ///
    /// # Arguments
    ///
    /// * `pnl` - The P&L from the trade (positive = profit, negative = loss)
    pub fn record_trade(&mut self, pnl: Decimal) {
        self.daily_pnl += pnl;
        self.trade_count += 1;

        if pnl >= Decimal::ZERO {
            self.winning_trades += 1;
            self.consecutive_losses = 0; // Reset on win
        } else {
            self.losing_trades += 1;
            self.consecutive_losses += 1;

            // Auto-disable if consecutive losses exceeded
            if self.consecutive_losses >= self.config.max_consecutive_losses {
                self.trading_enabled = false;
            }
        }

        // Auto-disable if daily loss limit exceeded
        if self.daily_pnl <= -self.daily_loss_limit() {
            self.trading_enabled = false;
        }
    }

    /// Record multiple trades at once (for batch processing).
    pub fn record_trades(&mut self, pnls: &[Decimal]) {
        for &pnl in pnls {
            self.record_trade(pnl);
        }
    }

    /// Reset daily stats (call at start of new trading day).
    pub fn reset_daily(&mut self) {
        self.daily_pnl = Decimal::ZERO;
        self.consecutive_losses = 0;
        self.trading_enabled = true;
        self.trade_count = 0;
        self.winning_trades = 0;
        self.losing_trades = 0;
    }

    /// Reset consecutive losses only (for manual intervention).
    pub fn reset_consecutive_losses(&mut self) {
        self.consecutive_losses = 0;
        // Re-enable if daily limit not exceeded
        if self.daily_pnl > -self.daily_loss_limit() {
            self.trading_enabled = true;
        }
    }

    /// Update the available balance (e.g., after deposit/withdrawal).
    pub fn update_balance(&mut self, new_balance: Decimal) {
        self.config.available_balance = new_balance;
    }

    /// Get stats for logging/display.
    pub fn stats(&self) -> PnlRiskStats {
        PnlRiskStats {
            daily_pnl: self.daily_pnl,
            daily_loss_limit: self.daily_loss_limit(),
            remaining_budget: self.remaining_loss_budget(),
            consecutive_losses: self.consecutive_losses,
            max_consecutive_losses: self.config.max_consecutive_losses,
            trade_count: self.trade_count,
            winning_trades: self.winning_trades,
            losing_trades: self.losing_trades,
            win_rate: self.win_rate(),
            trading_enabled: self.trading_enabled,
        }
    }
}

impl Default for PnlRiskManager {
    fn default() -> Self {
        Self::new(PnlRiskConfig::default())
    }
}

/// Statistics from the P&L risk manager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlRiskStats {
    /// Current daily P&L.
    pub daily_pnl: Decimal,
    /// Daily loss limit.
    pub daily_loss_limit: Decimal,
    /// Remaining loss budget.
    pub remaining_budget: Decimal,
    /// Current consecutive losses.
    pub consecutive_losses: u32,
    /// Maximum consecutive losses allowed.
    pub max_consecutive_losses: u32,
    /// Total trades today.
    pub trade_count: u32,
    /// Winning trades.
    pub winning_trades: u32,
    /// Losing trades.
    pub losing_trades: u32,
    /// Win rate (0.0 to 1.0).
    pub win_rate: Decimal,
    /// Whether trading is enabled.
    pub trading_enabled: bool,
}

impl std::fmt::Display for PnlRiskStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PnL: ${:.2} (limit: ${:.2}, remaining: ${:.2}), \
             Trades: {}/{}/{} (W/L/T), Win rate: {:.1}%, \
             Consec losses: {}/{}, Enabled: {}",
            self.daily_pnl,
            self.daily_loss_limit,
            self.remaining_budget,
            self.winning_trades,
            self.losing_trades,
            self.trade_count,
            self.win_rate * Decimal::ONE_HUNDRED,
            self.consecutive_losses,
            self.max_consecutive_losses,
            if self.trading_enabled { "yes" } else { "no" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn default_manager() -> PnlRiskManager {
        PnlRiskManager::with_balance(dec!(5000))
    }

    // =========================================================================
    // PnlRiskConfig Tests
    // =========================================================================

    #[test]
    fn test_config_new() {
        let config = PnlRiskConfig::new(dec!(5000));
        assert_eq!(config.available_balance, dec!(5000));
        assert_eq!(config.max_daily_loss_ratio, dec!(0.10));
        assert_eq!(config.max_consecutive_losses, 3);
        assert_eq!(config.min_hedge_ratio, dec!(0.20));
    }

    #[test]
    fn test_config_daily_loss_limit() {
        let config = PnlRiskConfig::new(dec!(5000));
        // 5000 * 0.10 = 500
        assert_eq!(config.daily_loss_limit(), dec!(500));

        let config = PnlRiskConfig::new(dec!(10000));
        // 10000 * 0.10 = 1000
        assert_eq!(config.daily_loss_limit(), dec!(1000));
    }

    #[test]
    fn test_config_with_params() {
        let config = PnlRiskConfig::with_params(
            dec!(10000),
            dec!(0.05), // 5% loss limit
            5,          // 5 consecutive losses
            dec!(0.25), // 25% hedge ratio
        );
        assert_eq!(config.available_balance, dec!(10000));
        assert_eq!(config.max_daily_loss_ratio, dec!(0.05));
        assert_eq!(config.max_consecutive_losses, 5);
        assert_eq!(config.min_hedge_ratio, dec!(0.25));
        assert_eq!(config.daily_loss_limit(), dec!(500));
    }

    #[test]
    fn test_config_default() {
        let config = PnlRiskConfig::default();
        assert_eq!(config.available_balance, dec!(5000));
        assert_eq!(config.daily_loss_limit(), dec!(500));
    }

    #[test]
    fn test_config_serialization() {
        let config = PnlRiskConfig::new(dec!(5000));
        let json = serde_json::to_string(&config).unwrap();
        let parsed: PnlRiskConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.available_balance, config.available_balance);
        assert_eq!(parsed.max_daily_loss_ratio, config.max_daily_loss_ratio);
    }

    // =========================================================================
    // PnlRiskManager Creation Tests
    // =========================================================================

    #[test]
    fn test_manager_new() {
        let manager = default_manager();
        assert_eq!(manager.daily_pnl(), dec!(0));
        assert_eq!(manager.consecutive_losses(), 0);
        assert!(manager.is_trading_enabled());
        assert_eq!(manager.trade_count(), 0);
        assert_eq!(manager.winning_trades(), 0);
        assert_eq!(manager.losing_trades(), 0);
    }

    #[test]
    fn test_manager_with_balance() {
        let manager = PnlRiskManager::with_balance(dec!(10000));
        assert_eq!(manager.daily_loss_limit(), dec!(1000));
    }

    #[test]
    fn test_manager_default() {
        let manager = PnlRiskManager::default();
        assert_eq!(manager.daily_loss_limit(), dec!(500));
    }

    // =========================================================================
    // Trade Recording Tests
    // =========================================================================

    #[test]
    fn test_record_winning_trade() {
        let mut manager = default_manager();

        manager.record_trade(dec!(10));

        assert_eq!(manager.daily_pnl(), dec!(10));
        assert_eq!(manager.trade_count(), 1);
        assert_eq!(manager.winning_trades(), 1);
        assert_eq!(manager.losing_trades(), 0);
        assert_eq!(manager.consecutive_losses(), 0);
        assert!(manager.is_trading_enabled());
    }

    #[test]
    fn test_record_losing_trade() {
        let mut manager = default_manager();

        manager.record_trade(dec!(-10));

        assert_eq!(manager.daily_pnl(), dec!(-10));
        assert_eq!(manager.trade_count(), 1);
        assert_eq!(manager.winning_trades(), 0);
        assert_eq!(manager.losing_trades(), 1);
        assert_eq!(manager.consecutive_losses(), 1);
        assert!(manager.is_trading_enabled());
    }

    #[test]
    fn test_record_multiple_trades() {
        let mut manager = default_manager();

        manager.record_trade(dec!(10));  // Win
        manager.record_trade(dec!(-5));  // Lose
        manager.record_trade(dec!(15));  // Win

        assert_eq!(manager.daily_pnl(), dec!(20));
        assert_eq!(manager.trade_count(), 3);
        assert_eq!(manager.winning_trades(), 2);
        assert_eq!(manager.losing_trades(), 1);
        assert_eq!(manager.consecutive_losses(), 0); // Reset after last win
    }

    #[test]
    fn test_record_trades_batch() {
        let mut manager = default_manager();
        let pnls = [dec!(10), dec!(-5), dec!(15)];

        manager.record_trades(&pnls);

        assert_eq!(manager.daily_pnl(), dec!(20));
        assert_eq!(manager.trade_count(), 3);
    }

    #[test]
    fn test_consecutive_losses_tracking() {
        let mut manager = default_manager();

        manager.record_trade(dec!(-10)); // 1
        assert_eq!(manager.consecutive_losses(), 1);

        manager.record_trade(dec!(-10)); // 2
        assert_eq!(manager.consecutive_losses(), 2);

        manager.record_trade(dec!(5));   // Win - resets
        assert_eq!(manager.consecutive_losses(), 0);

        manager.record_trade(dec!(-10)); // 1 again
        assert_eq!(manager.consecutive_losses(), 1);
    }

    #[test]
    fn test_auto_disable_consecutive_losses() {
        let mut manager = default_manager();
        assert!(manager.is_trading_enabled());

        // Lose 3 times in a row
        manager.record_trade(dec!(-10));
        manager.record_trade(dec!(-10));
        assert!(manager.is_trading_enabled()); // Still enabled at 2

        manager.record_trade(dec!(-10));
        assert!(!manager.is_trading_enabled()); // Disabled at 3
        assert_eq!(manager.consecutive_losses(), 3);
    }

    #[test]
    fn test_auto_disable_daily_loss_limit() {
        let mut manager = default_manager(); // Limit: $500

        manager.record_trade(dec!(-400));
        assert!(manager.is_trading_enabled());

        manager.record_trade(dec!(-100)); // Now at -500
        assert!(!manager.is_trading_enabled());
        assert_eq!(manager.daily_pnl(), dec!(-500));
    }

    #[test]
    fn test_win_rate() {
        let mut manager = default_manager();
        assert_eq!(manager.win_rate(), dec!(0)); // No trades

        manager.record_trade(dec!(10));  // Win
        assert_eq!(manager.win_rate(), dec!(1)); // 100%

        manager.record_trade(dec!(-10)); // Lose
        assert_eq!(manager.win_rate(), dec!(0.5)); // 50%

        manager.record_trade(dec!(10));  // Win
        // 2 wins, 1 loss = 2/3 = 0.666...
        let rate = manager.win_rate();
        assert!(rate > dec!(0.66) && rate < dec!(0.67));
    }

    // =========================================================================
    // Check Trade Tests
    // =========================================================================

    #[test]
    fn test_check_trade_approve() {
        let manager = default_manager();
        let decision = manager.check_trade(None, dec!(100));
        assert_eq!(decision, TradeDecision::Approve);
    }

    #[test]
    fn test_check_trade_reject_disabled() {
        let mut manager = default_manager();
        manager.disable_trading();

        let decision = manager.check_trade(None, dec!(100));
        assert!(matches!(decision, TradeDecision::Reject { .. }));
        assert!(decision.is_blocked());
    }

    #[test]
    fn test_check_trade_reject_daily_limit() {
        let mut manager = default_manager();
        // Record one big loss to hit daily limit (not 3 small ones which would
        // trigger consecutive loss first)
        manager.record_trade(dec!(-500)); // Hit limit

        // Manager is auto-disabled; re-enable to test the daily limit check
        manager.enable_trading();

        let decision = manager.check_trade(None, dec!(100));
        assert!(decision.is_blocked());
        if let TradeDecision::Reject { reason } = &decision {
            assert!(reason.contains("Daily loss limit"), "Got: {}", reason);
        } else {
            panic!("Expected Reject");
        }
    }

    #[test]
    fn test_check_trade_reject_consecutive_losses() {
        let mut manager = default_manager();
        // Record 3 losses (hitting limit) but not exceeding daily loss
        manager.record_trade(dec!(-50));
        manager.record_trade(dec!(-50));
        manager.record_trade(dec!(-50));

        // Manager is auto-disabled, re-enable to test the consecutive check
        manager.enable_trading();

        let decision = manager.check_trade(None, dec!(100));
        assert!(decision.is_blocked());
        if let TradeDecision::Reject { reason } = &decision {
            assert!(reason.contains("Consecutive loss limit"));
        } else {
            panic!("Expected Reject");
        }
    }

    #[test]
    fn test_check_trade_rebalance_required() {
        let manager = default_manager();

        // Position with low hedge ratio (15% on down side)
        let position = Position {
            up_shares: dec!(850),
            down_shares: dec!(150),
            up_cost: dec!(425),  // 85%
            down_cost: dec!(75), // 15%
        };
        assert_eq!(position.min_side_ratio(), dec!(0.15));

        let decision = manager.check_trade(Some(&position), dec!(100));
        assert!(decision.requires_rebalance());

        if let TradeDecision::RebalanceRequired { current_ratio, target_ratio } = decision {
            assert_eq!(current_ratio, dec!(0.15));
            assert_eq!(target_ratio, dec!(0.20));
        } else {
            panic!("Expected RebalanceRequired");
        }
    }

    #[test]
    fn test_check_trade_balanced_position_ok() {
        let manager = default_manager();

        // Balanced position (50/50)
        let position = Position {
            up_shares: dec!(100),
            down_shares: dec!(100),
            up_cost: dec!(50),
            down_cost: dec!(50),
        };
        assert_eq!(position.min_side_ratio(), dec!(0.50));

        let decision = manager.check_trade(Some(&position), dec!(100));
        assert_eq!(decision, TradeDecision::Approve);
    }

    #[test]
    fn test_check_trade_empty_position_ok() {
        let manager = default_manager();
        let position = Position::new(); // Empty

        let decision = manager.check_trade(Some(&position), dec!(100));
        assert_eq!(decision, TradeDecision::Approve);
    }

    #[test]
    fn test_check_trade_reduce_size_warning() {
        let mut manager = default_manager(); // Limit: $500

        // Lose $400 = 80% of limit, past 75% warning threshold
        manager.record_trade(dec!(-400));
        assert!(manager.is_approaching_limit());

        let decision = manager.check_trade(None, dec!(100));

        if let TradeDecision::ReduceSize { max_allowed } = decision {
            // 100 * 0.50 = 50
            assert_eq!(max_allowed, dec!(50));
        } else {
            panic!("Expected ReduceSize, got {:?}", decision);
        }
    }

    #[test]
    fn test_check_trade_under_warning_threshold() {
        let mut manager = default_manager(); // Limit: $500

        // Lose $300 = 60% of limit, under 75% warning threshold
        manager.record_trade(dec!(-300));
        assert!(!manager.is_approaching_limit());

        let decision = manager.check_trade(None, dec!(100));
        assert_eq!(decision, TradeDecision::Approve);
    }

    // =========================================================================
    // Can Trade Tests
    // =========================================================================

    #[test]
    fn test_can_trade_true() {
        let manager = default_manager();
        assert!(manager.can_trade());
    }

    #[test]
    fn test_can_trade_disabled() {
        let mut manager = default_manager();
        manager.disable_trading();
        assert!(!manager.can_trade());
    }

    #[test]
    fn test_can_trade_daily_limit() {
        let mut manager = default_manager();
        manager.record_trade(dec!(-500));
        assert!(!manager.can_trade());
    }

    #[test]
    fn test_can_trade_consecutive_losses() {
        let mut manager = default_manager();
        manager.record_trade(dec!(-50));
        manager.record_trade(dec!(-50));
        manager.record_trade(dec!(-50));
        assert!(!manager.can_trade());
    }

    // =========================================================================
    // Budget and Limit Tests
    // =========================================================================

    #[test]
    fn test_remaining_loss_budget_positive_pnl() {
        let mut manager = default_manager(); // Limit: $500
        manager.record_trade(dec!(100)); // $100 profit

        // With $100 profit, we can lose $600 before hitting -$500
        assert_eq!(manager.remaining_loss_budget(), dec!(600));
    }

    #[test]
    fn test_remaining_loss_budget_negative_pnl() {
        let mut manager = default_manager(); // Limit: $500
        manager.record_trade(dec!(-100)); // $100 loss

        // With $100 loss, we can lose $400 more before hitting -$500
        assert_eq!(manager.remaining_loss_budget(), dec!(400));
    }

    #[test]
    fn test_remaining_loss_budget_at_limit() {
        let mut manager = default_manager();
        manager.record_trade(dec!(-500));
        assert_eq!(manager.remaining_loss_budget(), dec!(0));
    }

    #[test]
    fn test_remaining_loss_budget_over_limit() {
        let mut manager = default_manager();
        // Force over limit by direct manipulation (shouldn't happen normally)
        manager.daily_pnl = dec!(-600);
        assert_eq!(manager.remaining_loss_budget(), dec!(0));
    }

    #[test]
    fn test_is_approaching_limit() {
        let mut manager = default_manager(); // Limit: $500

        // No loss yet
        assert!(!manager.is_approaching_limit());

        // $300 loss = 60%, under 75%
        manager.record_trade(dec!(-300));
        assert!(!manager.is_approaching_limit());

        // $100 more = $400 loss = 80%, over 75%
        manager.record_trade(dec!(-100));
        assert!(manager.is_approaching_limit());
    }

    #[test]
    fn test_is_approaching_limit_with_profit() {
        let mut manager = default_manager();
        manager.record_trade(dec!(1000)); // Big profit
        assert!(!manager.is_approaching_limit());
    }

    // =========================================================================
    // Reset Tests
    // =========================================================================

    #[test]
    fn test_reset_daily() {
        let mut manager = default_manager();

        // Accumulate some state
        manager.record_trade(dec!(-50));
        manager.record_trade(dec!(-50));
        manager.record_trade(dec!(20));
        manager.disable_trading();

        // Reset
        manager.reset_daily();

        assert_eq!(manager.daily_pnl(), dec!(0));
        assert_eq!(manager.consecutive_losses(), 0);
        assert!(manager.is_trading_enabled());
        assert_eq!(manager.trade_count(), 0);
        assert_eq!(manager.winning_trades(), 0);
        assert_eq!(manager.losing_trades(), 0);
    }

    #[test]
    fn test_reset_consecutive_losses() {
        let mut manager = default_manager();

        // Hit consecutive loss limit
        manager.record_trade(dec!(-50));
        manager.record_trade(dec!(-50));
        manager.record_trade(dec!(-50));
        assert!(!manager.is_trading_enabled());

        // Reset consecutive losses
        manager.reset_consecutive_losses();

        assert_eq!(manager.consecutive_losses(), 0);
        assert!(manager.is_trading_enabled()); // Re-enabled since daily limit not hit
        // Daily PnL and trade counts preserved
        assert_eq!(manager.daily_pnl(), dec!(-150));
        assert_eq!(manager.trade_count(), 3);
    }

    #[test]
    fn test_reset_consecutive_stays_disabled_if_daily_limit() {
        let mut manager = default_manager();

        // Hit daily loss limit
        manager.record_trade(dec!(-500));
        assert!(!manager.is_trading_enabled());

        // Reset consecutive losses - should stay disabled
        manager.reset_consecutive_losses();

        assert_eq!(manager.consecutive_losses(), 0);
        assert!(!manager.is_trading_enabled()); // Still disabled
    }

    // =========================================================================
    // Enable/Disable Tests
    // =========================================================================

    #[test]
    fn test_enable_disable_trading() {
        let mut manager = default_manager();
        assert!(manager.is_trading_enabled());

        manager.disable_trading();
        assert!(!manager.is_trading_enabled());

        manager.enable_trading();
        assert!(manager.is_trading_enabled());
    }

    #[test]
    fn test_update_balance() {
        let mut manager = default_manager();
        assert_eq!(manager.daily_loss_limit(), dec!(500)); // 5000 * 0.10

        manager.update_balance(dec!(10000));
        assert_eq!(manager.daily_loss_limit(), dec!(1000)); // 10000 * 0.10
    }

    // =========================================================================
    // Stats Tests
    // =========================================================================

    #[test]
    fn test_stats() {
        let mut manager = default_manager();
        manager.record_trade(dec!(10));
        manager.record_trade(dec!(-5));

        let stats = manager.stats();

        assert_eq!(stats.daily_pnl, dec!(5));
        assert_eq!(stats.daily_loss_limit, dec!(500));
        assert_eq!(stats.remaining_budget, dec!(505)); // 500 + 5 profit
        assert_eq!(stats.consecutive_losses, 1);
        assert_eq!(stats.max_consecutive_losses, 3);
        assert_eq!(stats.trade_count, 2);
        assert_eq!(stats.winning_trades, 1);
        assert_eq!(stats.losing_trades, 1);
        assert_eq!(stats.win_rate, dec!(0.5));
        assert!(stats.trading_enabled);
    }

    #[test]
    fn test_stats_display() {
        let manager = default_manager();
        let stats = manager.stats();
        let display = stats.to_string();

        assert!(display.contains("PnL:"));
        assert!(display.contains("Trades:"));
        assert!(display.contains("Win rate:"));
        assert!(display.contains("Enabled:"));
    }

    #[test]
    fn test_stats_serialization() {
        let mut manager = default_manager();
        manager.record_trade(dec!(10));

        let stats = manager.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let parsed: PnlRiskStats = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.daily_pnl, stats.daily_pnl);
        assert_eq!(parsed.trade_count, stats.trade_count);
    }

    // =========================================================================
    // PnlRejectionReason Tests
    // =========================================================================

    #[test]
    fn test_rejection_reason_display() {
        let reason = PnlRejectionReason::DailyLossLimit {
            current_pnl: dec!(-500),
            limit: dec!(500),
        };
        let display = reason.to_string();
        assert!(display.contains("Daily loss limit"));
        assert!(display.contains("$500"));

        let reason = PnlRejectionReason::ConsecutiveLosses {
            count: 3,
            max: 3,
        };
        let display = reason.to_string();
        assert!(display.contains("Consecutive loss limit"));
        assert!(display.contains("3"));

        let reason = PnlRejectionReason::TradingDisabled;
        let display = reason.to_string();
        assert!(display.contains("disabled"));
    }

    #[test]
    fn test_rejection_reason_serialization() {
        let reason = PnlRejectionReason::DailyLossLimit {
            current_pnl: dec!(-500),
            limit: dec!(500),
        };
        let json = serde_json::to_string(&reason).unwrap();
        let parsed: PnlRejectionReason = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, reason);
    }

    // =========================================================================
    // Integration/Scenario Tests
    // =========================================================================

    #[test]
    fn test_scenario_typical_trading_day() {
        let mut manager = default_manager();

        // Morning: some wins
        manager.record_trade(dec!(5));
        manager.record_trade(dec!(8));
        manager.record_trade(dec!(-3));
        assert!(manager.can_trade());
        assert_eq!(manager.daily_pnl(), dec!(10));
        assert_eq!(manager.consecutive_losses(), 1); // First loss

        // Midday: more losses (building up consecutive)
        manager.record_trade(dec!(-20));
        assert!(manager.can_trade());
        assert_eq!(manager.consecutive_losses(), 2);

        // Third consecutive loss - triggers auto-disable
        manager.record_trade(dec!(-15));
        assert!(!manager.can_trade()); // 3 consecutive losses
        assert_eq!(manager.consecutive_losses(), 3);

        // Manual reset by operator
        manager.reset_consecutive_losses();
        assert!(manager.can_trade());
        assert_eq!(manager.consecutive_losses(), 0);

        // One more trade to end the day
        manager.record_trade(dec!(-10));
        assert!(manager.can_trade()); // Only 1 consecutive now

        // End of day
        let stats = manager.stats();
        assert!(stats.daily_pnl < Decimal::ZERO);
        assert_eq!(stats.trade_count, 6);
    }

    #[test]
    fn test_scenario_hit_daily_limit_early() {
        let mut manager = PnlRiskManager::with_balance(dec!(1000)); // Limit: $100

        // One big loss
        manager.record_trade(dec!(-100));

        assert!(!manager.can_trade());
        assert!(!manager.is_trading_enabled());

        // Even resetting consecutive losses won't help
        manager.reset_consecutive_losses();
        assert!(!manager.can_trade());

        // Only a new day helps
        manager.reset_daily();
        assert!(manager.can_trade());
    }

    #[test]
    fn test_scenario_position_hedge_check() {
        let manager = default_manager();

        // Build up unbalanced position over trades
        let balanced = Position {
            up_shares: dec!(100),
            down_shares: dec!(100),
            up_cost: dec!(50),
            down_cost: dec!(50),
        };

        // Balanced - OK
        let decision = manager.check_trade(Some(&balanced), dec!(10));
        assert_eq!(decision, TradeDecision::Approve);

        // Getting unbalanced (25% hedge)
        let slightly_unbalanced = Position {
            up_shares: dec!(150),
            down_shares: dec!(50),
            up_cost: dec!(75),  // 75%
            down_cost: dec!(25), // 25%
        };

        // 25% >= 20% minimum, still OK
        let decision = manager.check_trade(Some(&slightly_unbalanced), dec!(10));
        assert_eq!(decision, TradeDecision::Approve);

        // Very unbalanced (10% hedge)
        let very_unbalanced = Position {
            up_shares: dec!(180),
            down_shares: dec!(20),
            up_cost: dec!(90),  // 90%
            down_cost: dec!(10), // 10%
        };

        // 10% < 20% minimum, needs rebalance
        let decision = manager.check_trade(Some(&very_unbalanced), dec!(10));
        assert!(decision.requires_rebalance());
    }
}
