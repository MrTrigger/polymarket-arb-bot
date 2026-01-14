//! Integration tests for directional trading strategy.
//!
//! These tests verify the directional strategy components:
//! - Signal detection based on price vs strike
//! - DirectionalDetector opportunity detection
//! - Confidence calculation integration
//! - Signal-based allocation ratios

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use poly_bot::strategy::{
    ConfidenceCalculator, ConfidenceFactors, DirectionalConfig, DirectionalDetector,
    DirectionalOpportunity, DirectionalSkipReason, Signal,
};
use poly_bot::strategy::signal::{calculate_distance, get_signal, get_thresholds};
use poly_bot::types::{MarketState, PriceLevel};
#[allow(unused_imports)]
use poly_bot::strategy::Confidence;
use poly_common::types::CryptoAsset;

// ============================================================================
// Test Helpers
// ============================================================================

/// Create a test market state with configurable parameters.
fn create_market_state(
    spot_price: Option<Decimal>,
    strike_price: Decimal,
    seconds_remaining: i64,
) -> MarketState {
    let mut state = MarketState::new(
        "test-event".to_string(),
        CryptoAsset::Btc,
        "yes-token".to_string(),
        "no-token".to_string(),
        strike_price,
        seconds_remaining,
    );
    state.spot_price = spot_price;
    state
}

/// Add order book levels to a market state.
fn add_book_levels(
    state: &mut MarketState,
    yes_bid: Decimal,
    yes_ask: Decimal,
    no_bid: Decimal,
    no_ask: Decimal,
    depth: Decimal,
) {
    state.yes_book.apply_snapshot(
        vec![PriceLevel::new(yes_bid, depth)],
        vec![PriceLevel::new(yes_ask, depth)],
        0,
    );
    state.no_book.apply_snapshot(
        vec![PriceLevel::new(no_bid, depth)],
        vec![PriceLevel::new(no_ask, depth)],
        0,
    );
}

// ============================================================================
// Signal Detection Tests
// ============================================================================

#[test]
fn test_signal_strong_up_far_above_strike() {
    // BTC at $100,100, strike at $100,000 = 0.1% above
    // With >12 minutes remaining, 0.06% strong threshold -> StrongUp
    let signal = get_signal(dec!(100100), dec!(100000), dec!(15));
    assert_eq!(signal, Signal::StrongUp);
    assert!(signal.is_directional());
    assert!(signal.is_strong());
}

#[test]
fn test_signal_lean_up_slightly_above_strike() {
    // BTC at $100,040, strike at $100,000 = 0.04% above
    // With >12 minutes remaining, 0.03% lean, 0.06% strong threshold -> LeanUp
    let signal = get_signal(dec!(100040), dec!(100000), dec!(15));
    assert_eq!(signal, Signal::LeanUp);
    assert!(signal.is_directional());
    assert!(!signal.is_strong());
}

#[test]
fn test_signal_neutral_at_strike() {
    // BTC at $100,020, strike at $100,000 = 0.02% above
    // With >12 minutes remaining, 0.03% lean threshold -> Neutral (below threshold)
    let signal = get_signal(dec!(100020), dec!(100000), dec!(15));
    assert_eq!(signal, Signal::Neutral);
    assert!(!signal.is_directional());
}

#[test]
fn test_signal_strong_down_far_below_strike() {
    // BTC at $99,900, strike at $100,000 = 0.1% below
    // With >12 min, 0.06% strong threshold -> StrongDown
    let signal = get_signal(dec!(99900), dec!(100000), dec!(15));
    assert_eq!(signal, Signal::StrongDown);
    assert!(signal.is_directional());
    assert!(signal.is_strong());
}

