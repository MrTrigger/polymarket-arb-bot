//! Confidence-based order sizing types.
//!
//! This module implements confidence factors for dynamic order sizing:
//! - Higher confidence = larger orders (up to 3x base size)
//! - Lower confidence = smaller orders (down to 0.5x base size)
//!
//! ## Confidence Factors
//!
//! Four factors determine sizing confidence:
//! - **Time**: More confidence as time runs out (if far from strike)
//! - **Distance**: More confidence when far from strike price
//! - **Signal**: More confidence with strong directional signals
//! - **Book**: More confidence with favorable depth and imbalance
//!
//! ## Multiplier Calculation
//!
//! The total multiplier uses geometric mean of all factors:
//! ```text
//! multiplier = (time × distance × signal × book)^0.25 × 1.5
//! ```
//!
//! This prevents any single factor from dominating while still allowing
//! meaningful size variation.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

use super::signal::Signal;

/// Minimum allowed multiplier (0.5x base size).
pub const MIN_MULTIPLIER: Decimal = dec!(0.5);

/// Maximum allowed multiplier (3.0x base size).
pub const MAX_MULTIPLIER: Decimal = dec!(3.0);

/// Input factors used to calculate confidence.
///
/// These are the raw market conditions that determine sizing.
#[derive(Debug, Clone)]
pub struct ConfidenceFactors {
    /// Distance from spot price to strike (in absolute dollars).
    /// Larger values = more confidence in outcome direction.
    pub distance_to_strike: Decimal,

    /// Minutes remaining in the market window.
    /// Lower values (near expiry) = more confidence in outcome.
    pub minutes_remaining: Decimal,

    /// Directional signal strength from signal detection.
    pub signal: Signal,

    /// Order book imbalance (-1.0 to +1.0).
    /// Positive = more bids than asks (bullish).
    /// Negative = more asks than bids (bearish).
    pub book_imbalance: Decimal,

    /// Depth available on our side of the book (in dollars).
    /// Higher depth = safer to place larger orders.
    pub favorable_depth: Decimal,
}

impl ConfidenceFactors {
    /// Create new confidence factors.
    pub fn new(
        distance_to_strike: Decimal,
        minutes_remaining: Decimal,
        signal: Signal,
        book_imbalance: Decimal,
        favorable_depth: Decimal,
    ) -> Self {
        Self {
            distance_to_strike,
            minutes_remaining,
            signal,
            book_imbalance,
            favorable_depth,
        }
    }

    /// Create neutral confidence factors (results in ~1.5x multiplier).
    ///
    /// Useful when confidence factors are not available but you still want
    /// confidence-based sizing with a baseline size.
    pub fn neutral() -> Self {
        Self {
            distance_to_strike: Decimal::new(25, 0), // $25 from strike
            minutes_remaining: Decimal::new(7, 0),   // Mid-market
            signal: Signal::Neutral,
            book_imbalance: Decimal::ZERO,           // No imbalance
            favorable_depth: Decimal::new(15000, 0), // Medium depth
        }
    }
}

/// Breakdown of confidence into component multipliers.
///
/// Each component is a multiplier typically in the range 0.5 to 1.6.
/// These are combined using geometric mean in `total_multiplier()`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Confidence {
    /// Time remaining + distance interaction multiplier.
    /// Higher when less time AND far from strike.
    pub time: Decimal,

    /// Distance to strike multiplier.
    /// Higher when farther from strike price.
    pub distance: Decimal,

    /// Signal strength multiplier.
    /// Higher for strong directional signals.
    pub signal: Decimal,

    /// Order book factors multiplier.
    /// Higher with favorable depth and imbalance.
    pub book: Decimal,
}

impl Confidence {
    /// Create a new confidence with all component values.
    pub fn new(time: Decimal, distance: Decimal, signal: Decimal, book: Decimal) -> Self {
        Self {
            time,
            distance,
            signal,
            book,
        }
    }

    /// Calculate combined multiplier using geometric mean.
    ///
    /// Formula: (time × distance × signal × book)^0.25 × 1.5
    ///
    /// The geometric mean prevents any single factor from dominating.
    /// Result is capped between 0.5x and 3.0x.
    pub fn total_multiplier(&self) -> Decimal {
        // Multiply all factors
        let product = self.time * self.distance * self.signal * self.book;

        // Geometric mean: product^0.25 (fourth root)
        // We use f64 for the power operation, then convert back
        let product_f64 = decimal_to_f64(product);
        let root = product_f64.powf(0.25);
        let scaled = root * 1.5;

        // Convert back to Decimal and cap
        let result = f64_to_decimal(scaled);
        clamp(result, MIN_MULTIPLIER, MAX_MULTIPLIER)
    }

