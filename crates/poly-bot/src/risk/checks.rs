//! Pre-trade risk checks for the trading bot.
//!
//! All checks run before order submission to prevent exceeding risk limits.
//! Each check returns a rejection reason if the trade should be blocked.
//!
//! ## Checks Implemented
//!
//! 1. **Time remaining**: Must have >30s before window close
//! 2. **Position size**: Cannot exceed max_position_per_market
//! 3. **Total exposure**: Cannot exceed max_total_exposure across all markets
//! 4. **Imbalance ratio**: Cannot exceed max_imbalance_ratio for inventory
//! 5. **Daily loss**: Cannot exceed max_daily_loss
//! 6. **Toxic severity**: Cannot exceed toxic_flow_threshold

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::config::RiskConfig;
use crate::strategy::ToxicSeverity;
use crate::types::InventoryState;

/// Configuration for risk checks.
#[derive(Debug, Clone)]
pub struct RiskCheckConfig {
    /// Minimum time remaining in window to trade (seconds).
    pub min_time_remaining_secs: u64,

    /// Maximum position size per market (USDC).
    pub max_position_per_market: Decimal,

    /// Maximum total exposure across all markets (USDC).
    pub max_total_exposure: Decimal,

    /// Maximum inventory imbalance ratio (0.0-1.0).
    pub max_imbalance_ratio: Decimal,

    /// Maximum daily loss before stopping (USDC).
    pub max_daily_loss: Decimal,

    /// Toxic flow severity threshold (0-100).
    /// Trades with severity >= this value are blocked.
    pub toxic_flow_threshold: u8,

    /// Emergency close threshold for unhedged exposure (USDC).
    pub emergency_close_threshold: Decimal,
}

impl Default for RiskCheckConfig {
    fn default() -> Self {
        Self {
            min_time_remaining_secs: 30,
            max_position_per_market: Decimal::new(1000, 0),
            max_total_exposure: Decimal::new(5000, 0),
            max_imbalance_ratio: Decimal::new(7, 1), // 0.7
            max_daily_loss: Decimal::new(500, 0),
            toxic_flow_threshold: 80,
            emergency_close_threshold: Decimal::new(200, 0),
        }
    }
}

impl RiskCheckConfig {
    /// Create from RiskConfig and trading min_time_remaining_secs.
    pub fn from_configs(risk: &RiskConfig, min_time_remaining_secs: u64) -> Self {
        Self {
            min_time_remaining_secs,
            max_position_per_market: risk.emergency_close_threshold, // Use position limit from trading config
            max_total_exposure: risk.max_daily_loss * Decimal::new(10, 0), // 10x daily loss as total exposure
            max_imbalance_ratio: risk.max_imbalance_ratio,
            max_daily_loss: risk.max_daily_loss,
            toxic_flow_threshold: risk.toxic_flow_threshold,
            emergency_close_threshold: risk.emergency_close_threshold,
        }
    }

    /// Create with explicit trading limits.
    pub fn with_limits(
        risk: &RiskConfig,
        min_time_remaining_secs: u64,
        max_position_per_market: Decimal,
        max_total_exposure: Decimal,
    ) -> Self {
        Self {
            min_time_remaining_secs,
            max_position_per_market,
            max_total_exposure,
            max_imbalance_ratio: risk.max_imbalance_ratio,
            max_daily_loss: risk.max_daily_loss,
            toxic_flow_threshold: risk.toxic_flow_threshold,
            emergency_close_threshold: risk.emergency_close_threshold,
        }
    }
}

/// Context for pre-trade checks.
///
/// Contains all the information needed to evaluate risk checks.
#[derive(Debug, Clone)]
pub struct PreTradeCheck {
    /// Event ID for the market.
    pub event_id: String,

    /// Proposed order size (shares).
    pub order_size: Decimal,

    /// Proposed order price.
    pub order_price: Decimal,

    /// Seconds remaining in the window.
    pub seconds_remaining: i64,

    /// Current position size in this market (USDC cost basis).
    pub current_position: Decimal,

    /// Total exposure across all markets (USDC).
    pub total_exposure: Decimal,

    /// Current inventory imbalance ratio for this market.
    pub imbalance_ratio: Decimal,

    /// Inventory state classification.
    pub inventory_state: InventoryState,

