//! Directional signal detection for binary options markets.
//!
//! This module implements signal-based directional trading:
//! - Calculates distance from spot price to strike as a percentage
//! - Uses time-based threshold selection for signal strength
//! - Returns allocation ratios for UP/DOWN positions
//!
//! ## Signal Strategy
//!
//! When spot price diverges from strike, a directional opportunity exists.
//! Signal strength determines the UP/DOWN allocation ratio:
//!
//! - StrongUp: Spot significantly above strike -> favor UP heavily
//! - LeanUp: Spot moderately above strike -> favor UP slightly
//! - Neutral: Spot near strike -> equal allocation
//! - LeanDown: Spot moderately below strike -> favor DOWN slightly
//! - StrongDown: Spot significantly below strike -> favor DOWN heavily
//!
//! ## Time-Based Thresholds
//!
//! Thresholds use **percentage distance** from strike price,
//! making them asset-agnostic (works for BTC, ETH, SOL, etc.).
//! As time remaining decreases, smaller price differences become significant:
//! - >12 min: 0.030% lean / 0.060% strong (wide, less certainty)
//! - 9-12 min: 0.025% lean / 0.050% strong
//! - 6-9 min: 0.015% lean / 0.040% strong
//! - 3-6 min: 0.012% lean / 0.035% strong
//! - <3 min: 0.008% lean / 0.025% strong (tight, high certainty)

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// Directional signal based on spot price vs strike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Signal {
    /// Spot significantly above strike - strong UP bias.
    StrongUp,
    /// Spot moderately above strike - slight UP bias.
    LeanUp,
    /// Spot near strike - no directional bias.
    Neutral,
    /// Spot moderately below strike - slight DOWN bias.
    LeanDown,
    /// Spot significantly below strike - strong DOWN bias.
    StrongDown,
}

impl Signal {
    /// Get the UP allocation ratio (0.0 to 1.0).
    ///
    /// Returns the fraction of the position that should be allocated to UP.
    /// OPTIMIZED via backtest: Strong 0.90, Lean 0.55
    #[inline]
    pub fn up_ratio(&self) -> Decimal {
        match self {
            Signal::StrongUp => dec!(0.90),   // 90% UP, 10% DOWN (optimized)
            Signal::LeanUp => dec!(0.55),     // 55% UP, 45% DOWN (conservative due to lower win rate)
            Signal::Neutral => dec!(0.50),    // 50/50 split
            Signal::LeanDown => dec!(0.45),   // 45% UP, 55% DOWN
            Signal::StrongDown => dec!(0.10), // 10% UP, 90% DOWN (optimized)
        }
    }

    /// Get the DOWN allocation ratio (0.0 to 1.0).
    ///
    /// Returns the fraction of the position that should be allocated to DOWN.
    #[inline]
    pub fn down_ratio(&self) -> Decimal {
        Decimal::ONE - self.up_ratio()
    }

    /// Check if this signal indicates any directional bias.
    #[inline]
    pub fn is_directional(&self) -> bool {
        !matches!(self, Signal::Neutral)
    }

    /// Check if this is a strong signal (StrongUp or StrongDown).
    #[inline]
    pub fn is_strong(&self) -> bool {
        matches!(self, Signal::StrongUp | Signal::StrongDown)
    }

    /// Get the absolute bias strength (0.0 to 0.28).
    ///
    /// Returns how far from 50/50 this signal tilts.
    #[inline]
    pub fn bias_strength(&self) -> Decimal {
        (self.up_ratio() - dec!(0.50)).abs()
    }
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Signal::StrongUp => write!(f, "STRONG_UP"),
            Signal::LeanUp => write!(f, "LEAN_UP"),
            Signal::Neutral => write!(f, "NEUTRAL"),
            Signal::LeanDown => write!(f, "LEAN_DOWN"),
            Signal::StrongDown => write!(f, "STRONG_DOWN"),
        }
    }
}

