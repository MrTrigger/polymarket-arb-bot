//! Position sizing for arbitrage trades.
//!
//! This module provides multiple sizing strategies:
//!
//! - **Limits**: Fixed base sizing with max position/exposure limits
//! - **Confidence**: Dynamic sizing based on market confidence factors
//! - **Hybrid**: Confidence-based sizing with limit caps (recommended)
//!
//! ## Sizing Modes
//!
//! ```ignore
//! // In config/bot.toml:
//! [trading.sizing]
//! mode = "hybrid"  # "limits" | "confidence" | "hybrid"
//! ```
//!
//! ## SizingStrategy Trait
//!
//! All sizers implement the `SizingStrategy` trait, enabling:
//! - Configurable sizing via TOML
//! - Runtime strategy switching
//! - Composition (HybridSizer wraps both strategies)
//!
//! ## MVP Approach (Limits Mode)
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
use crate::strategy::confidence::ConfidenceFactors;
use crate::strategy::confidence_sizing::{ConfidenceSizer, OrderSizeResult};
use crate::strategy::toxic::ToxicFlowWarning;
use crate::types::{Inventory, InventoryState};
use crate::config::SizingConfig as ConfidenceSizingConfig;

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
            min_order_size: Decimal::new(1, 0),             // $1 minimum (Polymarket minimum)
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
            min_order_size: Decimal::new(1, 0),             // $1 minimum (Polymarket minimum)
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

// ============================================================================
// Sizing Mode and Strategy
// ============================================================================

/// Sizing mode determines which strategy is used for order sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SizingMode {
    /// Fixed base sizing with max position/exposure limits (MVP approach).
    #[default]
    Limits,
    /// Dynamic sizing based on market confidence factors.
    Confidence,
    /// Confidence-based sizing with limit caps (recommended for production).
    Hybrid,
}

impl SizingMode {
    /// Parse sizing mode from string.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "limits" => Some(SizingMode::Limits),
            "confidence" => Some(SizingMode::Confidence),
            "hybrid" => Some(SizingMode::Hybrid),
            _ => None,
        }
    }
}

impl std::str::FromStr for SizingMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        SizingMode::parse(s).ok_or_else(|| format!("invalid sizing mode: {}", s))
    }
}

impl std::fmt::Display for SizingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SizingMode::Limits => write!(f, "limits"),
            SizingMode::Confidence => write!(f, "confidence"),
            SizingMode::Hybrid => write!(f, "hybrid"),
        }
    }
}

/// Input parameters for sizing calculation.
///
/// This struct aggregates all inputs needed by any sizing strategy,
/// allowing strategies to use only what they need.
#[derive(Debug, Clone)]
pub struct SizingInput<'a> {
    /// The arbitrage opportunity being sized.
    pub opportunity: &'a ArbOpportunity,
    /// Current inventory for this market.
    pub inventory: Option<&'a Inventory>,
    /// Current total exposure across all markets.
    pub total_exposure: Decimal,
    /// Toxic flow warning if any.
    pub toxic_warning: Option<&'a ToxicFlowWarning>,
    /// Confidence factors for confidence-based sizing.
    pub confidence_factors: Option<ConfidenceFactors>,
}

impl<'a> SizingInput<'a> {
    /// Create a new sizing input with required fields.
    pub fn new(
        opportunity: &'a ArbOpportunity,
        inventory: Option<&'a Inventory>,
        total_exposure: Decimal,
        toxic_warning: Option<&'a ToxicFlowWarning>,
    ) -> Self {
        Self {
            opportunity,
            inventory,
            total_exposure,
            toxic_warning,
            confidence_factors: None,
        }
    }

    /// Add confidence factors for confidence-based sizing.
    pub fn with_confidence(mut self, factors: ConfidenceFactors) -> Self {
        self.confidence_factors = Some(factors);
        self
    }
}

