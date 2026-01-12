//! Position sizing for arbitrage trades.
//!
//! This module calculates appropriate trade sizes based on:
//! - Configuration limits (max position per market, max total exposure)
//! - Available liquidity in the order book
//! - Current inventory imbalance
//! - Toxic flow warnings
//!
//! ## MVP Approach
//!
//! For MVP, we use fixed base sizing with adjustments:
//! 1. Start with `base_order_size` from config
//! 2. Limit by max_position_per_market (minus existing position)
//! 3. Limit by max_total_exposure (minus current total exposure)
//! 4. Limit by available liquidity (min of YES and NO ask sizes)
//! 5. Apply inventory imbalance multiplier
//! 6. Apply toxic flow severity multiplier (if any warning)

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::config::TradingConfig;
use crate::strategy::arb::ArbOpportunity;
use crate::strategy::toxic::ToxicFlowWarning;
use crate::types::{Inventory, InventoryState};

/// Configuration for position sizing.
#[derive(Debug, Clone)]
pub struct SizingConfig {
    /// Base order size (USDC).
    pub base_order_size: Decimal,

    /// Maximum position size per market (USDC).
    pub max_position_per_market: Decimal,

    /// Maximum total exposure across all markets (USDC).
    pub max_total_exposure: Decimal,

    /// Minimum order size (below this, skip the trade).
    pub min_order_size: Decimal,

    /// Maximum position as percentage of available liquidity (0.0-1.0).
    /// Prevents taking too much of the book and causing slippage.
    pub max_liquidity_take: Decimal,
}

impl Default for SizingConfig {
    fn default() -> Self {
        Self {
            base_order_size: Decimal::new(50, 0),       // $50
            max_position_per_market: Decimal::new(1000, 0), // $1000
            max_total_exposure: Decimal::new(5000, 0),      // $5000
            min_order_size: Decimal::new(5, 0),             // $5 minimum
            max_liquidity_take: Decimal::new(5, 1),         // 50% of book
        }
    }
}

impl SizingConfig {
    /// Create sizing config from trading config.
    pub fn from_trading_config(config: &TradingConfig) -> Self {
        Self {
            base_order_size: config.base_order_size,
            max_position_per_market: config.max_position_per_market,
            max_total_exposure: config.max_total_exposure,
            min_order_size: Decimal::new(5, 0),
            max_liquidity_take: Decimal::new(5, 1),
        }
    }
}

/// Result of position sizing calculation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizingResult {
    /// Calculated position size (in shares).
    pub size: Decimal,

    /// Whether this is a valid trade (size >= min_order_size).
    pub is_valid: bool,

    /// Expected cost for this size.
    pub expected_cost: Decimal,

    /// Expected profit for this size.
    pub expected_profit: Decimal,

    /// Reason if size was limited.
    pub limit_reason: Option<SizingLimit>,

    /// Adjustments applied during sizing.
    pub adjustments: SizingAdjustments,
}

impl SizingResult {
    /// Create an invalid result (trade should be skipped).
    pub fn invalid(reason: SizingLimit) -> Self {
        Self {
            size: Decimal::ZERO,
            is_valid: false,
            expected_cost: Decimal::ZERO,
            expected_profit: Decimal::ZERO,
            limit_reason: Some(reason),
            adjustments: SizingAdjustments::default(),
        }
    }
}

/// Reason why position size was limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizingLimit {
    /// Base order size (no limits hit).
    BaseSize,
    /// Limited by max_position_per_market.
    PositionLimit,
    /// Limited by max_total_exposure.
    ExposureLimit,
    /// Limited by available liquidity.
    LiquidityLimit,
    /// Limited by max_liquidity_take.
    LiquidityTakeLimit,
    /// Reduced due to inventory imbalance.
    ImbalanceAdjustment,
    /// Reduced due to toxic flow.
    ToxicFlowAdjustment,
    /// Below minimum order size.
    BelowMinimum,
    /// No valid opportunity.
    NoOpportunity,
}