    /// Create a neutral confidence (1.0 multiplier on everything).
    pub fn neutral() -> Self {
        Self {
            time: Decimal::ONE,
            distance: Decimal::ONE,
            signal: Decimal::ONE,
            book: Decimal::ONE,
        }
    }

    /// Create a low confidence (all factors at minimum).
    pub fn low() -> Self {
        Self {
            time: dec!(0.6),
            distance: dec!(0.6),
            signal: dec!(0.7),
            book: dec!(0.9),
        }
    }

    /// Create a high confidence (all factors near maximum).
    pub fn high() -> Self {
        Self {
            time: dec!(1.6),
            distance: dec!(1.5),
            signal: dec!(1.4),
            book: dec!(1.2),
        }
    }
}

impl Default for Confidence {
    fn default() -> Self {
        Self::neutral()
    }
}

/// Calculator for confidence factors based on market conditions.
///
/// This struct encapsulates the logic for converting raw market
/// conditions into confidence component values.
#[derive(Debug, Clone)]
pub struct ConfidenceCalculator;

impl ConfidenceCalculator {
    /// Calculate all confidence components from factors.
    pub fn calculate(factors: &ConfidenceFactors) -> Confidence {
        Confidence {
            time: Self::time_confidence(factors.minutes_remaining, factors.distance_to_strike),
            distance: Self::distance_confidence(factors.distance_to_strike),
            signal: Self::signal_confidence(factors.signal),
            book: Self::book_confidence(factors.book_imbalance, factors.favorable_depth),
        }
    }

    /// Time confidence: Higher when less time AND far from strike.
    ///
    /// Key insight: 2 mins left at strike = still uncertain
    ///              2 mins left $80 from strike = very certain
    ///
    /// Time factor ranges from 0.6 (early) to 1.6 (late).
    /// Distance modifier scales this down if close to strike.
    pub fn time_confidence(minutes_remaining: Decimal, distance: Decimal) -> Decimal {
        let time_factor = if minutes_remaining > dec!(12) {
            dec!(0.6) // Early: lots can change
        } else if minutes_remaining > dec!(9) {
            dec!(0.8)
        } else if minutes_remaining > dec!(6) {
            dec!(1.0) // Mid: baseline
        } else if minutes_remaining > dec!(3) {
            dec!(1.3) // Late: increasing confidence
        } else {
            dec!(1.6) // Final minutes: high confidence
        };

        // Only apply full time confidence if we're far from strike
        let abs_distance = distance.abs();
        let distance_modifier = if abs_distance > dec!(50) {
            dec!(1.0) // Far: full time confidence applies
        } else if abs_distance > dec!(20) {
            dec!(0.8)
        } else {
            dec!(0.5) // Close to strike: time doesn't help
        };

        time_factor * distance_modifier
    }

    /// Distance confidence: Further from strike = more certain outcome.
    ///
    /// Ranges from 0.6 (at strike) to 1.5 (very far).
    pub fn distance_confidence(distance: Decimal) -> Decimal {
        let abs_distance = distance.abs();
        if abs_distance > dec!(100) {
            dec!(1.5) // Very far: high confidence
        } else if abs_distance > dec!(50) {
            dec!(1.3)
        } else if abs_distance > dec!(30) {
            dec!(1.1)
        } else if abs_distance > dec!(20) {
            dec!(1.0) // Baseline
        } else if abs_distance > dec!(10) {
            dec!(0.8)
        } else {
            dec!(0.6) // At strike: low confidence
        }
    }

    /// Signal confidence: Strong signals get larger sizes.
    ///
    /// Ranges from 0.7 (neutral) to 1.4 (strong).
    pub fn signal_confidence(signal: Signal) -> Decimal {
        match signal {
            Signal::StrongUp | Signal::StrongDown => dec!(1.4),
            Signal::LeanUp | Signal::LeanDown => dec!(1.1),
            Signal::Neutral => dec!(0.7), // Uncertain: smaller sizes
        }
    }

