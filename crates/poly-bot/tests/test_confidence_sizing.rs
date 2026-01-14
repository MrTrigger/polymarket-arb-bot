//! Integration tests for confidence-based sizing system.
//!
//! These tests verify the confidence sizing components:
//! - ConfidenceSizer budget management
//! - TradingConfig balance scaling
//! - Confidence calculation integration
//! - SizingStrategy trait implementations

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use poly_bot::config::SizingConfig;
use poly_bot::strategy::{
    Confidence, ConfidenceCalculator, ConfidenceFactors, ConfidenceSizer,
    OrderSizeResult, Signal, SizeRejection,
};

// ============================================================================
// SizingConfig Tests
// ============================================================================

#[test]
fn test_sizing_config_default() {
    let config = SizingConfig::default();

    assert_eq!(config.available_balance, dec!(5000));
    assert_eq!(config.max_market_allocation, dec!(0.20));
    assert_eq!(config.expected_trades_per_market, 200);
    assert_eq!(config.min_order_size, Decimal::ONE);
    assert_eq!(config.max_confidence_multiplier, dec!(3.0));
}

#[test]
fn test_sizing_config_market_budget() {
    let config = SizingConfig::new(dec!(10000));
    // 10000 * 0.20 = 2000
    assert_eq!(config.market_budget(), dec!(2000));
}

#[test]
fn test_sizing_config_base_order_size() {
    let config = SizingConfig::new(dec!(10000));
    // budget / expected_trades = 2000 / 200 = 10
    assert_eq!(config.base_order_size(), dec!(10));
}

#[test]
fn test_sizing_config_min_order_floor() {
    // Small balance: $500
    let config = SizingConfig::new(dec!(500));
    // budget = 500 * 0.20 = 100
    // base = 100 / 200 = 0.5
    // but min is $1, so should be $1
    assert_eq!(config.base_order_size(), Decimal::ONE);
}

#[test]
fn test_sizing_config_balance_scaling() {
    // Test the balance scaling examples from the spec
    let test_cases = [
        (dec!(500), dec!(100), Decimal::ONE),  // min floor
        (dec!(1000), dec!(200), Decimal::ONE), // min floor
        (dec!(5000), dec!(1000), dec!(5)),
        (dec!(25000), dec!(5000), dec!(25)),
    ];

    for (balance, expected_budget, expected_base) in test_cases {
        let config = SizingConfig::new(balance);
        assert_eq!(
            config.market_budget(),
            expected_budget,
            "Budget mismatch for balance {}",
            balance
        );
        assert_eq!(
            config.base_order_size(),
            expected_base,
            "Base size mismatch for balance {}",
            balance
        );
    }
}

#[test]
fn test_sizing_config_daily_loss_limit() {
    let config = SizingConfig::new(dec!(10000));
    // 10000 * 0.10 = 1000
    assert_eq!(config.daily_loss_limit(), dec!(1000));
}

// ============================================================================
// ConfidenceSizer Tests
// ============================================================================

#[test]
fn test_confidence_sizer_new() {
    let config = SizingConfig::new(dec!(5000));
    let sizer = ConfidenceSizer::new(config);

    assert_eq!(sizer.market_budget(), dec!(1000)); // 5000 * 0.20
    assert_eq!(sizer.spent(), Decimal::ZERO);
    assert_eq!(sizer.remaining_budget(), dec!(1000));
    assert!(!sizer.is_budget_exhausted());
}

#[test]
fn test_confidence_sizer_with_balance() {
    let sizer = ConfidenceSizer::with_balance(dec!(10000));

    assert_eq!(sizer.market_budget(), dec!(2000)); // 10000 * 0.20
}