impl std::fmt::Display for SizingLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SizingLimit::BaseSize => write!(f, "base size"),
            SizingLimit::PositionLimit => write!(f, "position limit"),
            SizingLimit::ExposureLimit => write!(f, "exposure limit"),
            SizingLimit::LiquidityLimit => write!(f, "liquidity limit"),
            SizingLimit::LiquidityTakeLimit => write!(f, "liquidity take limit"),
            SizingLimit::ImbalanceAdjustment => write!(f, "imbalance adjustment"),
            SizingLimit::ToxicFlowAdjustment => write!(f, "toxic flow adjustment"),
            SizingLimit::BelowMinimum => write!(f, "below minimum"),
            SizingLimit::NoOpportunity => write!(f, "no opportunity"),
        }
    }
}

/// Adjustments applied during sizing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizingAdjustments {
    /// Inventory state multiplier (1.0 = no adjustment).
    pub inventory_multiplier: Decimal,
    /// Toxic flow multiplier (1.0 = no adjustment).
    pub toxic_multiplier: Decimal,
    /// Combined multiplier applied.
    pub combined_multiplier: Decimal,
    /// Original size before adjustments.
    pub size_before_adjustments: Decimal,
}

/// Position sizer with configurable limits.
#[derive(Debug, Clone)]
pub struct PositionSizer {
    /// Configuration.
    config: SizingConfig,
}

impl Default for PositionSizer {
    fn default() -> Self {
        Self::new(SizingConfig::default())
    }
}

impl PositionSizer {
    /// Create a new position sizer with the given configuration.
    pub fn new(config: SizingConfig) -> Self {
        Self { config }
    }

    /// Create from trading config.
    pub fn from_trading_config(config: &TradingConfig) -> Self {
        Self::new(SizingConfig::from_trading_config(config))
    }

    /// Calculate position size for an arbitrage opportunity.
    ///
    /// # Arguments
    /// * `opportunity` - The detected arbitrage opportunity
    /// * `inventory` - Current inventory for this market (or None for new market)
    /// * `total_exposure` - Current total exposure across all markets
    /// * `toxic_warning` - Optional toxic flow warning
    ///
    /// # Returns
    /// `SizingResult` with the calculated size and metadata.
    pub fn calculate_size(
        &self,
        opportunity: &ArbOpportunity,
        inventory: Option<&Inventory>,
        total_exposure: Decimal,
        toxic_warning: Option<&ToxicFlowWarning>,
    ) -> SizingResult {
        // Start with base order size
        let mut size = self.config.base_order_size;
        let mut limit_reason = SizingLimit::BaseSize;

        // 1. Limit by available liquidity
        let available_liquidity = opportunity.max_size;
        if available_liquidity <= Decimal::ZERO {
            return SizingResult::invalid(SizingLimit::NoOpportunity);
        }

        if size > available_liquidity {
            size = available_liquidity;
            limit_reason = SizingLimit::LiquidityLimit;
        }

        // 2. Limit by max_liquidity_take (don't take more than X% of the book)
        let liquidity_take_limit = available_liquidity * self.config.max_liquidity_take;
        if size > liquidity_take_limit {
            size = liquidity_take_limit;
            limit_reason = SizingLimit::LiquidityTakeLimit;
        }

        // 3. Limit by max_position_per_market (minus existing position)
        let existing_position = inventory
            .map(|inv| inv.total_exposure())
            .unwrap_or(Decimal::ZERO);
        let remaining_position_capacity = self.config.max_position_per_market - existing_position;

        if remaining_position_capacity <= Decimal::ZERO {
            return SizingResult::invalid(SizingLimit::PositionLimit);
        }

        // Size is in shares, need to convert to cost equivalent
        // Cost = size * combined_cost (approximately)
        let cost_per_share = opportunity.combined_cost;
        let max_shares_by_position = remaining_position_capacity / cost_per_share;

        if size > max_shares_by_position {
            size = max_shares_by_position;
            limit_reason = SizingLimit::PositionLimit;
        }

        // 4. Limit by max_total_exposure (minus current total exposure)
        let remaining_exposure_capacity = self.config.max_total_exposure - total_exposure;

        if remaining_exposure_capacity <= Decimal::ZERO {
            return SizingResult::invalid(SizingLimit::ExposureLimit);
        }

        let max_shares_by_exposure = remaining_exposure_capacity / cost_per_share;
        if size > max_shares_by_exposure {
            size = max_shares_by_exposure;
            limit_reason = SizingLimit::ExposureLimit;
        }

        // Save size before adjustments
        let size_before_adjustments = size;

        // 5. Apply inventory imbalance adjustment
        let inventory_state = inventory.map(|inv| inv.state()).unwrap_or(InventoryState::Balanced);
        let inventory_multiplier = inventory_state.size_multiplier();

        // 6. Apply toxic flow adjustment
        let toxic_multiplier = toxic_warning
            .map(|w| w.severity.size_multiplier())
            .unwrap_or(Decimal::ONE);

        // Combined multiplier
        let combined_multiplier = inventory_multiplier * toxic_multiplier;

        // Apply adjustments
        let adjusted_size = size * combined_multiplier;

        // Track if adjustment was the limiting factor
        if adjusted_size < size {
            if inventory_multiplier < Decimal::ONE && toxic_multiplier >= inventory_multiplier {
                limit_reason = SizingLimit::ImbalanceAdjustment;
            } else if toxic_multiplier < Decimal::ONE {
                limit_reason = SizingLimit::ToxicFlowAdjustment;
            }
        }
        size = adjusted_size;

        // 7. Check minimum order size
        if size < self.config.min_order_size {
            return SizingResult::invalid(SizingLimit::BelowMinimum);
        }

        // Calculate expected cost and profit
        let expected_cost = size * cost_per_share;
        let expected_profit = size * opportunity.margin;

        SizingResult {
            size,
            is_valid: true,
            expected_cost,
            expected_profit,
            limit_reason: Some(limit_reason),
            adjustments: SizingAdjustments {
                inventory_multiplier,
                toxic_multiplier,
                combined_multiplier,
                size_before_adjustments,
            },
        }
    }

