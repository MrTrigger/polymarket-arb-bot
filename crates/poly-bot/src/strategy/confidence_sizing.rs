//! Confidence-based position sizing.
//!
//! This module implements the `ConfidenceSizer` which dynamically adjusts
//! order sizes based on market confidence factors:
//!
//! - **Time remaining**: More confidence as window closes (if far from strike)
//! - **Distance to strike**: More confidence when price is far from strike
//! - **Signal strength**: Strong directional signals = larger sizes
//! - **Order book quality**: Good depth and favorable imbalance = larger sizes
//!
//! ## Sizing Formula
//!
//! ```text
//! order_size = base_order_size × confidence_multiplier
//! ```
//!
//! Where:
//! - `base_order_size = market_budget / expected_trades_per_market`
//! - `confidence_multiplier = (time × distance × signal × book)^0.25 × 1.5`
//! - Multiplier is capped between 0.5x and 3.0x
//!
//! ## Budget Management
//!
//! The sizer tracks:
//! - `spent`: Total USDC spent in the current market session
//! - `trades`: Number of trades executed
//!
//! When `spent >= market_budget`, returns `BudgetExhausted`.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::config::SizingConfig;
use crate::strategy::confidence::{Confidence, ConfidenceCalculator, ConfidenceFactors};
use crate::strategy::signal::Signal;

/// Result of a confidence-based sizing calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderSizeResult {
    /// The calculated order size in USDC.
    pub size: Decimal,

    /// Whether this is a valid order (size >= min_order_size, budget available).
    pub is_valid: bool,

    /// The base order size before confidence adjustment.
    pub base_size: Decimal,

    /// The confidence multiplier applied.
    pub confidence_multiplier: Decimal,

    /// Breakdown of confidence factors.
    pub confidence: Confidence,

    /// Reason if the order was rejected.
    pub rejection_reason: Option<SizeRejection>,

    /// Budget remaining after this order (if executed).
    pub budget_remaining: Decimal,

    /// Trades remaining (estimated based on base size).
    pub trades_remaining: u32,
}

impl OrderSizeResult {
    /// Create an invalid result with a rejection reason.
    pub fn rejected(reason: SizeRejection, budget_remaining: Decimal) -> Self {
        Self {
            size: Decimal::ZERO,
            is_valid: false,
            base_size: Decimal::ZERO,
            confidence_multiplier: Decimal::ONE,
            confidence: Confidence::neutral(),
            rejection_reason: Some(reason),
            budget_remaining,
            trades_remaining: 0,
        }
    }
}

/// Reason why an order size was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeRejection {
    /// Market budget is exhausted.
    BudgetExhausted,
    /// Calculated size is below minimum order size.
    BelowMinimum,
    /// No available balance.
    NoBalance,
}

impl std::fmt::Display for SizeRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SizeRejection::BudgetExhausted => write!(f, "budget exhausted"),
            SizeRejection::BelowMinimum => write!(f, "below minimum order size"),
            SizeRejection::NoBalance => write!(f, "no available balance"),
        }
    }
}

/// Confidence-based position sizer.
///
/// Tracks budget usage and adjusts order sizes based on market confidence.
///
/// # Example
///
/// ```ignore
/// use rust_decimal_macros::dec;
/// use poly_bot::config::SizingConfig;
/// use poly_bot::strategy::confidence_sizing::ConfidenceSizer;
/// use poly_bot::strategy::confidence::ConfidenceFactors;
/// use poly_bot::strategy::signal::Signal;
///
/// let config = SizingConfig::new(dec!(5000)); // $5,000 balance
/// let mut sizer = ConfidenceSizer::new(config);
///
/// let factors = ConfidenceFactors::new(
///     dec!(80),         // $80 from strike
///     dec!(2),          // 2 minutes remaining
///     Signal::StrongUp,
///     dec!(0.3),        // 30% imbalance
///     dec!(25000),      // $25k depth
/// );
///
/// let result = sizer.get_order_size(&factors);
/// assert!(result.is_valid);
/// assert!(result.confidence_multiplier > dec!(1.0));
/// ```
#[derive(Debug, Clone)]
pub struct ConfidenceSizer {
    /// Configuration for sizing calculations.
    config: SizingConfig,

    /// Total USDC spent in the current market session.
    spent: Decimal,

    /// Number of trades executed in this session.
    trades: u32,
}