#[test]
fn test_confidence_sizer_neutral_confidence() {
    let sizer = ConfidenceSizer::with_balance(dec!(5000));
    let factors = ConfidenceFactors::neutral();

    let result = sizer.get_order_size(&factors);

    assert!(result.is_valid);
    assert!(result.rejection_reason.is_none());
    // Neutral confidence should give ~1.5x multiplier
    assert!(result.confidence_multiplier >= dec!(1.0));
    assert!(result.confidence_multiplier <= dec!(2.0));
}

#[test]
fn test_confidence_sizer_high_confidence() {
    let sizer = ConfidenceSizer::with_balance(dec!(5000));

    // Strong signal, good distance, good book
    let factors = ConfidenceFactors::new(
        dec!(1000),  // $1000 from strike
        dec!(5),     // 5 minutes remaining
        Signal::StrongUp,
        dec!(0.5),   // Strong positive imbalance
        dec!(10000), // Good depth
    );

    let result = sizer.get_order_size(&factors);

    assert!(result.is_valid);
    // High confidence should give higher multiplier
    assert!(result.confidence_multiplier >= dec!(1.5));
    assert!(result.size > result.base_size); // Size should be scaled up
}

#[test]
fn test_confidence_sizer_low_confidence() {
    let sizer = ConfidenceSizer::with_balance(dec!(5000));

    // Weak signal, close to strike, poor book
    let factors = ConfidenceFactors::new(
        dec!(10),    // Only $10 from strike
        dec!(14),    // 14 minutes (early, uncertain)
        Signal::LeanUp,
        dec!(-0.5),  // Against us
        dec!(100),   // Poor depth
    );

    let result = sizer.get_order_size(&factors);

    assert!(result.is_valid);
    // Low confidence should give lower multiplier
    assert!(result.confidence_multiplier < dec!(1.5));
}

#[test]
fn test_confidence_sizer_multiplier_capped_high() {
    let sizer = ConfidenceSizer::with_balance(dec!(5000));

    // Extreme confidence factors (would exceed 3.0x)
    let factors = ConfidenceFactors::new(
        dec!(5000),  // Very far from strike
        dec!(1),     // Very late (high certainty)
        Signal::StrongUp,
        dec!(0.9),   // Extreme positive imbalance
        dec!(50000), // Excellent depth
    );

    let result = sizer.get_order_size(&factors);

    // Multiplier should be capped at 3.0x
    assert!(result.confidence_multiplier <= dec!(3.0));
}

#[test]
fn test_confidence_sizer_multiplier_capped_low() {
    let sizer = ConfidenceSizer::with_balance(dec!(5000));

    // Very low confidence factors
    let factors = ConfidenceFactors::new(
        dec!(1),     // Extremely close to strike
        dec!(14),    // Early (uncertain)
        Signal::Neutral,  // No directional edge
        dec!(-0.9),  // Extreme negative imbalance
        dec!(10),    // Very poor depth
    );

    let result = sizer.get_order_size(&factors);

    // Multiplier should be at least 0.5x
    assert!(result.confidence_multiplier >= dec!(0.5));
}

#[test]
fn test_confidence_sizer_budget_tracking() {
    let mut sizer = ConfidenceSizer::with_balance(dec!(5000));
    let factors = ConfidenceFactors::neutral();

    // Get initial size
    let result1 = sizer.get_order_size(&factors);
    let size1 = result1.size;

    // Record the trade
    sizer.record_trade(size1);

    // Check budget tracking
    assert_eq!(sizer.spent(), size1);
    assert_eq!(sizer.remaining_budget(), dec!(1000) - size1);
}

#[test]
fn test_confidence_sizer_budget_exhaustion() {
    let mut sizer = ConfidenceSizer::with_balance(dec!(100)); // Small balance
    let factors = ConfidenceFactors::neutral();

    // Budget = 100 * 0.20 = $20

    // Spend all the budget
    sizer.record_trade(dec!(20));

    // Next sizing should return budget exhausted
    let result = sizer.get_order_size(&factors);

    assert!(!result.is_valid);
    assert_eq!(result.rejection_reason, Some(SizeRejection::BudgetExhausted));
}