    /// Quick check if a trade is possible given current limits.
    ///
    /// Useful for fast filtering before full sizing calculation.
    pub fn can_trade(
        &self,
        existing_position: Decimal,
        total_exposure: Decimal,
    ) -> bool {
        // Check position limit
        if existing_position >= self.config.max_position_per_market {
            return false;
        }

        // Check exposure limit
        if total_exposure >= self.config.max_total_exposure {
            return false;
        }

        true
    }

    /// Get the configuration.
    pub fn config(&self) -> &SizingConfig {
        &self.config
    }

    /// Calculate the maximum possible size for a market.
    ///
    /// This doesn't account for liquidity or adjustments, just hard limits.
    pub fn max_possible_size(
        &self,
        existing_position: Decimal,
        total_exposure: Decimal,
        cost_per_share: Decimal,
    ) -> Decimal {
        if cost_per_share <= Decimal::ZERO {
            return Decimal::ZERO;
        }

        let remaining_position = self.config.max_position_per_market - existing_position;
        let remaining_exposure = self.config.max_total_exposure - total_exposure;

        let max_by_position = remaining_position / cost_per_share;
        let max_by_exposure = remaining_exposure / cost_per_share;

        max_by_position.min(max_by_exposure).max(Decimal::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::WindowPhase;
    use crate::strategy::toxic::{ToxicIndicators, ToxicSeverity};
    use poly_common::types::{CryptoAsset, Side};
    use rust_decimal_macros::dec;

    fn create_test_opportunity(
        yes_ask: Decimal,
        no_ask: Decimal,
        max_size: Decimal,
    ) -> ArbOpportunity {
        let combined_cost = yes_ask + no_ask;
        let margin = Decimal::ONE - combined_cost;
        ArbOpportunity {
            event_id: "event1".to_string(),
            asset: CryptoAsset::Btc,
            yes_token_id: "yes_token".to_string(),
            no_token_id: "no_token".to_string(),
            yes_ask,
            no_ask,
            combined_cost,
            margin,
            margin_bps: ((margin * dec!(10000)).try_into().unwrap_or(0)),
            max_size,
            seconds_remaining: 300,
            phase: WindowPhase::Early,
            required_threshold: dec!(0.025),
            confidence: 75,
            spot_price: Some(dec!(100500)),
            strike_price: dec!(100000),
            detected_at_ms: 1234567890,
        }
    }

    fn create_toxic_warning(severity: ToxicSeverity) -> ToxicFlowWarning {
        ToxicFlowWarning {
            token_id: "token1".to_string(),
            side: Side::Buy,
            order_size: dec!(5000),
            avg_order_size: dec!(100),
            size_multiplier: dec!(50),
            time_since_last_ms: 100,
            bbo_shift_bps: Some(100),
            severity,
            detected_at_ms: 1234567890,
            indicators: ToxicIndicators {
                large_order: true,
                sudden_appearance: true,
                ..Default::default()
            },
        }
    }

    #[test]
    fn test_config_default() {
        let config = SizingConfig::default();
        assert_eq!(config.base_order_size, dec!(50));
        assert_eq!(config.max_position_per_market, dec!(1000));
        assert_eq!(config.max_total_exposure, dec!(5000));
        assert_eq!(config.min_order_size, dec!(5));
        assert_eq!(config.max_liquidity_take, dec!(0.5));
    }

    #[test]
    fn test_basic_sizing() {
        let sizer = PositionSizer::default();
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));

        let result = sizer.calculate_size(&opportunity, None, Decimal::ZERO, None);

        assert!(result.is_valid);
        assert_eq!(result.size, dec!(50)); // Base order size
        assert_eq!(result.limit_reason, Some(SizingLimit::BaseSize));
        assert_eq!(result.adjustments.combined_multiplier, Decimal::ONE);
    }