    /// Daily P&L so far (USDC, negative = loss).
    pub daily_pnl: Decimal,

    /// Toxic flow severity (if any warning exists).
    pub toxic_severity: Option<ToxicSeverity>,
}

impl PreTradeCheck {
    /// Create a new pre-trade check context.
    pub fn new(event_id: String, order_size: Decimal, order_price: Decimal) -> Self {
        Self {
            event_id,
            order_size,
            order_price,
            seconds_remaining: i64::MAX, // Default: plenty of time
            current_position: Decimal::ZERO,
            total_exposure: Decimal::ZERO,
            imbalance_ratio: Decimal::ZERO,
            inventory_state: InventoryState::Balanced,
            daily_pnl: Decimal::ZERO,
            toxic_severity: None,
        }
    }

    /// Set time remaining.
    pub fn with_time_remaining(mut self, seconds: i64) -> Self {
        self.seconds_remaining = seconds;
        self
    }

    /// Set current position.
    pub fn with_current_position(mut self, position: Decimal) -> Self {
        self.current_position = position;
        self
    }

    /// Set total exposure.
    pub fn with_total_exposure(mut self, exposure: Decimal) -> Self {
        self.total_exposure = exposure;
        self
    }

    /// Set imbalance ratio and state.
    pub fn with_imbalance(mut self, ratio: Decimal, state: InventoryState) -> Self {
        self.imbalance_ratio = ratio;
        self.inventory_state = state;
        self
    }

    /// Set daily P&L.
    pub fn with_daily_pnl(mut self, pnl: Decimal) -> Self {
        self.daily_pnl = pnl;
        self
    }

    /// Set toxic severity.
    pub fn with_toxic_severity(mut self, severity: Option<ToxicSeverity>) -> Self {
        self.toxic_severity = severity;
        self
    }

    /// Calculate order cost.
    #[inline]
    pub fn order_cost(&self) -> Decimal {
        self.order_size * self.order_price
    }

    /// Calculate new position after this order.
    #[inline]
    pub fn new_position(&self) -> Decimal {
        self.current_position + self.order_cost()
    }

    /// Calculate new total exposure after this order.
    #[inline]
    pub fn new_total_exposure(&self) -> Decimal {
        self.total_exposure + self.order_cost()
    }
}

/// Reason why a pre-trade check rejected the trade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreTradeRejection {
    /// Insufficient time remaining in window.
    InsufficientTime {
        remaining_secs: i64,
        min_required_secs: u64,
    },

    /// Would exceed position limit for this market.
    PositionLimitExceeded {
        current: Decimal,
        order_cost: Decimal,
        max_allowed: Decimal,
    },

    /// Would exceed total exposure limit.
    ExposureLimitExceeded {
        current: Decimal,
        order_cost: Decimal,
        max_allowed: Decimal,
    },

    /// Inventory imbalance too high.
    ImbalanceTooHigh {
        current_ratio: Decimal,
        max_allowed: Decimal,
        state: InventoryState,
    },

    /// Daily loss limit reached.
    DailyLossLimitReached {
        current_loss: Decimal,
        max_allowed: Decimal,
    },

    /// Toxic flow severity too high.
    ToxicFlowBlocked {
        severity: ToxicSeverity,
        threshold: u8,
    },

    /// Inventory in crisis state - must hedge.
    InventoryCrisis,

    /// Order size is zero or negative.
    InvalidOrderSize { size: Decimal },

    /// Order price is invalid.
    InvalidOrderPrice { price: Decimal },
}

impl PreTradeRejection {
    /// Get a short code for this rejection.
    pub fn code(&self) -> &'static str {
        match self {
            PreTradeRejection::InsufficientTime { .. } => "TIME",
            PreTradeRejection::PositionLimitExceeded { .. } => "POS_LIMIT",
            PreTradeRejection::ExposureLimitExceeded { .. } => "EXP_LIMIT",
            PreTradeRejection::ImbalanceTooHigh { .. } => "IMBALANCE",
            PreTradeRejection::DailyLossLimitReached { .. } => "DAILY_LOSS",
            PreTradeRejection::ToxicFlowBlocked { .. } => "TOXIC",
            PreTradeRejection::InventoryCrisis => "CRISIS",
            PreTradeRejection::InvalidOrderSize { .. } => "BAD_SIZE",
            PreTradeRejection::InvalidOrderPrice { .. } => "BAD_PRICE",
        }
    }

    /// Check if this rejection is recoverable (might pass later).
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            PreTradeRejection::InsufficientTime { .. }
                | PreTradeRejection::ToxicFlowBlocked { .. }
                | PreTradeRejection::InventoryCrisis
        )
    }
}