#[test]
fn test_confidence_sizer_caps_at_remaining_budget() {
    let mut sizer = ConfidenceSizer::with_balance(dec!(5000));
    let factors = ConfidenceFactors::neutral();

    // Budget = $1000
    // Spend most of it
    sizer.record_trade(dec!(995));

    // Only $5 remaining
    let result = sizer.get_order_size(&factors);

    // Size should be capped at remaining budget
    assert!(result.size <= dec!(5));
}

#[test]
fn test_confidence_sizer_below_minimum() {
    let mut sizer = ConfidenceSizer::with_balance(dec!(5000));
    let factors = ConfidenceFactors::neutral();

    // Budget = $1000
    // Spend almost all of it, leaving less than minimum ($1)
    sizer.record_trade(dec!(999.5));

    let result = sizer.get_order_size(&factors);

    // Should reject as below minimum
    assert!(!result.is_valid);
    assert_eq!(result.rejection_reason, Some(SizeRejection::BelowMinimum));
}

#[test]
fn test_confidence_sizer_reset() {
    let mut sizer = ConfidenceSizer::with_balance(dec!(5000));

    // Spend some budget
    sizer.record_trade(dec!(500));
    assert_eq!(sizer.spent(), dec!(500));

    // Reset for new market session
    sizer.reset();

    assert_eq!(sizer.spent(), Decimal::ZERO);
    assert_eq!(sizer.remaining_budget(), dec!(1000));
}

// ============================================================================
// OrderSizeResult Tests
// ============================================================================

#[test]
fn test_order_size_result_valid() {
    let sizer = ConfidenceSizer::with_balance(dec!(5000));
    let factors = ConfidenceFactors::neutral();
    let result = sizer.get_order_size(&factors);

    assert!(result.is_valid);
    assert!(result.size > Decimal::ZERO);
    assert!(result.base_size > Decimal::ZERO);
    assert!(result.rejection_reason.is_none());
    assert!(result.budget_remaining >= Decimal::ZERO);
    assert!(result.trades_remaining > 0);
}

#[test]
fn test_order_size_result_rejected() {
    let rejected = OrderSizeResult::rejected(SizeRejection::BudgetExhausted, Decimal::ZERO);

    assert!(!rejected.is_valid);
    assert_eq!(rejected.size, Decimal::ZERO);
    assert_eq!(rejected.rejection_reason, Some(SizeRejection::BudgetExhausted));
}

#[test]
fn test_order_size_result_serialization() {
    let sizer = ConfidenceSizer::with_balance(dec!(5000));
    let factors = ConfidenceFactors::neutral();
    let result = sizer.get_order_size(&factors);

    let serialized = serde_json::to_string(&result).unwrap();
    assert!(serialized.contains("is_valid"));
    assert!(serialized.contains("size"));
    assert!(serialized.contains("confidence_multiplier"));

    let deserialized: OrderSizeResult = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized.is_valid, result.is_valid);
}

// ============================================================================
// Confidence Integration Tests
// ============================================================================

#[test]
fn test_confidence_total_multiplier_bounds() {
    // Test various factor combinations
    let test_cases = [
        // (distance, minutes, signal, imbalance, depth)
        (dec!(1000), dec!(5), Signal::StrongUp, dec!(0.5), dec!(5000)),
        (dec!(10), dec!(14), Signal::LeanUp, dec!(-0.3), dec!(500)),
        (dec!(100), dec!(10), Signal::Neutral, dec!(0.0), dec!(1000)),
        (dec!(500), dec!(3), Signal::LeanDown, dec!(0.2), dec!(2000)),
        (dec!(2000), dec!(1), Signal::StrongDown, dec!(-0.5), dec!(8000)),
    ];

    for (distance, minutes, signal, imbalance, depth) in test_cases {
        let factors = ConfidenceFactors::new(distance, minutes, signal, imbalance, depth);
        let confidence = ConfidenceCalculator::calculate(&factors);
        let multiplier = confidence.total_multiplier();

        // All multipliers should be within bounds
        assert!(
            multiplier >= dec!(0.5) && multiplier <= dec!(3.0),
            "Multiplier {} out of bounds for factors: {:?}",
            multiplier,
            factors
        );
    }
}