/// Unified result type for all sizing strategies.
///
/// Converts between `SizingResult` (limits) and `OrderSizeResult` (confidence).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedSizingResult {
    /// Calculated position size (in shares).
    pub size: Decimal,
    /// Whether this is a valid trade.
    pub is_valid: bool,
    /// Expected cost for this size.
    pub expected_cost: Decimal,
    /// Expected profit for this size.
    pub expected_profit: Decimal,
    /// Reason if size was limited or rejected.
    pub limit_reason: Option<String>,
    /// Confidence multiplier if confidence-based (1.0 for limits-only).
    pub confidence_multiplier: Decimal,
    /// Source strategy that produced this result.
    pub strategy: SizingMode,
}

impl UnifiedSizingResult {
    /// Create an invalid result.
    pub fn invalid(reason: &str, strategy: SizingMode) -> Self {
        Self {
            size: Decimal::ZERO,
            is_valid: false,
            expected_cost: Decimal::ZERO,
            expected_profit: Decimal::ZERO,
            limit_reason: Some(reason.to_string()),
            confidence_multiplier: Decimal::ONE,
            strategy,
        }
    }

    /// Convert from a limits-based SizingResult.
    pub fn from_limits_result(result: &SizingResult, _opportunity: &ArbOpportunity) -> Self {
        Self {
            size: result.size,
            is_valid: result.is_valid,
            expected_cost: result.expected_cost,
            expected_profit: result.expected_profit,
            limit_reason: result.limit_reason.map(|l| l.to_string()),
            confidence_multiplier: result.adjustments.combined_multiplier,
            strategy: SizingMode::Limits,
        }
    }

    /// Convert from a confidence-based OrderSizeResult.
    pub fn from_confidence_result(result: &OrderSizeResult, opportunity: &ArbOpportunity) -> Self {
        // Calculate expected cost/profit based on opportunity
        let cost_per_share = opportunity.combined_cost;
        let expected_cost = result.size * cost_per_share;
        let expected_profit = result.size * opportunity.margin;

        Self {
            size: result.size,
            is_valid: result.is_valid,
            expected_cost,
            expected_profit,
            limit_reason: result.rejection_reason.map(|r| r.to_string()),
            confidence_multiplier: result.confidence_multiplier,
            strategy: SizingMode::Confidence,
        }
    }
}

/// Trait for sizing strategies.
///
/// All sizing implementations (Limits, Confidence, Hybrid) implement this trait
/// to enable configurable and composable sizing.
pub trait SizingStrategy: Send + Sync {
    /// Calculate position size for the given inputs.
    fn calculate(&self, input: &SizingInput<'_>) -> UnifiedSizingResult;

    /// Check if trading is possible given current limits.
    fn can_trade(&self, existing_position: Decimal, total_exposure: Decimal) -> bool;

    /// Get the sizing mode this strategy implements.
    fn mode(&self) -> SizingMode;

    /// Record a completed trade (for budget tracking in confidence mode).
    fn record_trade(&mut self, cost: Decimal);

    /// Reset for a new market session.
    fn reset(&mut self);
}

// Implement SizingStrategy for PositionSizer (Limits mode)
impl SizingStrategy for PositionSizer {
    fn calculate(&self, input: &SizingInput<'_>) -> UnifiedSizingResult {
        let result = self.calculate_size(
            input.opportunity,
            input.inventory,
            input.total_exposure,
            input.toxic_warning,
        );
        UnifiedSizingResult::from_limits_result(&result, input.opportunity)
    }

    fn can_trade(&self, existing_position: Decimal, total_exposure: Decimal) -> bool {
        PositionSizer::can_trade(self, existing_position, total_exposure)
    }

    fn mode(&self) -> SizingMode {
        SizingMode::Limits
    }

    fn record_trade(&mut self, _cost: Decimal) {
        // PositionSizer doesn't track budget
    }