impl std::fmt::Display for PreTradeRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreTradeRejection::InsufficientTime {
                remaining_secs,
                min_required_secs,
            } => {
                write!(
                    f,
                    "Insufficient time: {}s remaining, {}s required",
                    remaining_secs, min_required_secs
                )
            }
            PreTradeRejection::PositionLimitExceeded {
                current,
                order_cost,
                max_allowed,
            } => {
                write!(
                    f,
                    "Position limit exceeded: ${} + ${} > ${}",
                    current, order_cost, max_allowed
                )
            }
            PreTradeRejection::ExposureLimitExceeded {
                current,
                order_cost,
                max_allowed,
            } => {
                write!(
                    f,
                    "Exposure limit exceeded: ${} + ${} > ${}",
                    current, order_cost, max_allowed
                )
            }
            PreTradeRejection::ImbalanceTooHigh {
                current_ratio,
                max_allowed,
                state,
            } => {
                write!(
                    f,
                    "Imbalance too high: {} > {} ({:?})",
                    current_ratio, max_allowed, state
                )
            }
            PreTradeRejection::DailyLossLimitReached {
                current_loss,
                max_allowed,
            } => {
                write!(
                    f,
                    "Daily loss limit reached: ${} >= ${}",
                    current_loss.abs(),
                    max_allowed
                )
            }
            PreTradeRejection::ToxicFlowBlocked { severity, threshold } => {
                write!(
                    f,
                    "Toxic flow blocked: {} severity >= {} threshold",
                    severity, threshold
                )
            }
            PreTradeRejection::InventoryCrisis => {
                write!(f, "Inventory in crisis state - hedge required")
            }
            PreTradeRejection::InvalidOrderSize { size } => {
                write!(f, "Invalid order size: {}", size)
            }
            PreTradeRejection::InvalidOrderPrice { price } => {
                write!(f, "Invalid order price: {}", price)
            }
        }
    }
}

/// Result of running all pre-trade checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskCheckResult {
    /// Whether all checks passed.
    pub passed: bool,

    /// List of rejections (empty if passed).
    pub rejections: Vec<PreTradeRejection>,

    /// Checks that passed.
    pub passed_checks: Vec<String>,

    /// Recommended size adjustment (1.0 = no change).
    pub size_adjustment: Decimal,
}

impl RiskCheckResult {
    /// Create a passing result.
    pub fn pass(passed_checks: Vec<&str>) -> Self {
        Self {
            passed: true,
            rejections: Vec::new(),
            passed_checks: passed_checks.into_iter().map(String::from).collect(),
            size_adjustment: Decimal::ONE,
        }
    }

    /// Create a failing result.
    pub fn fail(rejections: Vec<PreTradeRejection>, passed_checks: Vec<&str>) -> Self {
        Self {
            passed: false,
            rejections,
            passed_checks: passed_checks.into_iter().map(String::from).collect(),
            size_adjustment: Decimal::ZERO,
        }
    }

    /// Create a pass with size adjustment.
    pub fn pass_with_adjustment(passed_checks: Vec<&str>, adjustment: Decimal) -> Self {
        Self {
            passed: true,
            rejections: Vec::new(),
            passed_checks: passed_checks.into_iter().map(String::from).collect(),
            size_adjustment: adjustment,
        }
    }

    /// Create a passing result (internal, with owned strings).
    fn pass_internal(passed_checks: Vec<String>) -> Self {
        Self {
            passed: true,
            rejections: Vec::new(),
            passed_checks,
            size_adjustment: Decimal::ONE,
        }
    }

    /// Create a failing result (internal, with owned strings).
    fn fail_internal(rejections: Vec<PreTradeRejection>, passed_checks: Vec<String>) -> Self {
        Self {
            passed: false,
            rejections,
            passed_checks,
            size_adjustment: Decimal::ZERO,
        }
    }