#[test]
fn test_signal_thresholds_tighten_with_time() {
    // Verify thresholds decrease as time remaining decreases
    // New time brackets: >12, 9-12, 6-9, 3-6, <3 min (percentage-based)
    let early = get_thresholds(dec!(15));  // >12 min: 0.060%/0.030%
    let mid = get_thresholds(dec!(10));    // 9-12 min: 0.050%/0.025%
    let mid2 = get_thresholds(dec!(7));    // 6-9 min: 0.040%/0.015%
    let late = get_thresholds(dec!(4));    // 3-6 min: 0.035%/0.012%
    let very_late = get_thresholds(dec!(2)); // <3 min: 0.025%/0.008%

    // Strong thresholds should decrease
    assert!(early.strong > mid.strong);
    assert!(mid.strong > mid2.strong);
    assert!(mid2.strong > late.strong);
    assert!(late.strong > very_late.strong);

    // Lean thresholds should also decrease
    assert!(early.lean > mid.lean);
    assert!(mid2.lean > late.lean);
    assert!(late.lean > very_late.lean);
}

#[test]
fn test_signal_late_window_detects_smaller_moves() {
    // Same 0.01% move should be Neutral early but directional late
    let spot = dec!(100010);  // 0.01% above strike
    let strike = dec!(100000);

    // Early (>12 min): 0.01% < 0.03% lean threshold -> Neutral
    let early_signal = get_signal(spot, strike, dec!(15));
    assert_eq!(early_signal, Signal::Neutral);

    // Very late (<3 min): 0.01% > 0.008% lean threshold -> directional
    let late_signal = get_signal(spot, strike, dec!(2));
    // At <3 min, lean threshold is 0.008%, strong threshold is 0.025%
    // 0.01% is between lean and strong, so should be LeanUp
    assert!(late_signal.is_directional());
}

// ============================================================================
// Allocation Ratio Tests
// ============================================================================

#[test]
fn test_signal_allocation_ratios_strong_up() {
    let signal = Signal::StrongUp;
    assert_eq!(signal.up_ratio(), dec!(0.78));
    assert_eq!(signal.down_ratio(), dec!(0.22));
    // Ratios should sum to 1.0
    assert_eq!(signal.up_ratio() + signal.down_ratio(), Decimal::ONE);
}

#[test]
fn test_signal_allocation_ratios_lean_up() {
    let signal = Signal::LeanUp;
    assert_eq!(signal.up_ratio(), dec!(0.60));
    assert_eq!(signal.down_ratio(), dec!(0.40));
    assert_eq!(signal.up_ratio() + signal.down_ratio(), Decimal::ONE);
}

#[test]
fn test_signal_allocation_ratios_neutral() {
    let signal = Signal::Neutral;
    assert_eq!(signal.up_ratio(), dec!(0.50));
    assert_eq!(signal.down_ratio(), dec!(0.50));
    assert_eq!(signal.up_ratio() + signal.down_ratio(), Decimal::ONE);
}

#[test]
fn test_signal_allocation_ratios_lean_down() {
    let signal = Signal::LeanDown;
    assert_eq!(signal.up_ratio(), dec!(0.40));
    assert_eq!(signal.down_ratio(), dec!(0.60));
    assert_eq!(signal.up_ratio() + signal.down_ratio(), Decimal::ONE);
}

#[test]
fn test_signal_allocation_ratios_strong_down() {
    let signal = Signal::StrongDown;
    assert_eq!(signal.up_ratio(), dec!(0.22));
    assert_eq!(signal.down_ratio(), dec!(0.78));
    assert_eq!(signal.up_ratio() + signal.down_ratio(), Decimal::ONE);
}

// ============================================================================
// DirectionalDetector Tests
// ============================================================================

#[test]
fn test_detector_detects_strong_up_opportunity() {
    let detector = DirectionalDetector::new();

    // Create state with BTC 1% above strike
    let mut state = create_market_state(Some(dec!(101000)), dec!(100000), 600);
    add_book_levels(&mut state, dec!(0.45), dec!(0.50), dec!(0.45), dec!(0.48), dec!(1000));

    let result = detector.detect(&state);
    assert!(result.is_ok());

    let opp = result.unwrap();
    assert_eq!(opp.signal, Signal::StrongUp);
    assert_eq!(opp.up_ratio, dec!(0.78));
    assert_eq!(opp.down_ratio, dec!(0.22));
}