impl ConfidenceSizer {
    /// Create a new confidence sizer with the given configuration.
    pub fn new(config: SizingConfig) -> Self {
        Self {
            config,
            spent: Decimal::ZERO,
            trades: 0,
        }
    }

    /// Create a sizer with default configuration for the given balance.
    pub fn with_balance(available_balance: Decimal) -> Self {
        Self::new(SizingConfig::new(available_balance))
    }

    /// Calculate order size based on confidence factors.
    ///
    /// # Arguments
    ///
    /// * `factors` - Market conditions used to calculate confidence
    ///
    /// # Returns
    ///
    /// `OrderSizeResult` containing the calculated size and metadata.
    pub fn get_order_size(&self, factors: &ConfidenceFactors) -> OrderSizeResult {
        let budget = self.config.market_budget();
        let remaining_budget = budget - self.spent;

        // Check if budget is exhausted
        if remaining_budget <= Decimal::ZERO {
            return OrderSizeResult::rejected(SizeRejection::BudgetExhausted, Decimal::ZERO);
        }

        // Calculate base order size
        let base_size = self.config.base_order_size();

        // Calculate confidence from factors
        let confidence = ConfidenceCalculator::calculate(factors);
        let raw_multiplier = confidence.total_multiplier();

        // Cap multiplier at config maximum
        let confidence_multiplier = raw_multiplier.min(self.config.max_confidence_multiplier);

        // Apply confidence to base size
        let mut size = base_size * confidence_multiplier;

        // Cap at remaining budget
        if size > remaining_budget {
            size = remaining_budget;
        }

        // Check minimum order size
        if size < self.config.min_order_size {
            return OrderSizeResult::rejected(SizeRejection::BelowMinimum, remaining_budget);
        }

        // Calculate remaining budget and trades after this order
        let budget_after = remaining_budget - size;
        let trades_remaining = if base_size > Decimal::ZERO {
            let remaining = budget_after / base_size;
            remaining.trunc().to_string().parse().unwrap_or(0)
        } else {
            0
        };

        OrderSizeResult {
            size,
            is_valid: true,
            base_size,
            confidence_multiplier,
            confidence,
            rejection_reason: None,
            budget_remaining: budget_after,
            trades_remaining,
        }
    }

    /// Record a trade execution.
    ///
    /// Call this after an order is filled to update budget tracking.
    ///
    /// # Arguments
    ///
    /// * `cost` - The actual cost of the filled order in USDC
    pub fn record_trade(&mut self, cost: Decimal) {
        self.spent += cost;
        self.trades += 1;
    }

    /// Get the current market budget.
    pub fn market_budget(&self) -> Decimal {
        self.config.market_budget()
    }

    /// Get the amount spent so far.
    pub fn spent(&self) -> Decimal {
        self.spent
    }

    /// Get the remaining budget.
    pub fn remaining_budget(&self) -> Decimal {
        (self.config.market_budget() - self.spent).max(Decimal::ZERO)
    }

    /// Get the number of trades executed.
    pub fn trades(&self) -> u32 {
        self.trades
    }

    /// Check if the budget is exhausted.
    pub fn is_budget_exhausted(&self) -> bool {
        self.spent >= self.config.market_budget()
    }

    /// Reset the sizer for a new market session.
    pub fn reset(&mut self) {
        self.spent = Decimal::ZERO;
        self.trades = 0;
    }

    /// Get the underlying configuration.
    pub fn config(&self) -> &SizingConfig {
        &self.config
    }

    /// Update the available balance (e.g., after a deposit or withdrawal).
    pub fn set_available_balance(&mut self, balance: Decimal) {
        self.config.available_balance = balance;
    }

    // =========================================================================
    // Confidence calculation helpers (delegated to ConfidenceCalculator)
    // =========================================================================

    /// Calculate time confidence based on minutes remaining and distance.
    ///
    /// Higher confidence when less time AND far from strike.
    #[inline]
    pub fn time_confidence(minutes_remaining: Decimal, distance: Decimal) -> Decimal {
        ConfidenceCalculator::time_confidence(minutes_remaining, distance)
    }

    /// Calculate distance confidence based on distance to strike.
    ///
    /// Higher confidence when farther from strike price.
    #[inline]
    pub fn distance_confidence(distance: Decimal) -> Decimal {
        ConfidenceCalculator::distance_confidence(distance)
    }