    /// Create a pass with size adjustment (internal, with owned strings).
    fn pass_with_adjustment_internal(passed_checks: Vec<String>, adjustment: Decimal) -> Self {
        Self {
            passed: true,
            rejections: Vec::new(),
            passed_checks,
            size_adjustment: adjustment,
        }
    }

    /// Get the primary rejection reason (if any).
    pub fn primary_rejection(&self) -> Option<&PreTradeRejection> {
        self.rejections.first()
    }

    /// Check if any rejection is non-recoverable.
    pub fn has_hard_rejection(&self) -> bool {
        self.rejections.iter().any(|r| !r.is_recoverable())
    }
}

/// Pre-trade risk checker.
///
/// Runs all configured checks against a proposed trade.
#[derive(Debug, Clone)]
pub struct RiskChecker {
    config: RiskCheckConfig,
}

impl Default for RiskChecker {
    fn default() -> Self {
        Self::new(RiskCheckConfig::default())
    }
}

impl RiskChecker {
    /// Create a new risk checker.
    pub fn new(config: RiskCheckConfig) -> Self {
        Self { config }
    }

    /// Get the configuration.
    pub fn config(&self) -> &RiskCheckConfig {
        &self.config
    }

    /// Run all pre-trade checks.
    ///
    /// Returns Ok(result) with pass/fail and any rejections.
    pub fn check(&self, trade: &PreTradeCheck) -> RiskCheckResult {
        let mut rejections = Vec::new();
        let mut passed_checks: Vec<String> = Vec::new();

        // 1. Validate order parameters
        if let Some(rejection) = self.check_order_validity(trade) {
            rejections.push(rejection);
        } else {
            passed_checks.push("order_validity".to_string());
        }

        // 2. Check time remaining
        if let Some(rejection) = self.check_time_remaining(trade) {
            rejections.push(rejection);
        } else {
            passed_checks.push("time_remaining".to_string());
        }

        // 3. Check position limit
        if let Some(rejection) = self.check_position_limit(trade) {
            rejections.push(rejection);
        } else {
            passed_checks.push("position_limit".to_string());
        }

        // 4. Check total exposure
        if let Some(rejection) = self.check_exposure_limit(trade) {
            rejections.push(rejection);
        } else {
            passed_checks.push("exposure_limit".to_string());
        }

        // 5. Check imbalance
        if let Some(rejection) = self.check_imbalance(trade) {
            rejections.push(rejection);
        } else {
            passed_checks.push("imbalance".to_string());
        }

        // 6. Check daily loss
        if let Some(rejection) = self.check_daily_loss(trade) {
            rejections.push(rejection);
        } else {
            passed_checks.push("daily_loss".to_string());
        }

        // 7. Check toxic flow
        if let Some(rejection) = self.check_toxic_flow(trade) {
            rejections.push(rejection);
        } else {
            passed_checks.push("toxic_flow".to_string());
        }

        if rejections.is_empty() {
            // Calculate any size adjustment based on inventory state
            let adjustment = trade.inventory_state.size_multiplier();
            if adjustment < Decimal::ONE {
                RiskCheckResult::pass_with_adjustment_internal(passed_checks, adjustment)
            } else {
                RiskCheckResult::pass_internal(passed_checks)
            }
        } else {
            RiskCheckResult::fail_internal(rejections, passed_checks)
        }
    }

    /// Quick check if a trade can pass (for fast filtering).
    ///
    /// Returns true if the trade would likely pass all checks.
    #[inline]
    pub fn can_trade(&self, trade: &PreTradeCheck) -> bool {
        // Fast path: check most common rejection reasons
        trade.seconds_remaining >= self.config.min_time_remaining_secs as i64
            && trade.new_total_exposure() <= self.config.max_total_exposure
            && trade.daily_pnl > -self.config.max_daily_loss
            && !matches!(trade.inventory_state, InventoryState::Crisis)
    }

    /// Check order validity (size and price).
    fn check_order_validity(&self, trade: &PreTradeCheck) -> Option<PreTradeRejection> {
        if trade.order_size <= Decimal::ZERO {
            return Some(PreTradeRejection::InvalidOrderSize {
                size: trade.order_size,
            });
        }
        if trade.order_price <= Decimal::ZERO || trade.order_price > Decimal::ONE {
            return Some(PreTradeRejection::InvalidOrderPrice {
                price: trade.order_price,
            });
        }
        None
    }