    fn reset(&mut self) {
        // PositionSizer doesn't have session state
    }
}

// Implement SizingStrategy for ConfidenceSizer
impl SizingStrategy for ConfidenceSizer {
    fn calculate(&self, input: &SizingInput<'_>) -> UnifiedSizingResult {
        // Confidence mode requires ConfidenceFactors
        let factors = match &input.confidence_factors {
            Some(f) => f,
            None => {
                return UnifiedSizingResult::invalid(
                    "confidence factors required for confidence mode",
                    SizingMode::Confidence,
                );
            }
        };

        let result = self.get_order_size(factors);
        UnifiedSizingResult::from_confidence_result(&result, input.opportunity)
    }

    fn can_trade(&self, _existing_position: Decimal, _total_exposure: Decimal) -> bool {
        // ConfidenceSizer uses budget, not position limits
        !self.is_budget_exhausted()
    }

    fn mode(&self) -> SizingMode {
        SizingMode::Confidence
    }

    fn record_trade(&mut self, cost: Decimal) {
        ConfidenceSizer::record_trade(self, cost);
    }

    fn reset(&mut self) {
        ConfidenceSizer::reset(self);
    }
}

/// Hybrid sizer that combines confidence-based sizing with limit caps.
///
/// This is the recommended mode for production:
/// 1. Calculate size using confidence factors
/// 2. Apply position/exposure limits from PositionSizer
/// 3. Apply toxic flow and inventory adjustments
///
/// The result is confidence-scaled sizes that respect hard limits.
#[derive(Debug, Clone)]
pub struct HybridSizer {
    /// Confidence-based sizer for dynamic sizing.
    confidence_sizer: ConfidenceSizer,
    /// Limits-based sizer for cap enforcement.
    limits_sizer: PositionSizer,
}

impl HybridSizer {
    /// Create a new hybrid sizer.
    pub fn new(confidence_config: ConfidenceSizingConfig, limits_config: SizingConfig) -> Self {
        Self {
            confidence_sizer: ConfidenceSizer::new(confidence_config),
            limits_sizer: PositionSizer::new(limits_config),
        }
    }

    /// Create from trading config with default limits.
    pub fn from_trading_config(trading_config: &TradingConfig) -> Self {
        Self {
            confidence_sizer: ConfidenceSizer::new(trading_config.sizing.clone()),
            limits_sizer: PositionSizer::from_trading_config(trading_config),
        }
    }

    /// Get the confidence sizer.
    pub fn confidence_sizer(&self) -> &ConfidenceSizer {
        &self.confidence_sizer
    }

    /// Get the limits sizer.
    pub fn limits_sizer(&self) -> &PositionSizer {
        &self.limits_sizer
    }
}

impl SizingStrategy for HybridSizer {
    fn calculate(&self, input: &SizingInput<'_>) -> UnifiedSizingResult {
        // Step 1: Check if limits allow trading at all
        let existing_position = input.inventory
            .map(|inv| inv.total_exposure())
            .unwrap_or(Decimal::ZERO);

        if !self.limits_sizer.can_trade(existing_position, input.total_exposure) {
            return UnifiedSizingResult::invalid("position or exposure limit reached", SizingMode::Hybrid);
        }

        // Step 2: Get confidence-based size (or use defaults if no factors)
        let confidence_result = if let Some(factors) = &input.confidence_factors {
            self.confidence_sizer.get_order_size(factors)
        } else {
            // Without confidence factors, use base size with neutral confidence
            let factors = ConfidenceFactors::neutral();
            self.confidence_sizer.get_order_size(&factors)
        };

        // If confidence sizing rejects, return that rejection
        if !confidence_result.is_valid {
            return UnifiedSizingResult::from_confidence_result(&confidence_result, input.opportunity);
        }

        // Step 3: Apply limits to the confidence-sized order
        let limits_result = self.limits_sizer.calculate_size(
            input.opportunity,
            input.inventory,
            input.total_exposure,
            input.toxic_warning,
        );

        // Step 4: Take the minimum of confidence and limits sizes
        let final_size = confidence_result.size.min(limits_result.size);

        // Check minimum
        if final_size < self.limits_sizer.config().min_order_size {
            return UnifiedSizingResult::invalid("below minimum order size", SizingMode::Hybrid);
        }

        // Determine which was the limiting factor
        let (limit_reason, final_multiplier) = if confidence_result.size <= limits_result.size {
            (
                confidence_result.rejection_reason.map(|r| r.to_string())
                    .or_else(|| Some("confidence sized".to_string())),
                confidence_result.confidence_multiplier,
            )
        } else {
            (
                limits_result.limit_reason.map(|l| l.to_string()),
                limits_result.adjustments.combined_multiplier * confidence_result.confidence_multiplier,
            )
        };

        let cost_per_share = input.opportunity.combined_cost;
        UnifiedSizingResult {
            size: final_size,
            is_valid: true,
            expected_cost: final_size * cost_per_share,
            expected_profit: final_size * input.opportunity.margin,
            limit_reason,
            confidence_multiplier: final_multiplier,
            strategy: SizingMode::Hybrid,
        }
    }