/// Thresholds for signal detection at a given time bracket.
#[derive(Debug, Clone, Copy)]
pub struct SignalThresholds {
    /// Threshold for strong signal (distance >= this triggers Strong).
    pub strong: Decimal,
    /// Threshold for lean signal (distance >= this triggers Lean).
    pub lean: Decimal,
}

impl SignalThresholds {
    /// Create new thresholds.
    pub const fn new(strong: Decimal, lean: Decimal) -> Self {
        Self { strong, lean }
    }
}

/// Get the signal thresholds based on minutes remaining.
///
/// OPTIMIZED via Python backtest (14 days, 180 1-hour markets):
/// - Best config: Strong 0.03%, Strong ratio 0.90 -> 5.99% return, 61% win rate
/// - Lean signals are marginal (39-48% win rate) -> higher thresholds to reduce trades
///
/// Time brackets and thresholds (percentage distance from strike):
/// - >30 min: Strong=0.030%, Lean=0.020% (base thresholds, factor 1.0)
/// - 10-30 min: Strong=0.021%, Lean=0.014% (factor 0.7)
/// - 3-10 min: Strong=0.015%, Lean=0.010% (factor 0.5)
/// - <3 min: Strong=0.009%, Lean=0.006% (factor 0.3, high certainty required)
///
/// These percentage-based thresholds work across all assets (BTC, ETH, SOL, etc.)
/// and have been calibrated via backtesting.
#[inline]
pub fn get_thresholds(minutes_remaining: Decimal) -> SignalThresholds {
    if minutes_remaining > dec!(30) {
        // Early: >30 min - base thresholds (optimized via backtest)
        SignalThresholds::new(dec!(0.0003), dec!(0.0002))
    } else if minutes_remaining > dec!(10) {
        // Mid: 10-30 min - factor 0.7
        SignalThresholds::new(dec!(0.00021), dec!(0.00014))
    } else if minutes_remaining > dec!(3) {
        // Late: 3-10 min - factor 0.5
        SignalThresholds::new(dec!(0.00015), dec!(0.0001))
    } else {
        // Very late: <3 min - factor 0.3, tight thresholds
        SignalThresholds::new(dec!(0.00009), dec!(0.00006))
    }
}

/// Calculate the directional signal based on spot price vs strike.
///
/// # Arguments
///
/// * `spot_price` - Current spot price (e.g., BTC, ETH, SOL price)
/// * `strike_price` - Strike price for the market
/// * `minutes_remaining` - Minutes remaining until settlement
///
/// # Returns
///
/// The detected `Signal` based on percentage distance and time-adjusted thresholds.
///
/// # Example
///
/// ```ignore
/// use rust_decimal_macros::dec;
/// use poly_bot::strategy::signal::get_signal;
///
/// let signal = get_signal(dec!(95060), dec!(95000), dec!(5));
/// // With 0.063% distance and 5 minutes remaining -> StrongUp (threshold is 0.035%)
/// ```
#[inline]
pub fn get_signal(spot_price: Decimal, strike_price: Decimal, minutes_remaining: Decimal) -> Signal {
    // Calculate percentage distance from strike
    // distance = (spot - strike) / strike
    let distance = if strike_price.is_zero() {
        Decimal::ZERO
    } else {
        (spot_price - strike_price) / strike_price
    };

    let thresholds = get_thresholds(minutes_remaining);
    let abs_distance = distance.abs();

    if distance > Decimal::ZERO {
        // Spot above strike - UP signals
        if abs_distance >= thresholds.strong {
            Signal::StrongUp
        } else if abs_distance >= thresholds.lean {
            Signal::LeanUp
        } else {
            Signal::Neutral
        }
    } else if distance < Decimal::ZERO {
        // Spot below strike - DOWN signals
        if abs_distance >= thresholds.strong {
            Signal::StrongDown
        } else if abs_distance >= thresholds.lean {
            Signal::LeanDown
        } else {
            Signal::Neutral
        }
    } else {
        Signal::Neutral
    }
}