    /// Check time remaining in window.
    fn check_time_remaining(&self, trade: &PreTradeCheck) -> Option<PreTradeRejection> {
        if trade.seconds_remaining < self.config.min_time_remaining_secs as i64 {
            return Some(PreTradeRejection::InsufficientTime {
                remaining_secs: trade.seconds_remaining,
                min_required_secs: self.config.min_time_remaining_secs,
            });
        }
        None
    }

    /// Check position limit for the market.
    fn check_position_limit(&self, trade: &PreTradeCheck) -> Option<PreTradeRejection> {
        let new_position = trade.new_position();
        if new_position > self.config.max_position_per_market {
            return Some(PreTradeRejection::PositionLimitExceeded {
                current: trade.current_position,
                order_cost: trade.order_cost(),
                max_allowed: self.config.max_position_per_market,
            });
        }
        None
    }

    /// Check total exposure limit.
    fn check_exposure_limit(&self, trade: &PreTradeCheck) -> Option<PreTradeRejection> {
        let new_exposure = trade.new_total_exposure();
        if new_exposure > self.config.max_total_exposure {
            return Some(PreTradeRejection::ExposureLimitExceeded {
                current: trade.total_exposure,
                order_cost: trade.order_cost(),
                max_allowed: self.config.max_total_exposure,
            });
        }
        None
    }

    /// Check inventory imbalance.
    fn check_imbalance(&self, trade: &PreTradeCheck) -> Option<PreTradeRejection> {
        // Crisis state always blocked
        if matches!(trade.inventory_state, InventoryState::Crisis) {
            return Some(PreTradeRejection::InventoryCrisis);
        }

        // Check ratio against limit
        if trade.imbalance_ratio > self.config.max_imbalance_ratio {
            return Some(PreTradeRejection::ImbalanceTooHigh {
                current_ratio: trade.imbalance_ratio,
                max_allowed: self.config.max_imbalance_ratio,
                state: trade.inventory_state,
            });
        }
        None
    }

    /// Check daily loss limit.
    fn check_daily_loss(&self, trade: &PreTradeCheck) -> Option<PreTradeRejection> {
        // P&L is negative for losses
        if trade.daily_pnl <= -self.config.max_daily_loss {
            return Some(PreTradeRejection::DailyLossLimitReached {
                current_loss: trade.daily_pnl.abs(),
                max_allowed: self.config.max_daily_loss,
            });
        }
        None
    }

    /// Check toxic flow severity.
    fn check_toxic_flow(&self, trade: &PreTradeCheck) -> Option<PreTradeRejection> {
        if let Some(severity) = trade.toxic_severity {
            let score = severity.score();
            if score >= self.config.toxic_flow_threshold {
                return Some(PreTradeRejection::ToxicFlowBlocked {
                    severity,
                    threshold: self.config.toxic_flow_threshold,
                });
            }
        }
        None
    }