#[test]
fn test_confidence_neutral_returns_baseline() {
    let neutral = Confidence::neutral();
    let multiplier = neutral.total_multiplier();

    // Neutral confidence should give approximately 1.5x (baseline)
    assert!(multiplier >= dec!(1.0) && multiplier <= dec!(2.0));
}

// ============================================================================
// SizeRejection Tests
// ============================================================================

#[test]
fn test_size_rejection_display() {
    assert_eq!(format!("{}", SizeRejection::BudgetExhausted), "budget exhausted");
    assert_eq!(format!("{}", SizeRejection::BelowMinimum), "below minimum order size");
    assert_eq!(format!("{}", SizeRejection::NoBalance), "no available balance");
}

#[test]
fn test_size_rejection_serialization() {
    let rejections = [
        SizeRejection::BudgetExhausted,
        SizeRejection::BelowMinimum,
        SizeRejection::NoBalance,
    ];

    for rejection in rejections {
        let serialized = serde_json::to_string(&rejection).unwrap();
        let deserialized: SizeRejection = serde_json::from_str(&serialized).unwrap();
        assert_eq!(rejection, deserialized);
    }
}

// ============================================================================
// Multiple Market Session Tests
// ============================================================================

#[test]
fn test_sizer_multiple_trades() {
    let mut sizer = ConfidenceSizer::with_balance(dec!(5000));
    let factors = ConfidenceFactors::neutral();

    // Simulate multiple trades
    for i in 0..10 {
        let result = sizer.get_order_size(&factors);

        if result.is_valid {
            sizer.record_trade(result.size);
            assert!(
                sizer.spent() > Decimal::ZERO,
                "Spent should increase after trade {}",
                i
            );
        }
    }

    // Should have spent some budget
    assert!(sizer.spent() > Decimal::ZERO);
    // Remaining should be less than initial
    assert!(sizer.remaining_budget() < dec!(1000));
}

#[test]
fn test_sizer_trades_remaining_estimate() {
    let sizer = ConfidenceSizer::with_balance(dec!(5000));
    let factors = ConfidenceFactors::neutral();

    let result = sizer.get_order_size(&factors);

    // trades_remaining should be a reasonable estimate
    assert!(result.trades_remaining > 0);
    assert!(result.trades_remaining <= 200); // Can't exceed expected trades
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn test_sizer_zero_balance() {
    let config = SizingConfig::new(Decimal::ZERO);
    let sizer = ConfidenceSizer::new(config);
    let factors = ConfidenceFactors::neutral();

    let result = sizer.get_order_size(&factors);

    assert!(!result.is_valid);
}

#[test]
fn test_sizer_very_large_balance() {
    let sizer = ConfidenceSizer::with_balance(dec!(1000000)); // $1M
    let factors = ConfidenceFactors::neutral();

    let result = sizer.get_order_size(&factors);

    assert!(result.is_valid);
    // Budget = $200,000, base size = $1,000
    assert!(result.base_size >= dec!(1000));
}

#[test]
fn test_confidence_factors_neutral() {
    let neutral = ConfidenceFactors::neutral();

    // Neutral factors should be reasonable baseline values
    assert!(neutral.distance_to_strike > Decimal::ZERO);
    assert!(neutral.minutes_remaining > Decimal::ZERO);
    assert_eq!(neutral.signal, Signal::Neutral);
    assert_eq!(neutral.book_imbalance, Decimal::ZERO);
    assert!(neutral.favorable_depth > Decimal::ZERO);
}