#[test]
fn test_detector_detects_strong_down_opportunity() {
    let detector = DirectionalDetector::new();

    // Create state with BTC 1% below strike
    let mut state = create_market_state(Some(dec!(99000)), dec!(100000), 600);
    add_book_levels(&mut state, dec!(0.48), dec!(0.52), dec!(0.45), dec!(0.50), dec!(1000));

    let result = detector.detect(&state);
    assert!(result.is_ok());

    let opp = result.unwrap();
    assert_eq!(opp.signal, Signal::StrongDown);
    assert_eq!(opp.up_ratio, dec!(0.22));
    assert_eq!(opp.down_ratio, dec!(0.78));
}

#[test]
fn test_detector_skips_neutral_signal() {
    let detector = DirectionalDetector::new();

    // Create state with BTC at exactly strike (no directional signal)
    let mut state = create_market_state(Some(dec!(100000)), dec!(100000), 600);
    add_book_levels(&mut state, dec!(0.48), dec!(0.52), dec!(0.48), dec!(0.52), dec!(1000));

    let result = detector.detect(&state);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), DirectionalSkipReason::NeutralSignal);
}

#[test]
fn test_detector_skips_without_spot_price() {
    let detector = DirectionalDetector::new();

    // Create state without spot price
    let mut state = create_market_state(None, dec!(100000), 600);
    add_book_levels(&mut state, dec!(0.48), dec!(0.52), dec!(0.48), dec!(0.52), dec!(1000));

    let result = detector.detect(&state);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), DirectionalSkipReason::NoSpotPrice);
}

#[test]
fn test_detector_skips_insufficient_time() {
    let detector = DirectionalDetector::new();

    // Create state with only 30 seconds remaining (below default 60s minimum)
    let mut state = create_market_state(Some(dec!(101000)), dec!(100000), 30);
    add_book_levels(&mut state, dec!(0.48), dec!(0.52), dec!(0.48), dec!(0.52), dec!(1000));

    let result = detector.detect(&state);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), DirectionalSkipReason::InsufficientTime);
}

#[test]
fn test_detector_skips_book_not_ready() {
    let detector = DirectionalDetector::new();

    // Create state with empty order books
    let state = create_market_state(Some(dec!(101000)), dec!(100000), 600);
    // Don't add book levels - books remain empty

    let result = detector.detect(&state);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), DirectionalSkipReason::BookNotReady);
}

#[test]
fn test_detector_calculates_combined_cost() {
    let detector = DirectionalDetector::new();

    // YES ask = 0.50, NO ask = 0.48 -> combined = 0.98
    // Use tight spreads (< 10% to pass spread check)
    let mut state = create_market_state(Some(dec!(101000)), dec!(100000), 600);
    add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.46), dec!(0.48), dec!(1000));

    let result = detector.detect(&state);
    assert!(result.is_ok());

    let opp = result.unwrap();
    assert_eq!(opp.combined_cost, dec!(0.98));
    assert_eq!(opp.expected_profit(), dec!(0.02)); // $1.00 - $0.98 = $0.02
}

#[test]
fn test_detector_has_potential_filter() {
    let detector = DirectionalDetector::new();

    // State without spot price
    let state = create_market_state(None, dec!(100000), 600);
    assert!(!detector.has_potential(&state));

    // State with spot but no books
    let state = create_market_state(Some(dec!(100500)), dec!(100000), 600);
    assert!(!detector.has_potential(&state));

    // State with insufficient time
    let mut state = create_market_state(Some(dec!(100500)), dec!(100000), 30);
    add_book_levels(&mut state, dec!(0.48), dec!(0.52), dec!(0.48), dec!(0.52), dec!(1000));
    assert!(!detector.has_potential(&state));

    // Valid state
    let mut state = create_market_state(Some(dec!(100500)), dec!(100000), 600);
    add_book_levels(&mut state, dec!(0.48), dec!(0.52), dec!(0.48), dec!(0.52), dec!(1000));
    assert!(detector.has_potential(&state));
}

// ============================================================================
// Confidence Integration Tests
// ============================================================================