    /// Calculate maximum allowed order size given current state.
    ///
    /// Returns the largest order that would pass all limit checks.
    pub fn max_order_size(&self, trade: &PreTradeCheck) -> Decimal {
        // Start with a large value
        let mut max_size = Decimal::new(1_000_000, 0);

        // Limit by position
        let position_room = self.config.max_position_per_market - trade.current_position;
        if position_room > Decimal::ZERO && trade.order_price > Decimal::ZERO {
            let position_limit = position_room / trade.order_price;
            max_size = max_size.min(position_limit);
        } else {
            return Decimal::ZERO;
        }

        // Limit by exposure
        let exposure_room = self.config.max_total_exposure - trade.total_exposure;
        if exposure_room > Decimal::ZERO && trade.order_price > Decimal::ZERO {
            let exposure_limit = exposure_room / trade.order_price;
            max_size = max_size.min(exposure_limit);
        } else {
            return Decimal::ZERO;
        }

        max_size.max(Decimal::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn default_config() -> RiskCheckConfig {
        RiskCheckConfig {
            min_time_remaining_secs: 30,
            max_position_per_market: dec!(1000),
            max_total_exposure: dec!(5000),
            max_imbalance_ratio: dec!(0.7),
            max_daily_loss: dec!(500),
            toxic_flow_threshold: 80,
            emergency_close_threshold: dec!(200),
        }
    }

    fn valid_trade() -> PreTradeCheck {
        PreTradeCheck::new("event1".to_string(), dec!(100), dec!(0.50))
            .with_time_remaining(300)
            .with_current_position(dec!(100))
            .with_total_exposure(dec!(500))
            .with_imbalance(dec!(0.1), InventoryState::Balanced)
            .with_daily_pnl(dec!(50))
    }

    #[test]
    fn test_config_default() {
        let config = RiskCheckConfig::default();
        assert_eq!(config.min_time_remaining_secs, 30);
        assert_eq!(config.max_position_per_market, dec!(1000));
        assert_eq!(config.toxic_flow_threshold, 80);
    }

    #[test]
    fn test_pre_trade_check_builder() {
        let check = PreTradeCheck::new("event1".to_string(), dec!(100), dec!(0.50))
            .with_time_remaining(60)
            .with_current_position(dec!(200))
            .with_total_exposure(dec!(1000))
            .with_imbalance(dec!(0.3), InventoryState::Skewed)
            .with_daily_pnl(dec!(-100))
            .with_toxic_severity(Some(ToxicSeverity::Low));

        assert_eq!(check.event_id, "event1");
        assert_eq!(check.order_size, dec!(100));
        assert_eq!(check.order_price, dec!(0.50));
        assert_eq!(check.seconds_remaining, 60);
        assert_eq!(check.current_position, dec!(200));
        assert_eq!(check.total_exposure, dec!(1000));
        assert_eq!(check.imbalance_ratio, dec!(0.3));
        assert_eq!(check.inventory_state, InventoryState::Skewed);
        assert_eq!(check.daily_pnl, dec!(-100));
        assert_eq!(check.toxic_severity, Some(ToxicSeverity::Low));
    }

    #[test]
    fn test_order_cost_calculation() {
        let check = PreTradeCheck::new("event1".to_string(), dec!(100), dec!(0.50));
        assert_eq!(check.order_cost(), dec!(50));
        assert_eq!(check.new_position(), dec!(50)); // 0 + 50
        assert_eq!(check.new_total_exposure(), dec!(50)); // 0 + 50
    }

    #[test]
    fn test_valid_trade_passes() {
        let checker = RiskChecker::new(default_config());
        let trade = valid_trade();

        let result = checker.check(&trade);
        assert!(result.passed);
        assert!(result.rejections.is_empty());
        assert_eq!(result.passed_checks.len(), 7);
    }

    #[test]
    fn test_insufficient_time() {
        let checker = RiskChecker::new(default_config());
        let trade = valid_trade().with_time_remaining(20);

        let result = checker.check(&trade);
        assert!(!result.passed);
        assert!(matches!(
            result.primary_rejection(),
            Some(PreTradeRejection::InsufficientTime { .. })
        ));
    }

    #[test]
    fn test_position_limit_exceeded() {
        let checker = RiskChecker::new(default_config());
        let trade = PreTradeCheck::new("event1".to_string(), dec!(2000), dec!(0.50))
            .with_time_remaining(300)
            .with_current_position(dec!(500));

        let result = checker.check(&trade);
        assert!(!result.passed);
        assert!(matches!(
            result.primary_rejection(),
            Some(PreTradeRejection::PositionLimitExceeded { .. })
        ));
    }

    #[test]
    fn test_exposure_limit_exceeded() {
        let checker = RiskChecker::new(default_config());
        let trade = PreTradeCheck::new("event1".to_string(), dec!(1000), dec!(0.50))
            .with_time_remaining(300)
            .with_total_exposure(dec!(4600)); // 4600 + 500 > 5000

        let result = checker.check(&trade);
        assert!(!result.passed);
        assert!(matches!(
            result.primary_rejection(),
            Some(PreTradeRejection::ExposureLimitExceeded { .. })
        ));
    }

    #[test]
    fn test_imbalance_too_high() {
        let checker = RiskChecker::new(default_config());
        let trade = valid_trade().with_imbalance(dec!(0.8), InventoryState::Exposed);

        let result = checker.check(&trade);
        assert!(!result.passed);
        assert!(matches!(
            result.primary_rejection(),
            Some(PreTradeRejection::ImbalanceTooHigh { .. })
        ));
    }

    #[test]
    fn test_inventory_crisis() {
        let checker = RiskChecker::new(default_config());
        let trade = valid_trade().with_imbalance(dec!(0.5), InventoryState::Crisis);

        let result = checker.check(&trade);
        assert!(!result.passed);
        assert!(matches!(
            result.primary_rejection(),
            Some(PreTradeRejection::InventoryCrisis)
        ));
    }

    #[test]
    fn test_daily_loss_limit() {
        let checker = RiskChecker::new(default_config());
        let trade = valid_trade().with_daily_pnl(dec!(-500));

        let result = checker.check(&trade);
        assert!(!result.passed);
        assert!(matches!(
            result.primary_rejection(),
            Some(PreTradeRejection::DailyLossLimitReached { .. })
        ));
    }

    #[test]
    fn test_toxic_flow_blocked() {
        // Use threshold of 75 so High (score=75) is blocked
        let config = RiskCheckConfig {
            toxic_flow_threshold: 75,
            ..default_config()
        };
        let checker = RiskChecker::new(config);
        let trade = valid_trade().with_toxic_severity(Some(ToxicSeverity::High));

        let result = checker.check(&trade);
        assert!(!result.passed);
        assert!(matches!(
            result.primary_rejection(),
            Some(PreTradeRejection::ToxicFlowBlocked { .. })
        ));
    }

    #[test]
    fn test_toxic_flow_low_passes() {
        let checker = RiskChecker::new(default_config());
        let trade = valid_trade().with_toxic_severity(Some(ToxicSeverity::Low));

        let result = checker.check(&trade);
        assert!(result.passed);
    }

    #[test]
    fn test_toxic_flow_medium_passes() {
        let checker = RiskChecker::new(default_config());
        let trade = valid_trade().with_toxic_severity(Some(ToxicSeverity::Medium));

        let result = checker.check(&trade);
        assert!(result.passed);
    }

    #[test]
    fn test_toxic_flow_critical_blocked() {
        let checker = RiskChecker::new(default_config());
        let trade = valid_trade().with_toxic_severity(Some(ToxicSeverity::Critical));

        let result = checker.check(&trade);
        assert!(!result.passed);
        if let Some(PreTradeRejection::ToxicFlowBlocked { severity, .. }) = result.primary_rejection() {
            assert_eq!(*severity, ToxicSeverity::Critical);
        } else {
            panic!("Expected ToxicFlowBlocked rejection");
        }
    }

    #[test]
    fn test_invalid_order_size() {
        let checker = RiskChecker::new(default_config());
        let trade = PreTradeCheck::new("event1".to_string(), dec!(0), dec!(0.50))
            .with_time_remaining(300);

        let result = checker.check(&trade);
        assert!(!result.passed);
        assert!(matches!(
            result.primary_rejection(),
            Some(PreTradeRejection::InvalidOrderSize { .. })
        ));
    }

    #[test]
    fn test_invalid_order_price() {
        let checker = RiskChecker::new(default_config());
        let trade = PreTradeCheck::new("event1".to_string(), dec!(100), dec!(1.5))
            .with_time_remaining(300);

        let result = checker.check(&trade);
        assert!(!result.passed);
        assert!(matches!(
            result.primary_rejection(),
            Some(PreTradeRejection::InvalidOrderPrice { .. })
        ));
    }

    #[test]
    fn test_rejection_code() {
        assert_eq!(
            PreTradeRejection::InsufficientTime {
                remaining_secs: 10,
                min_required_secs: 30
            }
            .code(),
            "TIME"
        );
        assert_eq!(
            PreTradeRejection::PositionLimitExceeded {
                current: dec!(0),
                order_cost: dec!(0),
                max_allowed: dec!(0)
            }
            .code(),
            "POS_LIMIT"
        );
        assert_eq!(PreTradeRejection::InventoryCrisis.code(), "CRISIS");
    }

    #[test]
    fn test_rejection_is_recoverable() {
        assert!(PreTradeRejection::InsufficientTime {
            remaining_secs: 10,
            min_required_secs: 30
        }
        .is_recoverable());

        assert!(PreTradeRejection::ToxicFlowBlocked {
            severity: ToxicSeverity::High,
            threshold: 80
        }
        .is_recoverable());

        assert!(PreTradeRejection::InventoryCrisis.is_recoverable());

        assert!(!PreTradeRejection::DailyLossLimitReached {
            current_loss: dec!(500),
            max_allowed: dec!(500)
        }
        .is_recoverable());
    }

    #[test]
    fn test_rejection_display() {
        let rejection = PreTradeRejection::InsufficientTime {
            remaining_secs: 10,
            min_required_secs: 30,
        };
        assert!(rejection.to_string().contains("10s remaining"));

        let rejection = PreTradeRejection::InventoryCrisis;
        assert!(rejection.to_string().contains("crisis"));
    }

    #[test]
    fn test_can_trade_fast_path() {
        let checker = RiskChecker::new(default_config());
        let trade = valid_trade();
        assert!(checker.can_trade(&trade));

        // Time too short
        let trade = valid_trade().with_time_remaining(10);
        assert!(!checker.can_trade(&trade));

        // Exposure too high (order cost 50 + 4960 = 5010 > 5000)
        let trade = valid_trade().with_total_exposure(dec!(4960));
        assert!(!checker.can_trade(&trade));

        // Daily loss exceeded
        let trade = valid_trade().with_daily_pnl(dec!(-500));
        assert!(!checker.can_trade(&trade));

        // Crisis state
        let trade = valid_trade().with_imbalance(dec!(0.1), InventoryState::Crisis);
        assert!(!checker.can_trade(&trade));
    }

    #[test]
    fn test_max_order_size() {
        let checker = RiskChecker::new(default_config());

        // Room in both limits
        let trade = PreTradeCheck::new("event1".to_string(), dec!(100), dec!(0.50))
            .with_current_position(dec!(500))
            .with_total_exposure(dec!(2000));

        let max = checker.max_order_size(&trade);
        // Position limit: (1000 - 500) / 0.50 = 1000
        // Exposure limit: (5000 - 2000) / 0.50 = 6000
        // Min = 1000
        assert_eq!(max, dec!(1000));
    }

    #[test]
    fn test_max_order_size_at_limits() {
        let checker = RiskChecker::new(default_config());

        // At position limit
        let trade = PreTradeCheck::new("event1".to_string(), dec!(100), dec!(0.50))
            .with_current_position(dec!(1000));

        let max = checker.max_order_size(&trade);
        assert_eq!(max, dec!(0));
    }

    #[test]
    fn test_size_adjustment_for_skewed() {
        let checker = RiskChecker::new(default_config());
        let trade = valid_trade().with_imbalance(dec!(0.3), InventoryState::Skewed);

        let result = checker.check(&trade);
        assert!(result.passed);
        // Skewed state should reduce size
        assert_eq!(result.size_adjustment, dec!(0.75));
    }

    #[test]
    fn test_size_adjustment_for_exposed() {
        let checker = RiskChecker::new(default_config());
        // Exposed state but within imbalance limit
        let trade = valid_trade().with_imbalance(dec!(0.6), InventoryState::Exposed);

        let result = checker.check(&trade);
        assert!(result.passed);
        assert_eq!(result.size_adjustment, dec!(0.50));
    }

    #[test]
    fn test_multiple_rejections() {
        let checker = RiskChecker::new(default_config());
        let trade = PreTradeCheck::new("event1".to_string(), dec!(100), dec!(0.50))
            .with_time_remaining(10)
            .with_daily_pnl(dec!(-600));

        let result = checker.check(&trade);
        assert!(!result.passed);
        assert!(result.rejections.len() >= 2);
    }

    #[test]
    fn test_result_has_hard_rejection() {
        let result = RiskCheckResult::fail(
            vec![PreTradeRejection::InsufficientTime {
                remaining_secs: 10,
                min_required_secs: 30,
            }],
            vec![],
        );
        assert!(!result.has_hard_rejection()); // Time is recoverable

        let result = RiskCheckResult::fail(
            vec![PreTradeRejection::DailyLossLimitReached {
                current_loss: dec!(500),
                max_allowed: dec!(500),
            }],
            vec![],
        );
        assert!(result.has_hard_rejection()); // Daily loss is hard
    }

    #[test]
    fn test_inventory_state_serialization() {
        let rejection = PreTradeRejection::ImbalanceTooHigh {
            current_ratio: dec!(0.8),
            max_allowed: dec!(0.7),
            state: InventoryState::Exposed,
        };

        let json = serde_json::to_string(&rejection).unwrap();
        assert!(json.contains("Exposed"));
    }
}