/// Calculate distance from spot to strike as a percentage.
///
/// Positive values indicate spot > strike (UP territory).
/// Negative values indicate spot < strike (DOWN territory).
#[inline]
pub fn calculate_distance(spot_price: Decimal, strike_price: Decimal) -> Decimal {
    if strike_price.is_zero() {
        Decimal::ZERO
    } else {
        (spot_price - strike_price) / strike_price
    }
}

/// Calculate distance in basis points.
#[inline]
pub fn distance_bps(spot_price: Decimal, strike_price: Decimal) -> i32 {
    let distance = calculate_distance(spot_price, strike_price);
    let bps = distance * dec!(10000);
    bps.trunc().to_string().parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Signal Enum Tests
    // =========================================================================

    #[test]
    fn test_signal_up_ratio() {
        // OPTIMIZED via backtest: Strong 0.90, Lean 0.55
        assert_eq!(Signal::StrongUp.up_ratio(), dec!(0.90));
        assert_eq!(Signal::LeanUp.up_ratio(), dec!(0.55));
        assert_eq!(Signal::Neutral.up_ratio(), dec!(0.50));
        assert_eq!(Signal::LeanDown.up_ratio(), dec!(0.45));
        assert_eq!(Signal::StrongDown.up_ratio(), dec!(0.10));
    }

    #[test]
    fn test_signal_down_ratio() {
        // OPTIMIZED via backtest: Strong 0.90, Lean 0.55
        assert_eq!(Signal::StrongUp.down_ratio(), dec!(0.10));
        assert_eq!(Signal::LeanUp.down_ratio(), dec!(0.45));
        assert_eq!(Signal::Neutral.down_ratio(), dec!(0.50));
        assert_eq!(Signal::LeanDown.down_ratio(), dec!(0.55));
        assert_eq!(Signal::StrongDown.down_ratio(), dec!(0.90));
    }

    #[test]
    fn test_signal_ratios_sum_to_one() {
        for signal in [
            Signal::StrongUp,
            Signal::LeanUp,
            Signal::Neutral,
            Signal::LeanDown,
            Signal::StrongDown,
        ] {
            assert_eq!(
                signal.up_ratio() + signal.down_ratio(),
                Decimal::ONE,
                "Ratios for {:?} should sum to 1.0",
                signal
            );
        }
    }

    #[test]
    fn test_signal_is_directional() {
        assert!(Signal::StrongUp.is_directional());
        assert!(Signal::LeanUp.is_directional());
        assert!(!Signal::Neutral.is_directional());
        assert!(Signal::LeanDown.is_directional());
        assert!(Signal::StrongDown.is_directional());
    }

    #[test]
    fn test_signal_is_strong() {
        assert!(Signal::StrongUp.is_strong());
        assert!(!Signal::LeanUp.is_strong());
        assert!(!Signal::Neutral.is_strong());
        assert!(!Signal::LeanDown.is_strong());
        assert!(Signal::StrongDown.is_strong());
    }

    #[test]
    fn test_signal_bias_strength() {
        // OPTIMIZED: Strong has 0.40 bias (0.90 - 0.50), Lean has 0.05 bias
        assert_eq!(Signal::StrongUp.bias_strength(), dec!(0.40));
        assert_eq!(Signal::LeanUp.bias_strength(), dec!(0.05));
        assert_eq!(Signal::Neutral.bias_strength(), dec!(0.00));
        assert_eq!(Signal::LeanDown.bias_strength(), dec!(0.05));
        assert_eq!(Signal::StrongDown.bias_strength(), dec!(0.40));
    }

    #[test]
    fn test_signal_display() {
        assert_eq!(format!("{}", Signal::StrongUp), "STRONG_UP");
        assert_eq!(format!("{}", Signal::LeanUp), "LEAN_UP");
        assert_eq!(format!("{}", Signal::Neutral), "NEUTRAL");
        assert_eq!(format!("{}", Signal::LeanDown), "LEAN_DOWN");
        assert_eq!(format!("{}", Signal::StrongDown), "STRONG_DOWN");
    }

    // =========================================================================
    // Threshold Tests (4 Time Brackets) - OPTIMIZED via backtest
    // =========================================================================

    #[test]
    fn test_thresholds_early_bracket() {
        // >30 minutes: 0.030% strong, 0.020% lean
        let thresholds = get_thresholds(dec!(35));
        assert_eq!(thresholds.strong, dec!(0.0003));
        assert_eq!(thresholds.lean, dec!(0.0002));
    }

    #[test]
    fn test_thresholds_mid_bracket() {
        // 10-30 minutes: 0.021% strong, 0.014% lean
        let thresholds = get_thresholds(dec!(20));
        assert_eq!(thresholds.strong, dec!(0.00021));
        assert_eq!(thresholds.lean, dec!(0.00014));
    }

    #[test]
    fn test_thresholds_late_bracket() {
        // 3-10 minutes: 0.015% strong, 0.010% lean
        let thresholds = get_thresholds(dec!(5));
        assert_eq!(thresholds.strong, dec!(0.00015));
        assert_eq!(thresholds.lean, dec!(0.0001));
    }

    #[test]
    fn test_thresholds_very_late_bracket() {
        // <3 minutes: 0.009% strong, 0.006% lean
        let thresholds = get_thresholds(dec!(2));
        assert_eq!(thresholds.strong, dec!(0.00009));
        assert_eq!(thresholds.lean, dec!(0.00006));
    }

    #[test]
    fn test_thresholds_boundary_values() {
        // At exactly 30 min -> mid bracket
        let at_30 = get_thresholds(dec!(30));
        assert_eq!(at_30.strong, dec!(0.00021));

        // At exactly 10 min -> late bracket
        let at_10 = get_thresholds(dec!(10));
        assert_eq!(at_10.strong, dec!(0.00015));

        // At exactly 3 min -> very late bracket
        let at_3 = get_thresholds(dec!(3));
        assert_eq!(at_3.strong, dec!(0.00009));
    }

    // =========================================================================
    // Signal Detection Tests - Percentage distance
    // =========================================================================

    #[test]
    fn test_get_signal_neutral_equal_prices() {
        let signal = get_signal(dec!(100000), dec!(100000), dec!(5));
        assert_eq!(signal, Signal::Neutral);
    }

    #[test]
    fn test_get_signal_strong_up() {
        // 0.02% above strike with 5 minutes (3-10 min bracket)
        // Thresholds at 3-10 min: strong=0.015%, lean=0.010%
        // 0.02% > 0.015% -> StrongUp
        let spot = dec!(100020);  // 0.02% above
        let strike = dec!(100000);
        let signal = get_signal(spot, strike, dec!(5));
        assert_eq!(signal, Signal::StrongUp);
    }

    #[test]
    fn test_get_signal_lean_up() {
        // 0.012% above strike with 5 minutes (3-10 min bracket)
        // Thresholds at 3-10 min: strong=0.015%, lean=0.010%
        // 0.010% < 0.012% < 0.015% -> LeanUp
        let spot = dec!(100012);  // 0.012% above
        let strike = dec!(100000);
        let signal = get_signal(spot, strike, dec!(5));
        assert_eq!(signal, Signal::LeanUp);
    }

    #[test]
    fn test_get_signal_strong_down() {
        // 0.02% below strike with 5 minutes
        let spot = dec!(99980);  // 0.02% below
        let strike = dec!(100000);
        let signal = get_signal(spot, strike, dec!(5));
        assert_eq!(signal, Signal::StrongDown);
    }

    #[test]
    fn test_get_signal_lean_down() {
        // 0.012% below strike with 5 minutes
        let spot = dec!(99988);  // 0.012% below
        let strike = dec!(100000);
        let signal = get_signal(spot, strike, dec!(5));
        assert_eq!(signal, Signal::LeanDown);
    }

    #[test]
    fn test_get_signal_neutral_within_lean_threshold() {
        // 0.005% above strike with 5 minutes (3-10 min bracket)
        // Thresholds at 3-10 min: strong=0.015%, lean=0.010%
        // 0.005% < 0.010% -> Neutral
        let spot = dec!(100005);  // 0.005% above
        let strike = dec!(100000);
        let signal = get_signal(spot, strike, dec!(5));
        assert_eq!(signal, Signal::Neutral);
    }

    #[test]
    fn test_get_signal_time_sensitivity() {
        // Same 0.025% distance, different time remaining
        let spot = dec!(100025);  // 0.025% above
        let strike = dec!(100000);

        // At 35 min: strong=0.030%, lean=0.020%, 0.025% -> LeanUp
        let signal_early = get_signal(spot, strike, dec!(35));
        assert_eq!(signal_early, Signal::LeanUp);

        // At 5 min: strong=0.015%, lean=0.010%, 0.025% -> StrongUp
        let signal_mid = get_signal(spot, strike, dec!(5));
        assert_eq!(signal_mid, Signal::StrongUp);

        // At 1 min: strong=0.009%, lean=0.006%, 0.025% -> StrongUp
        let signal_late = get_signal(spot, strike, dec!(1));
        assert_eq!(signal_late, Signal::StrongUp);
    }

    #[test]
    fn test_get_signal_zero_strike_price() {
        // Edge case: zero strike returns Neutral (division by zero protection)
        let signal = get_signal(dec!(100000), dec!(0), dec!(5));
        assert_eq!(signal, Signal::Neutral);
    }

    #[test]
    fn test_get_signal_zero_minutes() {
        // At 0 minutes, use very late thresholds (strong=0.009%, lean=0.006%)
        let spot = dec!(100015);  // 0.015% above
        let strike = dec!(100000);
        let signal = get_signal(spot, strike, dec!(0));
        // 0.015% distance >= 0.009% strong threshold -> StrongUp
        assert_eq!(signal, Signal::StrongUp);
    }

    // =========================================================================
    // Distance Calculation Tests
    // =========================================================================

    #[test]
    fn test_calculate_distance_above() {
        let distance = calculate_distance(dec!(100500), dec!(100000));
        assert_eq!(distance, dec!(0.005)); // 0.5% above
    }

    #[test]
    fn test_calculate_distance_below() {
        let distance = calculate_distance(dec!(99500), dec!(100000));
        assert_eq!(distance, dec!(-0.005)); // 0.5% below
    }

    #[test]
    fn test_calculate_distance_equal() {
        let distance = calculate_distance(dec!(100000), dec!(100000));
        assert_eq!(distance, dec!(0));
    }

    #[test]
    fn test_calculate_distance_zero_strike() {
        let distance = calculate_distance(dec!(100000), dec!(0));
        assert_eq!(distance, dec!(0));
    }

    #[test]
    fn test_distance_bps() {
        assert_eq!(distance_bps(dec!(100500), dec!(100000)), 50); // 0.5% = 50 bps
        assert_eq!(distance_bps(dec!(99500), dec!(100000)), -50); // -0.5% = -50 bps
        assert_eq!(distance_bps(dec!(100000), dec!(100000)), 0);
    }

    // =========================================================================
    // Serialization Tests
    // =========================================================================

    #[test]
    fn test_signal_serialization() {
        let json = serde_json::to_string(&Signal::StrongUp).unwrap();
        assert_eq!(json, "\"StrongUp\"");

        let parsed: Signal = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Signal::StrongUp);
    }

    #[test]
    fn test_signal_all_variants_serialize() {
        for signal in [
            Signal::StrongUp,
            Signal::LeanUp,
            Signal::Neutral,
            Signal::LeanDown,
            Signal::StrongDown,
        ] {
            let json = serde_json::to_string(&signal).unwrap();
            let parsed: Signal = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, signal);
        }
    }
}