#[test]
fn test_confidence_factors_integration() {
    // Create factors matching a strong signal scenario
    let factors = ConfidenceFactors::new(
        dec!(1000),  // $1000 distance from strike
        dec!(10),    // 10 minutes remaining
        Signal::StrongUp,
        dec!(0.3),   // Moderate positive imbalance
        dec!(5000),  // $5000 depth
    );

    let confidence = ConfidenceCalculator::calculate(&factors);

    // Total multiplier should be in valid range
    assert!(confidence.total_multiplier() >= dec!(0.5));
    assert!(confidence.total_multiplier() <= dec!(3.0));

    // Strong signal should contribute positively
    assert!(confidence.signal >= Decimal::ONE);
}

#[test]
fn test_confidence_weak_scenario() {
    // Create factors matching a weak signal scenario
    let factors = ConfidenceFactors::new(
        dec!(50),    // Only $50 distance (close to strike)
        dec!(14),    // 14 minutes remaining (early, less certain)
        Signal::LeanUp,  // Weak signal
        dec!(0.0),   // Neutral imbalance
        dec!(500),   // Low depth
    );

    let confidence = ConfidenceCalculator::calculate(&factors);

    // Weak scenario should produce lower multiplier
    // But still within valid bounds
    assert!(confidence.total_multiplier() >= dec!(0.5));
    assert!(confidence.total_multiplier() < dec!(2.0));
}

#[test]
fn test_directional_opportunity_confidence_multiplier() {
    let detector = DirectionalDetector::new();

    // Strong up signal with good book
    let mut state = create_market_state(Some(dec!(101500)), dec!(100000), 300);
    add_book_levels(&mut state, dec!(0.45), dec!(0.50), dec!(0.45), dec!(0.48), dec!(5000));

    let result = detector.detect(&state);
    assert!(result.is_ok());

    let opp = result.unwrap();

    // Should have valid confidence multiplier
    let multiplier = opp.confidence_multiplier();
    assert!(multiplier >= dec!(0.5));
    assert!(multiplier <= dec!(3.0));
}

// ============================================================================
// Distance Calculation Tests
// ============================================================================

#[test]
fn test_distance_calculation_above_strike() {
    let distance = calculate_distance(dec!(101000), dec!(100000));
    // 1% above strike
    assert_eq!(distance, dec!(0.01));
}

#[test]
fn test_distance_calculation_below_strike() {
    let distance = calculate_distance(dec!(99000), dec!(100000));
    // 1% below strike (signed - negative for below)
    assert_eq!(distance, dec!(-0.01));
}

#[test]
fn test_distance_calculation_at_strike() {
    let distance = calculate_distance(dec!(100000), dec!(100000));
    assert_eq!(distance, Decimal::ZERO);
}

// ============================================================================
// Custom Config Tests
// ============================================================================

#[test]
fn test_directional_config_custom() {
    let config = DirectionalConfig {
        min_seconds_remaining: 120,  // 2 minutes minimum
        max_combined_cost: dec!(0.98),  // 2% minimum margin
        max_spread_ratio: dec!(0.05),   // 5% max spread
        min_favorable_depth: dec!(500), // $500 minimum depth
    };

    let detector = DirectionalDetector::with_config(config);

    // Should skip with insufficient time (90 seconds < 120 seconds)
    let mut state = create_market_state(Some(dec!(101000)), dec!(100000), 90);
    add_book_levels(&mut state, dec!(0.45), dec!(0.50), dec!(0.45), dec!(0.48), dec!(1000));

    let result = detector.detect(&state);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), DirectionalSkipReason::InsufficientTime);
}

#[test]
fn test_directional_config_spread_check() {
    let config = DirectionalConfig {
        min_seconds_remaining: 60,
        max_combined_cost: dec!(0.995),
        max_spread_ratio: dec!(0.02),  // Very tight 2% max spread
        min_favorable_depth: dec!(100),
    };

    let detector = DirectionalDetector::with_config(config);

    // Create state with wide spread (10%)
    let mut state = create_market_state(Some(dec!(101000)), dec!(100000), 600);
    // bid=0.40, ask=0.50 -> spread = 20%
    add_book_levels(&mut state, dec!(0.40), dec!(0.50), dec!(0.40), dec!(0.50), dec!(1000));

    let result = detector.detect(&state);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), DirectionalSkipReason::SpreadTooWide);
}

