//! Directional signal detection for binary options markets.
//!
//! This module implements signal-based directional trading:
//! - Calculates distance from spot price to strike
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
//! As time remaining decreases, smaller price differences become significant:
//! - >10 min: Wide thresholds (less certainty)
//! - 5-10 min: Medium thresholds
//! - 2-5 min: Tighter thresholds
//! - 1-2 min: Tight thresholds
//! - <1 min: Very tight thresholds (high certainty)

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
    #[inline]
    pub fn up_ratio(&self) -> Decimal {
        match self {
            Signal::StrongUp => dec!(0.78),   // 78% UP, 22% DOWN
            Signal::LeanUp => dec!(0.60),     // 60% UP, 40% DOWN
            Signal::Neutral => dec!(0.50),    // 50/50 split
            Signal::LeanDown => dec!(0.40),   // 40% UP, 60% DOWN
            Signal::StrongDown => dec!(0.22), // 22% UP, 78% DOWN
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
/// Time brackets and thresholds (distance as percentage of strike):
/// - >10 min: Strong=0.20%, Lean=0.08% (wide, less certainty)
/// - 5-10 min: Strong=0.15%, Lean=0.05%
/// - 2-5 min: Strong=0.10%, Lean=0.03%
/// - 1-2 min: Strong=0.05%, Lean=0.02%
/// - <1 min: Strong=0.03%, Lean=0.01% (tight, high certainty)
#[inline]
pub fn get_thresholds(minutes_remaining: Decimal) -> SignalThresholds {
    if minutes_remaining > dec!(10) {
        // Early: >10 min - wide thresholds
        SignalThresholds::new(dec!(0.0020), dec!(0.0008))
    } else if minutes_remaining > dec!(5) {
        // Mid-early: 5-10 min
        SignalThresholds::new(dec!(0.0015), dec!(0.0005))
    } else if minutes_remaining > dec!(2) {
        // Mid: 2-5 min
        SignalThresholds::new(dec!(0.0010), dec!(0.0003))
    } else if minutes_remaining > dec!(1) {
        // Late: 1-2 min
        SignalThresholds::new(dec!(0.0005), dec!(0.0002))
    } else {
        // Very late: <1 min - tight thresholds
        SignalThresholds::new(dec!(0.0003), dec!(0.0001))
    }
}

/// Calculate the directional signal based on spot price vs strike.
///
/// # Arguments
///
/// * `spot_price` - Current spot price (e.g., BTC price)
/// * `strike_price` - Strike price for the market
/// * `minutes_remaining` - Minutes remaining until settlement
///
/// # Returns
///
/// The detected `Signal` based on price distance and time-adjusted thresholds.
///
/// # Example
///
/// ```ignore
/// use rust_decimal_macros::dec;
/// use poly_bot::strategy::signal::get_signal;
///
/// let signal = get_signal(dec!(100500), dec!(100000), dec!(3));
/// // With 0.5% distance and 3 minutes remaining -> likely StrongUp
/// ```
#[inline]
pub fn get_signal(spot_price: Decimal, strike_price: Decimal, minutes_remaining: Decimal) -> Signal {
    // Calculate distance as a ratio of strike price
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
        assert_eq!(Signal::StrongUp.up_ratio(), dec!(0.78));
        assert_eq!(Signal::LeanUp.up_ratio(), dec!(0.60));
        assert_eq!(Signal::Neutral.up_ratio(), dec!(0.50));
        assert_eq!(Signal::LeanDown.up_ratio(), dec!(0.40));
        assert_eq!(Signal::StrongDown.up_ratio(), dec!(0.22));
    }

    #[test]
    fn test_signal_down_ratio() {
        assert_eq!(Signal::StrongUp.down_ratio(), dec!(0.22));
        assert_eq!(Signal::LeanUp.down_ratio(), dec!(0.40));
        assert_eq!(Signal::Neutral.down_ratio(), dec!(0.50));
        assert_eq!(Signal::LeanDown.down_ratio(), dec!(0.60));
        assert_eq!(Signal::StrongDown.down_ratio(), dec!(0.78));
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
        assert_eq!(Signal::StrongUp.bias_strength(), dec!(0.28));
        assert_eq!(Signal::LeanUp.bias_strength(), dec!(0.10));
        assert_eq!(Signal::Neutral.bias_strength(), dec!(0.00));
        assert_eq!(Signal::LeanDown.bias_strength(), dec!(0.10));
        assert_eq!(Signal::StrongDown.bias_strength(), dec!(0.28));
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
    // Threshold Tests (5 Time Brackets)
    // =========================================================================

    #[test]
    fn test_thresholds_early_bracket() {
        // >10 minutes
        let thresholds = get_thresholds(dec!(12));
        assert_eq!(thresholds.strong, dec!(0.0020));
        assert_eq!(thresholds.lean, dec!(0.0008));
    }

    #[test]
    fn test_thresholds_mid_early_bracket() {
        // 5-10 minutes
        let thresholds = get_thresholds(dec!(7));
        assert_eq!(thresholds.strong, dec!(0.0015));
        assert_eq!(thresholds.lean, dec!(0.0005));
    }

    #[test]
    fn test_thresholds_mid_bracket() {
        // 2-5 minutes
        let thresholds = get_thresholds(dec!(3));
        assert_eq!(thresholds.strong, dec!(0.0010));
        assert_eq!(thresholds.lean, dec!(0.0003));
    }

    #[test]
    fn test_thresholds_late_bracket() {
        // 1-2 minutes
        let thresholds = get_thresholds(dec!(1.5));
        assert_eq!(thresholds.strong, dec!(0.0005));
        assert_eq!(thresholds.lean, dec!(0.0002));
    }

    #[test]
    fn test_thresholds_very_late_bracket() {
        // <1 minute
        let thresholds = get_thresholds(dec!(0.5));
        assert_eq!(thresholds.strong, dec!(0.0003));
        assert_eq!(thresholds.lean, dec!(0.0001));
    }

    #[test]
    fn test_thresholds_boundary_values() {
        // At exactly 10 min -> mid-early bracket
        let at_10 = get_thresholds(dec!(10));
        assert_eq!(at_10.strong, dec!(0.0015));

        // At exactly 5 min -> mid bracket
        let at_5 = get_thresholds(dec!(5));
        assert_eq!(at_5.strong, dec!(0.0010));

        // At exactly 2 min -> late bracket
        let at_2 = get_thresholds(dec!(2));
        assert_eq!(at_2.strong, dec!(0.0005));

        // At exactly 1 min -> very late bracket
        let at_1 = get_thresholds(dec!(1));
        assert_eq!(at_1.strong, dec!(0.0003));
    }

    // =========================================================================
    // Signal Detection Tests
    // =========================================================================

    #[test]
    fn test_get_signal_neutral_equal_prices() {
        let signal = get_signal(dec!(100000), dec!(100000), dec!(5));
        assert_eq!(signal, Signal::Neutral);
    }

    #[test]
    fn test_get_signal_strong_up() {
        // 0.25% above strike with 5 minutes remaining
        // Thresholds at 5 min: strong=0.15%, lean=0.05%
        let spot = dec!(100250);
        let strike = dec!(100000);
        let signal = get_signal(spot, strike, dec!(5));
        assert_eq!(signal, Signal::StrongUp);
    }

    #[test]
    fn test_get_signal_lean_up() {
        // 0.08% above strike with 5 minutes remaining
        // Thresholds at 5 min: strong=0.15%, lean=0.05%
        let spot = dec!(100080);
        let strike = dec!(100000);
        let signal = get_signal(spot, strike, dec!(5));
        assert_eq!(signal, Signal::LeanUp);
    }

    #[test]
    fn test_get_signal_strong_down() {
        // 0.25% below strike with 5 minutes remaining
        let spot = dec!(99750);
        let strike = dec!(100000);
        let signal = get_signal(spot, strike, dec!(5));
        assert_eq!(signal, Signal::StrongDown);
    }

    #[test]
    fn test_get_signal_lean_down() {
        // 0.08% below strike with 5 minutes remaining
        let spot = dec!(99920);
        let strike = dec!(100000);
        let signal = get_signal(spot, strike, dec!(5));
        assert_eq!(signal, Signal::LeanDown);
    }

    #[test]
    fn test_get_signal_neutral_within_lean_threshold() {
        // 0.02% above strike with 5 minutes remaining
        // Thresholds at 5 min: strong=0.15%, lean=0.05%
        // 0.02% < 0.05% -> Neutral
        let spot = dec!(100020);
        let strike = dec!(100000);
        let signal = get_signal(spot, strike, dec!(5));
        assert_eq!(signal, Signal::Neutral);
    }

    #[test]
    fn test_get_signal_time_sensitivity() {
        // Same 0.05% distance, different time remaining
        let spot = dec!(100050);
        let strike = dec!(100000);

        // At 12 min: strong=0.20%, lean=0.08%, 0.05% -> Neutral
        let signal_early = get_signal(spot, strike, dec!(12));
        assert_eq!(signal_early, Signal::Neutral);

        // At 3 min: strong=0.10%, lean=0.03%, 0.05% -> LeanUp
        let signal_mid = get_signal(spot, strike, dec!(3));
        assert_eq!(signal_mid, Signal::LeanUp);

        // At 0.5 min: strong=0.03%, lean=0.01%, 0.05% -> StrongUp
        let signal_late = get_signal(spot, strike, dec!(0.5));
        assert_eq!(signal_late, Signal::StrongUp);
    }

    #[test]
    fn test_get_signal_zero_strike_price() {
        // Edge case: zero strike should return Neutral
        let signal = get_signal(dec!(100000), dec!(0), dec!(5));
        assert_eq!(signal, Signal::Neutral);
    }

    #[test]
    fn test_get_signal_zero_minutes() {
        // At 0 minutes, use very late thresholds
        let spot = dec!(100020);
        let strike = dec!(100000);
        let signal = get_signal(spot, strike, dec!(0));
        // 0.02% distance, very late threshold: strong=0.03%, lean=0.01%
        // 0.02% >= 0.01% lean -> LeanUp
        assert_eq!(signal, Signal::LeanUp);
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