    fn can_trade(&self, existing_position: Decimal, total_exposure: Decimal) -> bool {
        // Both conditions must be met
        self.limits_sizer.can_trade(existing_position, total_exposure)
            && !self.confidence_sizer.is_budget_exhausted()
    }

    fn mode(&self) -> SizingMode {
        SizingMode::Hybrid
    }

    fn record_trade(&mut self, cost: Decimal) {
        self.confidence_sizer.record_trade(cost);
    }

    fn reset(&mut self) {
        self.confidence_sizer.reset();
    }
}

/// Factory function to create a sizer based on mode.
///
/// # Arguments
///
/// * `mode` - The sizing mode to use
/// * `trading_config` - Trading configuration with sizing parameters
///
/// # Returns
///
/// A boxed `SizingStrategy` implementation for the given mode.
pub fn create_sizer(mode: SizingMode, trading_config: &TradingConfig) -> Box<dyn SizingStrategy> {
    match mode {
        SizingMode::Limits => Box::new(PositionSizer::from_trading_config(trading_config)),
        SizingMode::Confidence => Box::new(ConfidenceSizer::new(trading_config.sizing.clone())),
        SizingMode::Hybrid => Box::new(HybridSizer::from_trading_config(trading_config)),
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
        assert_eq!(config.min_order_size, dec!(1));
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
            base_order_size: dec!(2),
            min_order_size: dec!(1),
            ..Default::default()
        };
        let sizer = PositionSizer::new(config);
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        let warning = create_toxic_warning(ToxicSeverity::High); // 0.25x

        let result = sizer.calculate_size(&opportunity, None, Decimal::ZERO, Some(&warning));

        // 2 * 0.25 = 0.5 < 1 minimum
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

    // =========================================================================
    // SizingMode Tests
    // =========================================================================

    #[test]
    fn test_sizing_mode_default() {
        let mode: SizingMode = Default::default();
        assert_eq!(mode, SizingMode::Limits);
    }

    #[test]
    fn test_sizing_mode_parse() {
        assert_eq!(SizingMode::parse("limits"), Some(SizingMode::Limits));
        assert_eq!(SizingMode::parse("LIMITS"), Some(SizingMode::Limits));
        assert_eq!(SizingMode::parse("confidence"), Some(SizingMode::Confidence));
        assert_eq!(SizingMode::parse("CONFIDENCE"), Some(SizingMode::Confidence));
        assert_eq!(SizingMode::parse("hybrid"), Some(SizingMode::Hybrid));
        assert_eq!(SizingMode::parse("HYBRID"), Some(SizingMode::Hybrid));
        assert_eq!(SizingMode::parse("invalid"), None);
    }

    #[test]
    fn test_sizing_mode_from_str_trait() {
        use std::str::FromStr;
        assert_eq!(SizingMode::from_str("limits").unwrap(), SizingMode::Limits);
        assert_eq!(SizingMode::from_str("hybrid").unwrap(), SizingMode::Hybrid);
        assert!(SizingMode::from_str("invalid").is_err());
    }

    #[test]
    fn test_sizing_mode_display() {
        assert_eq!(SizingMode::Limits.to_string(), "limits");
        assert_eq!(SizingMode::Confidence.to_string(), "confidence");
        assert_eq!(SizingMode::Hybrid.to_string(), "hybrid");
    }

    #[test]
    fn test_sizing_mode_serialization() {
        let mode = SizingMode::Hybrid;
        let json = serde_json::to_string(&mode).unwrap();
        assert!(json.contains("hybrid"));

        let parsed: SizingMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SizingMode::Hybrid);
    }