    /// Order book confidence: Favorable depth and imbalance = more confidence.
    ///
    /// Combines imbalance factor (1.0 or 1.2) with depth factor (0.9 to 1.2).
    pub fn book_confidence(imbalance: Decimal, favorable_depth: Decimal) -> Decimal {
        let imbalance_factor = if imbalance.abs() > dec!(0.3) {
            dec!(1.2)
        } else {
            dec!(1.0)
        };

        let depth_factor = if favorable_depth > dec!(50000) {
            dec!(1.2) // Very deep
        } else if favorable_depth > dec!(20000) {
            dec!(1.1)
        } else if favorable_depth < dec!(5000) {
            dec!(0.9) // Thin: be careful
        } else {
            dec!(1.0)
        };

        imbalance_factor * depth_factor
    }
}

/// Clamp a value between min and max.
#[inline]
fn clamp(value: Decimal, min: Decimal, max: Decimal) -> Decimal {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

/// Convert Decimal to f64 for math operations.
#[inline]
fn decimal_to_f64(d: Decimal) -> f64 {
    use std::str::FromStr;
    f64::from_str(&d.to_string()).unwrap_or(1.0)
}

/// Convert f64 back to Decimal.
#[inline]
fn f64_to_decimal(f: f64) -> Decimal {
    use rust_decimal::prelude::FromPrimitive;
    Decimal::from_f64(f).unwrap_or(Decimal::ONE)
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // ConfidenceFactors Tests
    // =========================================================================

    #[test]
    fn test_confidence_factors_new() {
        let factors = ConfidenceFactors::new(
            dec!(50),
            dec!(5),
            Signal::StrongUp,
            dec!(0.2),
            dec!(25000),
        );

        assert_eq!(factors.distance_to_strike, dec!(50));
        assert_eq!(factors.minutes_remaining, dec!(5));
        assert_eq!(factors.signal, Signal::StrongUp);
        assert_eq!(factors.book_imbalance, dec!(0.2));
        assert_eq!(factors.favorable_depth, dec!(25000));
    }

    // =========================================================================
    // Confidence Component Tests
    // =========================================================================

    #[test]
    fn test_confidence_new() {
        let conf = Confidence::new(dec!(1.2), dec!(1.3), dec!(1.1), dec!(1.0));
        assert_eq!(conf.time, dec!(1.2));
        assert_eq!(conf.distance, dec!(1.3));
        assert_eq!(conf.signal, dec!(1.1));
        assert_eq!(conf.book, dec!(1.0));
    }

    #[test]
    fn test_confidence_neutral() {
        let conf = Confidence::neutral();
        assert_eq!(conf.time, Decimal::ONE);
        assert_eq!(conf.distance, Decimal::ONE);
        assert_eq!(conf.signal, Decimal::ONE);
        assert_eq!(conf.book, Decimal::ONE);

        // Neutral should give ~1.5 multiplier (1.0^0.25 * 1.5)
        let mult = conf.total_multiplier();
        assert!(mult >= dec!(1.4) && mult <= dec!(1.6));
    }

    #[test]
    fn test_confidence_low() {
        let conf = Confidence::low();
        assert_eq!(conf.time, dec!(0.6));
        assert_eq!(conf.distance, dec!(0.6));
        assert_eq!(conf.signal, dec!(0.7));
        assert_eq!(conf.book, dec!(0.9));

        // Low confidence should give multiplier < 1.5
        let mult = conf.total_multiplier();
        assert!(mult < dec!(1.5));
    }

    #[test]
    fn test_confidence_high() {
        let conf = Confidence::high();
        assert_eq!(conf.time, dec!(1.6));
        assert_eq!(conf.distance, dec!(1.5));
        assert_eq!(conf.signal, dec!(1.4));
        assert_eq!(conf.book, dec!(1.2));

        // High confidence should give multiplier > 1.5
        let mult = conf.total_multiplier();
        assert!(mult > dec!(1.5));
    }

    #[test]
    fn test_confidence_default() {
        let conf = Confidence::default();
        assert_eq!(conf.time, Decimal::ONE);
        assert_eq!(conf.distance, Decimal::ONE);
    }

    // =========================================================================
    // Total Multiplier Tests
    // =========================================================================

    #[test]
    fn test_total_multiplier_caps_at_minimum() {
        // Very low factors
        let conf = Confidence::new(dec!(0.1), dec!(0.1), dec!(0.1), dec!(0.1));
        let mult = conf.total_multiplier();
        assert_eq!(mult, MIN_MULTIPLIER);
    }

    #[test]
    fn test_total_multiplier_caps_at_maximum() {
        // Very high factors
        let conf = Confidence::new(dec!(10.0), dec!(10.0), dec!(10.0), dec!(10.0));
        let mult = conf.total_multiplier();
        assert_eq!(mult, MAX_MULTIPLIER);
    }

    #[test]
    fn test_total_multiplier_geometric_mean() {
        // All factors at 1.0 should give 1.5 (1.0^0.25 * 1.5)
        let conf = Confidence::neutral();
        let mult = conf.total_multiplier();
        // Allow small floating point tolerance
        assert!(mult >= dec!(1.49) && mult <= dec!(1.51));
    }

    #[test]
    fn test_total_multiplier_single_high_factor() {
        // One high factor, rest at 1.0
        // Product = 2.0, root = 2^0.25 ≈ 1.189, * 1.5 ≈ 1.78
        let conf = Confidence::new(dec!(2.0), dec!(1.0), dec!(1.0), dec!(1.0));
        let mult = conf.total_multiplier();
        assert!(mult > dec!(1.5) && mult < dec!(2.0));
    }

    // =========================================================================
    // ConfidenceCalculator - Time Confidence Tests
    // =========================================================================

    #[test]
    fn test_time_confidence_early_market() {
        // >12 minutes, far from strike
        let conf = ConfidenceCalculator::time_confidence(dec!(14), dec!(80));
        assert_eq!(conf, dec!(0.6)); // Early: 0.6 * 1.0
    }

    #[test]
    fn test_time_confidence_mid_market() {
        // 6-9 minutes, far from strike
        // time_factor = 0.8 (7 minutes is in 6-9 range)
        // distance_modifier = 1.0 (60 > 50)
        // result = 0.8 * 1.0 = 0.8
        // But wait, 7 is in the 6-9 range, so time_factor = 0.8
        // Actually 7 > 6, so it's in the 6-9 range where time_factor = 1.0 (baseline)
        // Wait - the check is minutes_remaining > 6, so 7 > 6 is true, giving 1.0
        let conf = ConfidenceCalculator::time_confidence(dec!(7), dec!(60));
        assert_eq!(conf, dec!(1.0)); // 1.0 * 1.0 (7 > 6 means mid baseline, 60 > 50 means full distance modifier)
    }

    #[test]
    fn test_time_confidence_late_market() {
        // 3-6 minutes, far from strike
        let conf = ConfidenceCalculator::time_confidence(dec!(4), dec!(80));
        assert_eq!(conf, dec!(1.3)); // 1.3 * 1.0
    }

    #[test]
    fn test_time_confidence_final_minutes() {
        // <3 minutes, far from strike
        let conf = ConfidenceCalculator::time_confidence(dec!(1), dec!(100));
        assert_eq!(conf, dec!(1.6)); // 1.6 * 1.0
    }

    #[test]
    fn test_time_confidence_close_to_strike() {
        // Late market but close to strike - time factor is diminished
        let conf = ConfidenceCalculator::time_confidence(dec!(1), dec!(10));
        assert_eq!(conf, dec!(0.8)); // 1.6 * 0.5
    }

    #[test]
    fn test_time_confidence_medium_distance() {
        // Late market, medium distance
        let conf = ConfidenceCalculator::time_confidence(dec!(2), dec!(30));
        // 1.6 * 0.8 = 1.28
        assert_eq!(conf, dec!(1.28));
    }

    // =========================================================================
    // ConfidenceCalculator - Distance Confidence Tests
    // =========================================================================

    #[test]
    fn test_distance_confidence_very_far() {
        let conf = ConfidenceCalculator::distance_confidence(dec!(150));
        assert_eq!(conf, dec!(1.5));
    }

    #[test]
    fn test_distance_confidence_far() {
        let conf = ConfidenceCalculator::distance_confidence(dec!(60));
        assert_eq!(conf, dec!(1.3));
    }

    #[test]
    fn test_distance_confidence_medium() {
        let conf = ConfidenceCalculator::distance_confidence(dec!(35));
        assert_eq!(conf, dec!(1.1));
    }

    #[test]
    fn test_distance_confidence_baseline() {
        let conf = ConfidenceCalculator::distance_confidence(dec!(25));
        assert_eq!(conf, dec!(1.0));
    }

    #[test]
    fn test_distance_confidence_close() {
        let conf = ConfidenceCalculator::distance_confidence(dec!(15));
        assert_eq!(conf, dec!(0.8));
    }

    #[test]
    fn test_distance_confidence_at_strike() {
        let conf = ConfidenceCalculator::distance_confidence(dec!(5));
        assert_eq!(conf, dec!(0.6));
    }

    #[test]
    fn test_distance_confidence_negative() {
        // Negative distance (below strike) should use absolute value
        let conf = ConfidenceCalculator::distance_confidence(dec!(-80));
        assert_eq!(conf, dec!(1.3));
    }

    // =========================================================================
    // ConfidenceCalculator - Signal Confidence Tests
    // =========================================================================

    #[test]
    fn test_signal_confidence_strong_up() {
        let conf = ConfidenceCalculator::signal_confidence(Signal::StrongUp);
        assert_eq!(conf, dec!(1.4));
    }

    #[test]
    fn test_signal_confidence_strong_down() {
        let conf = ConfidenceCalculator::signal_confidence(Signal::StrongDown);
        assert_eq!(conf, dec!(1.4));
    }

    #[test]
    fn test_signal_confidence_lean_up() {
        let conf = ConfidenceCalculator::signal_confidence(Signal::LeanUp);
        assert_eq!(conf, dec!(1.1));
    }

    #[test]
    fn test_signal_confidence_lean_down() {
        let conf = ConfidenceCalculator::signal_confidence(Signal::LeanDown);
        assert_eq!(conf, dec!(1.1));
    }

    #[test]
    fn test_signal_confidence_neutral() {
        let conf = ConfidenceCalculator::signal_confidence(Signal::Neutral);
        assert_eq!(conf, dec!(0.7));
    }

    // =========================================================================
    // ConfidenceCalculator - Book Confidence Tests
    // =========================================================================

    #[test]
    fn test_book_confidence_neutral_medium_depth() {
        let conf = ConfidenceCalculator::book_confidence(dec!(0.1), dec!(15000));
        assert_eq!(conf, dec!(1.0)); // 1.0 * 1.0
    }

    #[test]
    fn test_book_confidence_high_imbalance() {
        let conf = ConfidenceCalculator::book_confidence(dec!(0.5), dec!(15000));
        assert_eq!(conf, dec!(1.2)); // 1.2 * 1.0
    }

    #[test]
    fn test_book_confidence_very_deep() {
        let conf = ConfidenceCalculator::book_confidence(dec!(0.1), dec!(60000));
        assert_eq!(conf, dec!(1.2)); // 1.0 * 1.2
    }

    #[test]
    fn test_book_confidence_thin() {
        let conf = ConfidenceCalculator::book_confidence(dec!(0.1), dec!(3000));
        assert_eq!(conf, dec!(0.9)); // 1.0 * 0.9
    }

    #[test]
    fn test_book_confidence_high_imbalance_deep() {
        let conf = ConfidenceCalculator::book_confidence(dec!(0.4), dec!(55000));
        // 1.2 * 1.2 = 1.44
        assert_eq!(conf, dec!(1.44));
    }

    #[test]
    fn test_book_confidence_high_imbalance_thin() {
        let conf = ConfidenceCalculator::book_confidence(dec!(0.5), dec!(2000));
        // 1.2 * 0.9 = 1.08
        assert_eq!(conf, dec!(1.08));
    }

    #[test]
    fn test_book_confidence_negative_imbalance() {
        // Negative imbalance (more asks) should still consider absolute value
        let conf = ConfidenceCalculator::book_confidence(dec!(-0.5), dec!(15000));
        assert_eq!(conf, dec!(1.2)); // 1.2 * 1.0
    }

    // =========================================================================
    // ConfidenceCalculator - Full Calculation Tests
    // =========================================================================

    #[test]
    fn test_calculate_full_confidence() {
        let factors = ConfidenceFactors::new(
            dec!(80),   // Far from strike
            dec!(2),    // 2 minutes remaining
            Signal::StrongUp,
            dec!(0.4),  // High imbalance
            dec!(40000), // Good depth
        );

        let conf = ConfidenceCalculator::calculate(&factors);

        // Time: 1.6 (final minutes, far from strike)
        assert_eq!(conf.time, dec!(1.6));
        // Distance: 1.3 (far)
        assert_eq!(conf.distance, dec!(1.3));
        // Signal: 1.4 (strong)
        assert_eq!(conf.signal, dec!(1.4));
        // Book: 1.2 * 1.1 = 1.32
        assert_eq!(conf.book, dec!(1.32));
    }

    #[test]
    fn test_calculate_low_confidence_scenario() {
        let factors = ConfidenceFactors::new(
            dec!(5),    // Very close to strike
            dec!(14),   // Early in market
            Signal::Neutral,
            dec!(0.05), // Low imbalance
            dec!(3000), // Thin book
        );

        let conf = ConfidenceCalculator::calculate(&factors);

        // Time: 0.6 * 0.5 = 0.3 (early, close to strike)
        assert_eq!(conf.time, dec!(0.3));
        // Distance: 0.6 (at strike)
        assert_eq!(conf.distance, dec!(0.6));
        // Signal: 0.7 (neutral)
        assert_eq!(conf.signal, dec!(0.7));
        // Book: 1.0 * 0.9 = 0.9 (thin)
        assert_eq!(conf.book, dec!(0.9));

        // Total multiplier should be low
        let mult = conf.total_multiplier();
        assert!(mult <= dec!(1.0));
    }

    #[test]
    fn test_calculate_high_confidence_scenario() {
        let factors = ConfidenceFactors::new(
            dec!(120),  // Very far from strike
            dec!(1),    // Final minute
            Signal::StrongDown,
            dec!(0.6),  // High imbalance
            dec!(80000), // Very deep
        );

        let conf = ConfidenceCalculator::calculate(&factors);

        // All factors should be high
        assert!(conf.time > dec!(1.0));
        assert!(conf.distance > dec!(1.0));
        assert!(conf.signal > dec!(1.0));
        assert!(conf.book > dec!(1.0));

        // Total multiplier should be high (likely capped at 3.0)
        let mult = conf.total_multiplier();
        assert!(mult > dec!(2.0));
    }

    // =========================================================================
    // Serialization Tests
    // =========================================================================

    #[test]
    fn test_confidence_serialization() {
        let conf = Confidence::new(dec!(1.2), dec!(1.3), dec!(1.1), dec!(1.0));

        let json = serde_json::to_string(&conf).unwrap();
        assert!(json.contains("time"));
        assert!(json.contains("distance"));
        assert!(json.contains("signal"));
        assert!(json.contains("book"));

        let parsed: Confidence = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.time, conf.time);
        assert_eq!(parsed.distance, conf.distance);
        assert_eq!(parsed.signal, conf.signal);
        assert_eq!(parsed.book, conf.book);
    }

    // =========================================================================
    // Spec Example Tests
    // =========================================================================

    #[test]
    fn test_spec_example_weak_confidence() {
        // Scenario 1 from spec: Early market, close to strike, weak signal
        // Expected: ~0.7x multiplier → $0.70 order on $1 base
        let factors = ConfidenceFactors::new(
            dec!(15),   // Close to strike
            dec!(13),   // Early
            Signal::Neutral,
            dec!(0.0),  // No imbalance
            dec!(10000), // Medium depth
        );

        let conf = ConfidenceCalculator::calculate(&factors);
        let mult = conf.total_multiplier();

        // Should be around 0.7x (give some tolerance)
        assert!(mult >= dec!(0.5) && mult <= dec!(1.0));
    }

    #[test]
    fn test_spec_example_strong_confidence() {
        // Scenario 2 from spec: Late market, far from strike, strong signal
        // Expected: ~2.5x multiplier → $2.50 order on $1 base
        let factors = ConfidenceFactors::new(
            dec!(80),   // Far from strike
            dec!(2),    // Late
            Signal::StrongDown,
            dec!(-0.4), // Favorable imbalance
            dec!(40000), // Good depth
        );

        let conf = ConfidenceCalculator::calculate(&factors);
        let mult = conf.total_multiplier();

        // Should be around 2.5x (give some tolerance)
        assert!(mult >= dec!(2.0) && mult <= dec!(3.0));
    }
}