    #[test]
    fn test_liquidity_limited() {
        let sizer = PositionSizer::default();
        // Very small liquidity: only 20 shares available
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(20));

        let result = sizer.calculate_size(&opportunity, None, Decimal::ZERO, None);

        assert!(result.is_valid);
        // Limited by liquidity take (50% of 20 = 10)
        assert_eq!(result.size, dec!(10));
        assert_eq!(result.limit_reason, Some(SizingLimit::LiquidityTakeLimit));
    }

    #[test]
    fn test_position_limited() {
        let sizer = PositionSizer::default();
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));

        // Already have $950 exposure in this market
        let mut inventory = Inventory::new("event1".to_string());
        inventory.yes_cost_basis = dec!(475);
        inventory.no_cost_basis = dec!(475);

        let result = sizer.calculate_size(&opportunity, Some(&inventory), Decimal::ZERO, None);

        assert!(result.is_valid);
        // Remaining capacity: $1000 - $950 = $50
        // At combined cost 0.97, max shares = 50 / 0.97 ≈ 51.5
        // Base size of 50 should still fit
        assert!(result.size <= dec!(52));
    }

    #[test]
    fn test_position_limit_exceeded() {
        let sizer = PositionSizer::default();
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));

        // Already at max position
        let mut inventory = Inventory::new("event1".to_string());
        inventory.yes_cost_basis = dec!(500);
        inventory.no_cost_basis = dec!(500);

        let result = sizer.calculate_size(&opportunity, Some(&inventory), Decimal::ZERO, None);

        assert!(!result.is_valid);
        assert_eq!(result.limit_reason, Some(SizingLimit::PositionLimit));
    }

    #[test]
    fn test_exposure_limited() {
        let sizer = PositionSizer::default();
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));

        // Already have $4990 total exposure
        let result = sizer.calculate_size(&opportunity, None, dec!(4990), None);

        assert!(result.is_valid);
        // Remaining: $10 / 0.97 ≈ 10.3 shares
        assert!(result.size < dec!(11));
        assert_eq!(result.limit_reason, Some(SizingLimit::ExposureLimit));
    }

    #[test]
    fn test_exposure_limit_exceeded() {
        let sizer = PositionSizer::default();
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));

        // Already at max exposure
        let result = sizer.calculate_size(&opportunity, None, dec!(5000), None);

        assert!(!result.is_valid);
        assert_eq!(result.limit_reason, Some(SizingLimit::ExposureLimit));
    }

    #[test]
    fn test_inventory_imbalance_adjustment() {
        let sizer = PositionSizer::default();
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));

        // Skewed inventory (imbalance = 0.4, multiplier = 0.75)
        let mut inventory = Inventory::new("event1".to_string());
        inventory.yes_shares = dec!(70);
        inventory.no_shares = dec!(30);
        inventory.yes_cost_basis = dec!(31.5);  // 70 * 0.45
        inventory.no_cost_basis = dec!(15.6);   // 30 * 0.52

        let result = sizer.calculate_size(&opportunity, Some(&inventory), Decimal::ZERO, None);

        assert!(result.is_valid);
        assert_eq!(result.adjustments.inventory_multiplier, dec!(0.75));
        // Base 50 * 0.75 = 37.5
        assert_eq!(result.size, dec!(37.5));
        assert_eq!(result.limit_reason, Some(SizingLimit::ImbalanceAdjustment));
    }

    #[test]
    fn test_toxic_flow_adjustment() {
        let sizer = PositionSizer::default();
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        let warning = create_toxic_warning(ToxicSeverity::Medium);

        let result = sizer.calculate_size(&opportunity, None, Decimal::ZERO, Some(&warning));

        assert!(result.is_valid);
        assert_eq!(result.adjustments.toxic_multiplier, dec!(0.50));
        // Base 50 * 0.50 = 25
        assert_eq!(result.size, dec!(25));
        assert_eq!(result.limit_reason, Some(SizingLimit::ToxicFlowAdjustment));
    }

    #[test]
    fn test_combined_adjustments() {
        let sizer = PositionSizer::default();
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        let warning = create_toxic_warning(ToxicSeverity::Low); // 0.75x

        // Skewed inventory (0.75x)
        let mut inventory = Inventory::new("event1".to_string());
        inventory.yes_shares = dec!(70);
        inventory.no_shares = dec!(30);
        inventory.yes_cost_basis = dec!(31.5);
        inventory.no_cost_basis = dec!(15.6);

        let result = sizer.calculate_size(&opportunity, Some(&inventory), Decimal::ZERO, Some(&warning));

        assert!(result.is_valid);
        assert_eq!(result.adjustments.inventory_multiplier, dec!(0.75));
        assert_eq!(result.adjustments.toxic_multiplier, dec!(0.75));
        // Combined = 0.75 * 0.75 = 0.5625
        assert_eq!(result.adjustments.combined_multiplier, dec!(0.5625));
        // Base 50 * 0.5625 = 28.125
        assert_eq!(result.size, dec!(28.125));
    }

    #[test]
    fn test_critical_toxic_blocks_trade() {
        let sizer = PositionSizer::default();
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        let warning = create_toxic_warning(ToxicSeverity::Critical); // 0x

        let result = sizer.calculate_size(&opportunity, None, Decimal::ZERO, Some(&warning));

        // Critical toxic flow = 0 multiplier -> size = 0 -> below minimum
        assert!(!result.is_valid);
        assert_eq!(result.limit_reason, Some(SizingLimit::BelowMinimum));
    }

    #[test]
    fn test_below_minimum() {
        let config = SizingConfig {
            base_order_size: dec!(10),
            min_order_size: dec!(5),
            ..Default::default()
        };
        let sizer = PositionSizer::new(config);
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        let warning = create_toxic_warning(ToxicSeverity::High); // 0.25x

        let result = sizer.calculate_size(&opportunity, None, Decimal::ZERO, Some(&warning));

        // 10 * 0.25 = 2.5 < 5 minimum
        assert!(!result.is_valid);
        assert_eq!(result.limit_reason, Some(SizingLimit::BelowMinimum));
    }

    #[test]
    fn test_no_liquidity() {
        let sizer = PositionSizer::default();
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(0));

        let result = sizer.calculate_size(&opportunity, None, Decimal::ZERO, None);

        assert!(!result.is_valid);
        assert_eq!(result.limit_reason, Some(SizingLimit::NoOpportunity));
    }

    #[test]
    fn test_can_trade() {
        let sizer = PositionSizer::default();

        // Fresh start
        assert!(sizer.can_trade(Decimal::ZERO, Decimal::ZERO));

        // Below limits
        assert!(sizer.can_trade(dec!(500), dec!(2000)));

        // At position limit
        assert!(!sizer.can_trade(dec!(1000), dec!(2000)));

        // At exposure limit
        assert!(!sizer.can_trade(dec!(500), dec!(5000)));
    }

    #[test]
    fn test_max_possible_size() {
        let sizer = PositionSizer::default();

        // Fresh start at combined_cost = 0.97
        let max = sizer.max_possible_size(Decimal::ZERO, Decimal::ZERO, dec!(0.97));
        // Limited by position: 1000 / 0.97 ≈ 1030
        assert!(max > dec!(1000) && max < dec!(1100));

        // Already have $500 position
        let max = sizer.max_possible_size(dec!(500), dec!(500), dec!(0.97));
        // Remaining position: 500 / 0.97 ≈ 515
        assert!(max > dec!(500) && max < dec!(520));

        // Zero cost (edge case)
        let max = sizer.max_possible_size(Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
        assert_eq!(max, Decimal::ZERO);
    }

    #[test]
    fn test_expected_profit_calculation() {
        let sizer = PositionSizer::default();
        // YES: 0.45, NO: 0.52, combined = 0.97, margin = 0.03
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));

        let result = sizer.calculate_size(&opportunity, None, Decimal::ZERO, None);

        assert!(result.is_valid);
        // Size: 50, margin: 0.03
        // Expected profit: 50 * 0.03 = 1.50
        assert_eq!(result.expected_profit, dec!(1.5));
        // Expected cost: 50 * 0.97 = 48.5
        assert_eq!(result.expected_cost, dec!(48.5));
    }

    #[test]
    fn test_sizing_limit_display() {
        assert_eq!(SizingLimit::BaseSize.to_string(), "base size");
        assert_eq!(SizingLimit::PositionLimit.to_string(), "position limit");
        assert_eq!(SizingLimit::ExposureLimit.to_string(), "exposure limit");
        assert_eq!(SizingLimit::LiquidityLimit.to_string(), "liquidity limit");
        assert_eq!(SizingLimit::BelowMinimum.to_string(), "below minimum");
    }

    #[test]
    fn test_from_trading_config() {
        let trading_config = TradingConfig {
            base_order_size: dec!(100),
            max_position_per_market: dec!(2000),
            max_total_exposure: dec!(10000),
            ..Default::default()
        };

        let sizer = PositionSizer::from_trading_config(&trading_config);

        assert_eq!(sizer.config.base_order_size, dec!(100));
        assert_eq!(sizer.config.max_position_per_market, dec!(2000));
        assert_eq!(sizer.config.max_total_exposure, dec!(10000));
    }

    #[test]
    fn test_crisis_inventory_heavily_reduced() {
        let sizer = PositionSizer::default();
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));

        // Crisis inventory (imbalance = 0.9, multiplier = 0.25)
        let mut inventory = Inventory::new("event1".to_string());
        inventory.yes_shares = dec!(95);
        inventory.no_shares = dec!(5);
        inventory.yes_cost_basis = dec!(42.75);
        inventory.no_cost_basis = dec!(2.60);

        let result = sizer.calculate_size(&opportunity, Some(&inventory), Decimal::ZERO, None);

        assert!(result.is_valid);
        assert_eq!(result.adjustments.inventory_multiplier, dec!(0.25));
        // Base 50 * 0.25 = 12.5
        assert_eq!(result.size, dec!(12.5));
        assert_eq!(result.limit_reason, Some(SizingLimit::ImbalanceAdjustment));
    }

    #[test]
    fn test_exposed_inventory_should_rebalance() {
        let mut inventory = Inventory::new("event1".to_string());
        inventory.yes_shares = dec!(85);
        inventory.no_shares = dec!(15);

        // Exposed state should recommend rebalancing
        assert!(inventory.state().should_rebalance());
    }
}