    // =========================================================================
    // SizingInput Tests
    // =========================================================================

    #[test]
    fn test_sizing_input_new() {
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        let input = SizingInput::new(&opportunity, None, Decimal::ZERO, None);

        assert_eq!(input.total_exposure, Decimal::ZERO);
        assert!(input.inventory.is_none());
        assert!(input.toxic_warning.is_none());
        assert!(input.confidence_factors.is_none());
    }

    #[test]
    fn test_sizing_input_with_confidence() {
        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        let factors = ConfidenceFactors::neutral();

        let input = SizingInput::new(&opportunity, None, Decimal::ZERO, None)
            .with_confidence(factors);

        assert!(input.confidence_factors.is_some());
    }

    // =========================================================================
    // UnifiedSizingResult Tests
    // =========================================================================

    #[test]
    fn test_unified_result_invalid() {
        let result = UnifiedSizingResult::invalid("test reason", SizingMode::Limits);

        assert!(!result.is_valid);
        assert_eq!(result.size, Decimal::ZERO);
        assert_eq!(result.limit_reason, Some("test reason".to_string()));
        assert_eq!(result.strategy, SizingMode::Limits);
    }

    #[test]
    fn test_unified_result_serialization() {
        let result = UnifiedSizingResult {
            size: dec!(50),
            is_valid: true,
            expected_cost: dec!(48.5),
            expected_profit: dec!(1.5),
            limit_reason: Some("base size".to_string()),
            confidence_multiplier: dec!(1.5),
            strategy: SizingMode::Hybrid,
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("hybrid"));
        assert!(json.contains("confidence_multiplier"));

        let parsed: UnifiedSizingResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.size, dec!(50));
        assert_eq!(parsed.strategy, SizingMode::Hybrid);
    }

    // =========================================================================
    // SizingStrategy Trait Implementation Tests
    // =========================================================================

    #[test]
    fn test_limits_sizer_implements_strategy() {
        let sizer = PositionSizer::default();

        assert_eq!(sizer.mode(), SizingMode::Limits);
        assert!(sizer.can_trade(Decimal::ZERO, Decimal::ZERO));

        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        let input = SizingInput::new(&opportunity, None, Decimal::ZERO, None);
        let result = sizer.calculate(&input);

        assert!(result.is_valid);
        assert_eq!(result.strategy, SizingMode::Limits);
    }

    #[test]
    fn test_confidence_sizer_implements_strategy() {
        use crate::config::SizingConfig as ConfSizingConfig;
        let config = ConfSizingConfig::new(dec!(5000));
        let sizer = ConfidenceSizer::new(config);

        assert_eq!(sizer.mode(), SizingMode::Confidence);
        assert!(sizer.can_trade(Decimal::ZERO, Decimal::ZERO));

        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        let factors = ConfidenceFactors::neutral();
        let input = SizingInput::new(&opportunity, None, Decimal::ZERO, None)
            .with_confidence(factors);
        let result = sizer.calculate(&input);

        assert!(result.is_valid);
        assert_eq!(result.strategy, SizingMode::Confidence);
    }

    #[test]
    fn test_confidence_sizer_requires_factors() {
        use crate::config::SizingConfig as ConfSizingConfig;
        let config = ConfSizingConfig::new(dec!(5000));
        let sizer = ConfidenceSizer::new(config);

        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        // No confidence factors provided
        let input = SizingInput::new(&opportunity, None, Decimal::ZERO, None);
        let result = sizer.calculate(&input);

        // Should reject without factors
        assert!(!result.is_valid);
        assert!(result.limit_reason.unwrap().contains("confidence factors required"));
    }

    // =========================================================================
    // HybridSizer Tests
    // =========================================================================

    #[test]
    fn test_hybrid_sizer_creation() {
        let trading_config = TradingConfig::default();
        let sizer = HybridSizer::from_trading_config(&trading_config);

        assert_eq!(sizer.mode(), SizingMode::Hybrid);
    }

    #[test]
    fn test_hybrid_sizer_with_confidence() {
        let trading_config = TradingConfig::default();
        let sizer = HybridSizer::from_trading_config(&trading_config);

        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        let factors = ConfidenceFactors::neutral();
        let input = SizingInput::new(&opportunity, None, Decimal::ZERO, None)
            .with_confidence(factors);

        let result = sizer.calculate(&input);

        assert!(result.is_valid);
        assert_eq!(result.strategy, SizingMode::Hybrid);
        // Should have non-trivial confidence multiplier
        assert!(result.confidence_multiplier > Decimal::ZERO);
    }

    #[test]
    fn test_hybrid_sizer_without_confidence_uses_neutral() {
        let trading_config = TradingConfig::default();
        let sizer = HybridSizer::from_trading_config(&trading_config);

        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        // No confidence factors - should use neutral defaults
        let input = SizingInput::new(&opportunity, None, Decimal::ZERO, None);

        let result = sizer.calculate(&input);

        assert!(result.is_valid);
        assert_eq!(result.strategy, SizingMode::Hybrid);
    }

    #[test]
    fn test_hybrid_sizer_respects_limits() {
        let trading_config = TradingConfig::default();
        let sizer = HybridSizer::from_trading_config(&trading_config);

        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        // Provide high confidence factors that would suggest a large size
        let factors = ConfidenceFactors::new(
            dec!(100), // Far from strike
            dec!(1),   // Final minute
            crate::strategy::signal::Signal::StrongUp,
            dec!(0.5), // High imbalance
            dec!(60000), // Deep book
        );
        let input = SizingInput::new(&opportunity, None, Decimal::ZERO, None)
            .with_confidence(factors);

        let result = sizer.calculate(&input);

        assert!(result.is_valid);
        // Size should be capped by limits (base_order_size = 50)
        // Even with high confidence, limits apply
        assert!(result.size <= dec!(150)); // 50 * 3.0 max multiplier
    }

    #[test]
    fn test_hybrid_sizer_can_trade() {
        let trading_config = TradingConfig::default();
        let sizer = HybridSizer::from_trading_config(&trading_config);

        // Fresh start
        assert!(sizer.can_trade(Decimal::ZERO, Decimal::ZERO));

        // At limits
        assert!(!sizer.can_trade(dec!(1000), Decimal::ZERO)); // Position limit
        assert!(!sizer.can_trade(Decimal::ZERO, dec!(5000))); // Exposure limit
    }

    // =========================================================================
    // Factory Function Tests
    // =========================================================================

    #[test]
    fn test_create_sizer_limits() {
        let trading_config = TradingConfig::default();
        let sizer = create_sizer(SizingMode::Limits, &trading_config);

        assert_eq!(sizer.mode(), SizingMode::Limits);
    }

    #[test]
    fn test_create_sizer_confidence() {
        let trading_config = TradingConfig::default();
        let sizer = create_sizer(SizingMode::Confidence, &trading_config);

        assert_eq!(sizer.mode(), SizingMode::Confidence);
    }

    #[test]
    fn test_create_sizer_hybrid() {
        let trading_config = TradingConfig::default();
        let sizer = create_sizer(SizingMode::Hybrid, &trading_config);

        assert_eq!(sizer.mode(), SizingMode::Hybrid);
    }

    #[test]
    fn test_factory_sizer_usable() {
        let trading_config = TradingConfig::default();
        let sizer = create_sizer(SizingMode::Hybrid, &trading_config);

        let opportunity = create_test_opportunity(dec!(0.45), dec!(0.52), dec!(1000));
        let input = SizingInput::new(&opportunity, None, Decimal::ZERO, None);

        let result = sizer.calculate(&input);
        assert!(result.is_valid);
    }
}