    /// Calculate signal confidence based on signal strength.
    ///
    /// Strong signals get larger sizes.
    #[inline]
    pub fn signal_confidence(signal: Signal) -> Decimal {
        ConfidenceCalculator::signal_confidence(signal)
    }

    /// Calculate book confidence based on imbalance and depth.
    ///
    /// Favorable depth and imbalance = more confidence.
    #[inline]
    pub fn book_confidence(imbalance: Decimal, favorable_depth: Decimal) -> Decimal {
        ConfidenceCalculator::book_confidence(imbalance, favorable_depth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::confidence::{MAX_MULTIPLIER, MIN_MULTIPLIER};
    use rust_decimal_macros::dec;

    // =========================================================================
    // ConfidenceSizer Creation Tests
    // =========================================================================

    #[test]
    fn test_sizer_new() {
        let config = SizingConfig::new(dec!(5000));
        let sizer = ConfidenceSizer::new(config);

        assert_eq!(sizer.spent(), Decimal::ZERO);
        assert_eq!(sizer.trades(), 0);
        assert_eq!(sizer.market_budget(), dec!(1000)); // 5000 * 0.20
        assert_eq!(sizer.remaining_budget(), dec!(1000));
    }

    #[test]
    fn test_sizer_with_balance() {
        let sizer = ConfidenceSizer::with_balance(dec!(10000));

        assert_eq!(sizer.market_budget(), dec!(2000)); // 10000 * 0.20
        assert_eq!(sizer.config().available_balance, dec!(10000));
    }

    // =========================================================================
    // Order Size Calculation Tests
    // =========================================================================

    #[test]
    fn test_get_order_size_neutral_confidence() {
        let sizer = ConfidenceSizer::with_balance(dec!(5000));

        // Neutral confidence factors
        let factors = ConfidenceFactors::new(
            dec!(25), // Medium distance
            dec!(7),  // Mid-market time
            Signal::Neutral,
            dec!(0.0),   // No imbalance
            dec!(15000), // Medium depth
        );

        let result = sizer.get_order_size(&factors);

        assert!(result.is_valid);
        assert!(result.rejection_reason.is_none());
        // Base size is $5 (1000 budget / 200 trades)
        assert_eq!(result.base_size, dec!(5));
        // With neutral confidence, multiplier should be moderate
        assert!(result.confidence_multiplier >= MIN_MULTIPLIER);
        assert!(result.confidence_multiplier <= MAX_MULTIPLIER);
    }

    #[test]
    fn test_get_order_size_high_confidence() {
        let sizer = ConfidenceSizer::with_balance(dec!(5000));

        // High confidence factors
        let factors = ConfidenceFactors::new(
            dec!(100), // Far from strike
            dec!(1),   // Final minutes
            Signal::StrongUp,
            dec!(0.5),   // High imbalance
            dec!(60000), // Very deep
        );

        let result = sizer.get_order_size(&factors);

        assert!(result.is_valid);
        // With high confidence, multiplier should be above 1.5
        assert!(result.confidence_multiplier > dec!(1.5));
        // Size should be larger than base
        assert!(result.size > result.base_size);
    }

    #[test]
    fn test_get_order_size_low_confidence() {
        let sizer = ConfidenceSizer::with_balance(dec!(5000));

        // Low confidence factors
        let factors = ConfidenceFactors::new(
            dec!(5),  // Close to strike
            dec!(14), // Early market
            Signal::Neutral,
            dec!(0.0),  // No imbalance
            dec!(3000), // Thin book
        );

        let result = sizer.get_order_size(&factors);

        assert!(result.is_valid);
        // With low confidence, multiplier should be below 1.5
        assert!(result.confidence_multiplier < dec!(1.5));
        // Size might be smaller than base or at minimum
        assert!(result.size >= sizer.config().min_order_size);
    }

    // =========================================================================
    // Budget Management Tests
    // =========================================================================

    #[test]
    fn test_record_trade() {
        let mut sizer = ConfidenceSizer::with_balance(dec!(5000));

        assert_eq!(sizer.spent(), Decimal::ZERO);
        assert_eq!(sizer.trades(), 0);

        sizer.record_trade(dec!(10));
        assert_eq!(sizer.spent(), dec!(10));
        assert_eq!(sizer.trades(), 1);

        sizer.record_trade(dec!(15));
        assert_eq!(sizer.spent(), dec!(25));
        assert_eq!(sizer.trades(), 2);
    }

    #[test]
    fn test_remaining_budget() {
        let mut sizer = ConfidenceSizer::with_balance(dec!(5000));
        // Market budget = 5000 * 0.20 = 1000

        assert_eq!(sizer.remaining_budget(), dec!(1000));

        sizer.record_trade(dec!(300));
        assert_eq!(sizer.remaining_budget(), dec!(700));

        sizer.record_trade(dec!(700));
        assert_eq!(sizer.remaining_budget(), Decimal::ZERO);

        // Should not go negative
        sizer.record_trade(dec!(100));
        assert_eq!(sizer.remaining_budget(), Decimal::ZERO);
    }

    #[test]
    fn test_budget_exhausted() {
        let mut sizer = ConfidenceSizer::with_balance(dec!(5000));
        // Market budget = 1000

        assert!(!sizer.is_budget_exhausted());

        sizer.record_trade(dec!(500));
        assert!(!sizer.is_budget_exhausted());

        sizer.record_trade(dec!(500));
        assert!(sizer.is_budget_exhausted());
    }

    #[test]
    fn test_budget_exhausted_rejection() {
        let mut sizer = ConfidenceSizer::with_balance(dec!(5000));

        // Exhaust the budget
        sizer.record_trade(dec!(1000));

        let factors = ConfidenceFactors::new(
            dec!(50),
            dec!(5),
            Signal::StrongUp,
            dec!(0.3),
            dec!(25000),
        );

        let result = sizer.get_order_size(&factors);

        assert!(!result.is_valid);
        assert_eq!(result.rejection_reason, Some(SizeRejection::BudgetExhausted));
        assert_eq!(result.size, Decimal::ZERO);
    }

    #[test]
    fn test_reset() {
        let mut sizer = ConfidenceSizer::with_balance(dec!(5000));

        sizer.record_trade(dec!(500));
        sizer.record_trade(dec!(300));
        assert_eq!(sizer.spent(), dec!(800));
        assert_eq!(sizer.trades(), 2);

        sizer.reset();

        assert_eq!(sizer.spent(), Decimal::ZERO);
        assert_eq!(sizer.trades(), 0);
        assert_eq!(sizer.remaining_budget(), dec!(1000));
    }

    // =========================================================================
    // Size Capping Tests
    // =========================================================================

    #[test]
    fn test_size_capped_at_remaining_budget() {
        let mut sizer = ConfidenceSizer::with_balance(dec!(5000));

        // Spend most of the budget
        sizer.record_trade(dec!(990));

        // Only $10 remaining
        let factors = ConfidenceFactors::new(
            dec!(100),
            dec!(1),
            Signal::StrongUp,
            dec!(0.5),
            dec!(60000),
        );

        let result = sizer.get_order_size(&factors);

        // Should cap at remaining budget
        assert!(result.is_valid);
        assert!(result.size <= dec!(10));
    }

    #[test]
    fn test_below_minimum_rejection() {
        let mut sizer = ConfidenceSizer::with_balance(dec!(5000));

        // Spend almost all budget, leaving less than min_order_size ($1)
        sizer.record_trade(dec!(999.5));

        let factors = ConfidenceFactors::new(
            dec!(50),
            dec!(5),
            Signal::Neutral,
            dec!(0.0),
            dec!(15000),
        );

        let result = sizer.get_order_size(&factors);

        assert!(!result.is_valid);
        assert_eq!(result.rejection_reason, Some(SizeRejection::BelowMinimum));
    }

    #[test]
    fn test_multiplier_capped_at_config_max() {
        // Create sizer with low max multiplier
        let mut config = SizingConfig::new(dec!(5000));
        config.max_confidence_multiplier = dec!(2.0);
        let sizer = ConfidenceSizer::new(config);

        // High confidence factors that would normally give >2.0x
        let factors = ConfidenceFactors::new(
            dec!(120),
            dec!(0.5),
            Signal::StrongDown,
            dec!(0.6),
            dec!(80000),
        );

        let result = sizer.get_order_size(&factors);

        assert!(result.is_valid);
        assert!(result.confidence_multiplier <= dec!(2.0));
    }

    // =========================================================================
    // Balance Scaling Tests (Spec Examples)
    // =========================================================================

    #[test]
    fn test_scaling_500_balance() {
        // $500 account (very small)
        let sizer = ConfidenceSizer::with_balance(dec!(500));

        assert_eq!(sizer.market_budget(), dec!(100)); // 500 * 0.20
        assert_eq!(sizer.config().base_order_size(), dec!(1)); // 100/200 = 0.5, floored to $1

        let factors = ConfidenceFactors::new(
            dec!(25),
            dec!(7),
            Signal::Neutral,
            dec!(0.0),
            dec!(15000),
        );

        let result = sizer.get_order_size(&factors);
        assert!(result.is_valid);
        assert_eq!(result.base_size, dec!(1));
    }

    #[test]
    fn test_scaling_1000_balance() {
        // $1,000 account
        let sizer = ConfidenceSizer::with_balance(dec!(1000));

        assert_eq!(sizer.market_budget(), dec!(200)); // 1000 * 0.20
        assert_eq!(sizer.config().base_order_size(), dec!(1)); // 200/200 = 1

        let factors = ConfidenceFactors::new(
            dec!(25),
            dec!(7),
            Signal::Neutral,
            dec!(0.0),
            dec!(15000),
        );

        let result = sizer.get_order_size(&factors);
        assert!(result.is_valid);
        assert_eq!(result.base_size, dec!(1));
    }

    #[test]
    fn test_scaling_5000_balance() {
        // $5,000 account (default)
        let sizer = ConfidenceSizer::with_balance(dec!(5000));

        assert_eq!(sizer.market_budget(), dec!(1000)); // 5000 * 0.20
        assert_eq!(sizer.config().base_order_size(), dec!(5)); // 1000/200 = 5

        let factors = ConfidenceFactors::new(
            dec!(25),
            dec!(7),
            Signal::Neutral,
            dec!(0.0),
            dec!(15000),
        );

        let result = sizer.get_order_size(&factors);
        assert!(result.is_valid);
        assert_eq!(result.base_size, dec!(5));
    }

    #[test]
    fn test_scaling_25000_balance() {
        // $25,000 account
        let sizer = ConfidenceSizer::with_balance(dec!(25000));

        assert_eq!(sizer.market_budget(), dec!(5000)); // 25000 * 0.20
        assert_eq!(sizer.config().base_order_size(), dec!(25)); // 5000/200 = 25

        let factors = ConfidenceFactors::new(
            dec!(25),
            dec!(7),
            Signal::Neutral,
            dec!(0.0),
            dec!(15000),
        );

        let result = sizer.get_order_size(&factors);
        assert!(result.is_valid);
        assert_eq!(result.base_size, dec!(25));
    }

    // =========================================================================
    // Confidence Helper Tests
    // =========================================================================

    #[test]
    fn test_time_confidence_helper() {
        // Early, far from strike
        assert_eq!(ConfidenceSizer::time_confidence(dec!(14), dec!(80)), dec!(0.6));

        // Late, far from strike
        assert_eq!(ConfidenceSizer::time_confidence(dec!(1), dec!(100)), dec!(1.6));

        // Late, close to strike
        assert_eq!(ConfidenceSizer::time_confidence(dec!(1), dec!(10)), dec!(0.8));
    }

    #[test]
    fn test_distance_confidence_helper() {
        assert_eq!(ConfidenceSizer::distance_confidence(dec!(5)), dec!(0.6)); // At strike
        assert_eq!(ConfidenceSizer::distance_confidence(dec!(25)), dec!(1.0)); // Baseline
        assert_eq!(ConfidenceSizer::distance_confidence(dec!(150)), dec!(1.5)); // Very far
    }

    #[test]
    fn test_signal_confidence_helper() {
        assert_eq!(ConfidenceSizer::signal_confidence(Signal::StrongUp), dec!(1.4));
        assert_eq!(ConfidenceSizer::signal_confidence(Signal::LeanUp), dec!(1.1));
        assert_eq!(ConfidenceSizer::signal_confidence(Signal::Neutral), dec!(0.7));
    }

    #[test]
    fn test_book_confidence_helper() {
        // Neutral imbalance, medium depth
        assert_eq!(ConfidenceSizer::book_confidence(dec!(0.1), dec!(15000)), dec!(1.0));

        // High imbalance, very deep
        assert_eq!(ConfidenceSizer::book_confidence(dec!(0.5), dec!(60000)), dec!(1.44));

        // Thin book
        assert_eq!(ConfidenceSizer::book_confidence(dec!(0.1), dec!(3000)), dec!(0.9));
    }

    // =========================================================================
    // Spec Example Tests
    // =========================================================================

    #[test]
    fn test_spec_weak_confidence_scenario() {
        // Scenario from spec: Early market, close to strike, weak signal
        // Expected: ~0.7x multiplier
        let sizer = ConfidenceSizer::with_balance(dec!(5000));

        let factors = ConfidenceFactors::new(
            dec!(15),   // Close to strike
            dec!(13),   // Early
            Signal::Neutral,
            dec!(0.0),   // No imbalance
            dec!(10000), // Medium depth
        );

        let result = sizer.get_order_size(&factors);

        assert!(result.is_valid);
        // Multiplier should be low (around 0.5-1.0)
        assert!(result.confidence_multiplier >= dec!(0.5));
        assert!(result.confidence_multiplier <= dec!(1.2));
    }

    #[test]
    fn test_spec_strong_confidence_scenario() {
        // Scenario from spec: Late market, far from strike, strong signal
        // Expected: ~2.5x multiplier
        let sizer = ConfidenceSizer::with_balance(dec!(5000));

        let factors = ConfidenceFactors::new(
            dec!(80),    // Far from strike
            dec!(2),     // Late
            Signal::StrongDown,
            dec!(-0.4),  // Favorable imbalance
            dec!(40000), // Good depth
        );

        let result = sizer.get_order_size(&factors);

        assert!(result.is_valid);
        // Multiplier should be high (around 2.0-3.0)
        assert!(result.confidence_multiplier >= dec!(2.0));
        assert!(result.confidence_multiplier <= dec!(3.0));
    }

    // =========================================================================
    // Trades Remaining Calculation Test
    // =========================================================================

    #[test]
    fn test_trades_remaining_calculation() {
        let sizer = ConfidenceSizer::with_balance(dec!(5000));
        // Market budget = 1000, base size = 5
        // If we use $10 per trade (2x base), we have ~100 trades remaining initially

        let factors = ConfidenceFactors::new(
            dec!(25),
            dec!(7),
            Signal::Neutral,
            dec!(0.0),
            dec!(15000),
        );

        let result = sizer.get_order_size(&factors);

        // Budget remaining should be close to budget - size
        assert!(result.budget_remaining < sizer.market_budget());
        // Trades remaining should be budget_remaining / base_size (rough estimate)
        assert!(result.trades_remaining > 0);
    }

    // =========================================================================
    // Serialization Tests
    // =========================================================================

    #[test]
    fn test_order_size_result_serialization() {
        let result = OrderSizeResult {
            size: dec!(10.50),
            is_valid: true,
            base_size: dec!(5),
            confidence_multiplier: dec!(2.1),
            confidence: Confidence::neutral(),
            rejection_reason: None,
            budget_remaining: dec!(500),
            trades_remaining: 100,
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("10.50"));
        assert!(json.contains("budget_remaining"));

        let parsed: OrderSizeResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.size, dec!(10.50));
        assert!(parsed.is_valid);
    }

    #[test]
    fn test_rejection_serialization() {
        let result = OrderSizeResult::rejected(SizeRejection::BudgetExhausted, dec!(0));

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("BudgetExhausted"));

        let parsed: OrderSizeResult = serde_json::from_str(&json).unwrap();
        assert!(!parsed.is_valid);
        assert_eq!(parsed.rejection_reason, Some(SizeRejection::BudgetExhausted));
    }

    #[test]
    fn test_size_rejection_display() {
        assert_eq!(SizeRejection::BudgetExhausted.to_string(), "budget exhausted");
        assert_eq!(SizeRejection::BelowMinimum.to_string(), "below minimum order size");
        assert_eq!(SizeRejection::NoBalance.to_string(), "no available balance");
    }

    // =========================================================================
    // Set Balance Test
    // =========================================================================

    #[test]
    fn test_set_available_balance() {
        let mut sizer = ConfidenceSizer::with_balance(dec!(5000));
        assert_eq!(sizer.market_budget(), dec!(1000));

        sizer.set_available_balance(dec!(10000));
        assert_eq!(sizer.market_budget(), dec!(2000)); // 10000 * 0.20
    }
}