// ============================================================================
// Opportunity Properties Tests
// ============================================================================

#[test]
fn test_opportunity_expected_profit() {
    let detector = DirectionalDetector::new();

    // Combined cost = 0.50 + 0.48 = 0.98
    // Use tight spreads (< 10%)
    let mut state = create_market_state(Some(dec!(101000)), dec!(100000), 600);
    add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.46), dec!(0.48), dec!(1000));

    let opp = detector.detect(&state).unwrap();

    // Profit = $1.00 - $0.98 = $0.02 per share pair
    assert_eq!(opp.expected_profit(), dec!(0.02));
}

#[test]
fn test_opportunity_is_strong_check() {
    let detector = DirectionalDetector::new();

    // Strong signal (0.1% above strike, at 10 min = 9-12 bracket, 0.05% strong threshold)
    // Use tight spreads (< 10%)
    let mut strong_state = create_market_state(Some(dec!(100100)), dec!(100000), 600);
    add_book_levels(&mut strong_state, dec!(0.48), dec!(0.50), dec!(0.46), dec!(0.48), dec!(1000));
    let strong_opp = detector.detect(&strong_state).unwrap();
    assert!(strong_opp.is_strong());

    // Lean signal (0.035% above strike, at 10 min, 0.025% lean < 0.035% < 0.05% strong)
    let mut lean_state = create_market_state(Some(dec!(100035)), dec!(100000), 600);
    add_book_levels(&mut lean_state, dec!(0.48), dec!(0.50), dec!(0.46), dec!(0.48), dec!(1000));
    let lean_opp = detector.detect(&lean_state).unwrap();
    assert!(!lean_opp.is_strong());
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn test_detector_invalid_strike_zero() {
    let detector = DirectionalDetector::new();

    // Strike of zero should be rejected
    let mut state = create_market_state(Some(dec!(100000)), Decimal::ZERO, 600);
    add_book_levels(&mut state, dec!(0.48), dec!(0.52), dec!(0.48), dec!(0.52), dec!(1000));

    let result = detector.detect(&state);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), DirectionalSkipReason::InvalidStrike);
}

#[test]
fn test_all_time_brackets_produce_valid_signals() {
    // Test that all 5 time brackets produce valid signals
    let spot = dec!(101000);  // 1% above strike
    let strike = dec!(100000);

    let time_brackets = [
        (dec!(12), "> 10 min"),
        (dec!(7), "5-10 min"),
        (dec!(3), "2-5 min"),
        (dec!(1.5), "1-2 min"),
        (dec!(0.5), "< 1 min"),
    ];

    for (minutes, _description) in time_brackets {
        let signal = get_signal(spot, strike, minutes);
        assert!(signal.is_directional(), "1% move should be directional in all brackets");
    }
}

#[test]
fn test_signal_serialization_roundtrip() {
    let signals = [
        Signal::StrongUp,
        Signal::LeanUp,
        Signal::Neutral,
        Signal::LeanDown,
        Signal::StrongDown,
    ];

    for signal in signals {
        let serialized = serde_json::to_string(&signal).unwrap();
        let deserialized: Signal = serde_json::from_str(&serialized).unwrap();
        assert_eq!(signal, deserialized);
    }
}

#[test]
fn test_directional_opportunity_serialization() {
    let detector = DirectionalDetector::new();

    // Use tight spreads (< 10%)
    let mut state = create_market_state(Some(dec!(101000)), dec!(100000), 600);
    add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.46), dec!(0.48), dec!(1000));

    let opp = detector.detect(&state).unwrap();

    // Should serialize to JSON without error
    let serialized = serde_json::to_string(&opp).unwrap();
    assert!(serialized.contains("StrongUp"));

    // Should deserialize back
    let deserialized: DirectionalOpportunity = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized.signal, opp.signal);
    assert_eq!(deserialized.up_ratio, opp.up_ratio);
}
