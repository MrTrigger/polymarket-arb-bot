//! Directional trading strategy detector.
//!
//! This module implements directional trading based on price signals:
//! - Uses Signal system to determine market bias
//! - Calculates UP/DOWN allocation ratios based on signal strength
//! - Computes confidence for position sizing
//!
//! ## Strategy Overview
//!
//! When spot price diverges from strike, a directional opportunity exists:
//! - StrongUp signal: Favor UP heavily (78% UP / 22% DOWN)
//! - LeanUp signal: Favor UP slightly (60% UP / 40% DOWN)
//! - Neutral signal: Skip trading (no edge)
//! - LeanDown signal: Favor DOWN slightly (40% UP / 60% DOWN)
//! - StrongDown signal: Favor DOWN heavily (22% UP / 78% DOWN)
//!
//! ## Hedging
//!
//! Unlike pure directional bets, we always buy both sides:
//! - Guaranteed $1.00 payout at settlement
//! - Asymmetric allocation captures directional edge
//! - Reduces variance while maintaining expected value

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

use super::confidence::{Confidence, ConfidenceCalculator, ConfidenceFactors};
use super::signal::{calculate_distance, get_signal, Signal};
use crate::types::{MarketState, OrderBook};

/// Reason for skipping a directional opportunity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectionalSkipReason {
    /// Signal is neutral - no directional edge.
    NeutralSignal,
    /// No spot price available.
    NoSpotPrice,
    /// Strike price is zero or invalid.
    InvalidStrike,
    /// Order book not ready (missing bids/asks).
    BookNotReady,
    /// Insufficient time remaining.
    InsufficientTime,
    /// Spread too wide to profitably trade.
    SpreadTooWide,
}

impl std::fmt::Display for DirectionalSkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeutralSignal => write!(f, "neutral signal"),
            Self::NoSpotPrice => write!(f, "no spot price"),
            Self::InvalidStrike => write!(f, "invalid strike"),
            Self::BookNotReady => write!(f, "book not ready"),
            Self::InsufficientTime => write!(f, "insufficient time"),
            Self::SpreadTooWide => write!(f, "spread too wide"),
        }
    }
}

/// A detected directional trading opportunity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectionalOpportunity {
    /// Event ID for this market.
    pub event_id: String,
    /// YES token ID.
    pub yes_token_id: String,
    /// NO token ID.
    pub no_token_id: String,
    /// Detected signal.
    pub signal: Signal,
    /// UP allocation ratio (0.0 to 1.0).
    pub up_ratio: Decimal,
    /// DOWN allocation ratio (0.0 to 1.0).
    pub down_ratio: Decimal,
    /// Confidence breakdown for sizing.
    pub confidence: Confidence,
    /// Spot price used for detection.
    pub spot_price: Decimal,
    /// Strike price.
    pub strike_price: Decimal,
    /// Distance from spot to strike (as ratio).
    pub distance: Decimal,
    /// Minutes remaining in window.
    pub minutes_remaining: Decimal,
    /// Best ask price for YES token.
    pub yes_ask: Decimal,
    /// Best ask price for NO token.
    pub no_ask: Decimal,
    /// Combined cost to buy both sides.
    pub combined_cost: Decimal,
    /// Order book imbalance for YES (-1 to +1).
    pub yes_imbalance: Decimal,
    /// Order book imbalance for NO (-1 to +1).
    pub no_imbalance: Decimal,
    /// Detection timestamp (milliseconds).
    pub detected_at_ms: i64,
}

impl DirectionalOpportunity {
    /// Get the expected profit per share at settlement.
    ///
    /// Since we buy both sides, payout is always $1.00.
    /// Profit = $1.00 - combined_cost
    #[inline]
    pub fn expected_profit(&self) -> Decimal {
        Decimal::ONE - self.combined_cost
    }

    /// Check if this is a strong signal opportunity.
    #[inline]
    pub fn is_strong(&self) -> bool {
        self.signal.is_strong()
    }

    /// Get the total confidence multiplier for sizing.
    #[inline]
    pub fn confidence_multiplier(&self) -> Decimal {
        self.confidence.total_multiplier()
    }
}

/// Configuration for the directional detector.
#[derive(Debug, Clone)]
pub struct DirectionalConfig {
    /// Minimum time remaining (seconds) to consider directional trades.
    pub min_seconds_remaining: i64,
    /// Maximum combined cost (must be < 1.0 to profit).
    pub max_combined_cost: Decimal,
    /// Maximum spread as ratio of price (e.g., 0.05 = 5%).
    pub max_spread_ratio: Decimal,
    /// Minimum depth required on favorable side (in dollars).
    pub min_favorable_depth: Decimal,
}

impl Default for DirectionalConfig {
    fn default() -> Self {
        Self {
            min_seconds_remaining: 60, // At least 1 minute
            max_combined_cost: dec!(0.995), // At least 0.5% margin
            max_spread_ratio: dec!(0.10), // Max 10% spread
            min_favorable_depth: dec!(100), // At least $100 depth
        }
    }
}

/// Detector for directional trading opportunities.
///
/// Uses the Signal system to identify when spot price diverges from strike,
/// then calculates allocation ratios and confidence for position sizing.
#[derive(Debug, Clone)]
pub struct DirectionalDetector {
    /// Configuration.
    config: DirectionalConfig,
}

impl DirectionalDetector {
    /// Create a new directional detector with default config.
    pub fn new() -> Self {
        Self {
            config: DirectionalConfig::default(),
        }
    }

    /// Create a new directional detector with custom config.
    pub fn with_config(config: DirectionalConfig) -> Self {
        Self { config }
    }

    /// Detect a directional opportunity from market state.
    ///
    /// # Arguments
    ///
    /// * `state` - Current market state with order books and prices
    ///
    /// # Returns
    ///
    /// * `Ok(DirectionalOpportunity)` - Opportunity detected
    /// * `Err(DirectionalSkipReason)` - Why no opportunity exists
    pub fn detect(&self, state: &MarketState) -> Result<DirectionalOpportunity, DirectionalSkipReason> {
        // Validate inputs
        let spot_price = state.spot_price.ok_or(DirectionalSkipReason::NoSpotPrice)?;

        if state.strike_price.is_zero() {
            return Err(DirectionalSkipReason::InvalidStrike);
        }

        // Check time remaining
        if state.seconds_remaining < self.config.min_seconds_remaining {
            return Err(DirectionalSkipReason::InsufficientTime);
        }

        // Get order book prices
        let yes_ask = state.yes_book.best_ask().ok_or(DirectionalSkipReason::BookNotReady)?;
        let no_ask = state.no_book.best_ask().ok_or(DirectionalSkipReason::BookNotReady)?;

        // Check spread on both books
        if let (Some(yes_bid), Some(no_bid)) = (state.yes_book.best_bid(), state.no_book.best_bid()) {
            let yes_spread = (yes_ask - yes_bid) / yes_ask;
            let no_spread = (no_ask - no_bid) / no_ask;

            if yes_spread > self.config.max_spread_ratio || no_spread > self.config.max_spread_ratio {
                return Err(DirectionalSkipReason::SpreadTooWide);
            }
        }

        // Check combined cost
        let combined_cost = yes_ask + no_ask;
        if combined_cost > self.config.max_combined_cost {
            // No profit margin even without directional edge
            // Still allow if there's a strong signal (directional edge compensates)
        }

        // Calculate signal
        let minutes_remaining = Decimal::from(state.seconds_remaining) / dec!(60);
        let signal = get_signal(spot_price, state.strike_price, minutes_remaining);

        // Skip if neutral - no directional edge
        if !signal.is_directional() {
            return Err(DirectionalSkipReason::NeutralSignal);
        }

        // Calculate distance
        let distance = calculate_distance(spot_price, state.strike_price);
        let distance_dollars = (spot_price - state.strike_price).abs();

        // Calculate order book metrics
        let yes_imbalance = calculate_imbalance(&state.yes_book);
        let no_imbalance = calculate_imbalance(&state.no_book);

        // Use the imbalance that favors our direction
        let book_imbalance = match signal {
            Signal::StrongUp | Signal::LeanUp => yes_imbalance,
            Signal::StrongDown | Signal::LeanDown => -no_imbalance, // Negative for DOWN
            Signal::Neutral => Decimal::ZERO,
        };

        // Calculate favorable depth based on signal direction
        let favorable_depth = match signal {
            Signal::StrongUp | Signal::LeanUp => state.yes_book.ask_depth(),
            Signal::StrongDown | Signal::LeanDown => state.no_book.ask_depth(),
            Signal::Neutral => Decimal::ZERO,
        };

        // Build confidence factors
        let factors = ConfidenceFactors::new(
            distance_dollars,
            minutes_remaining,
            signal,
            book_imbalance,
            favorable_depth,
        );

        let confidence = ConfidenceCalculator::calculate(&factors);

        Ok(DirectionalOpportunity {
            event_id: state.event_id.clone(),
            yes_token_id: state.yes_book.token_id.clone(),
            no_token_id: state.no_book.token_id.clone(),
            signal,
            up_ratio: signal.up_ratio(),
            down_ratio: signal.down_ratio(),
            confidence,
            spot_price,
            strike_price: state.strike_price,
            distance,
            minutes_remaining,
            yes_ask,
            no_ask,
            combined_cost,
            yes_imbalance,
            no_imbalance,
            detected_at_ms: chrono::Utc::now().timestamp_millis(),
        })
    }

    /// Quick check if directional opportunity might exist.
    ///
    /// This is a fast filter to avoid full detection when clearly no opportunity.
    #[inline]
    pub fn has_potential(&self, state: &MarketState) -> bool {
        // Must have spot price and books
        if state.spot_price.is_none() {
            return false;
        }
        if state.yes_book.best_ask().is_none() || state.no_book.best_ask().is_none() {
            return false;
        }
        // Must have time remaining
        if state.seconds_remaining < self.config.min_seconds_remaining {
            return false;
        }
        true
    }
}

impl Default for DirectionalDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Calculate order book imbalance.
///
/// Returns value from -1.0 (all asks) to +1.0 (all bids).
fn calculate_imbalance(book: &OrderBook) -> Decimal {
    let bid_depth = book.bid_depth();
    let ask_depth = book.ask_depth();
    let total = bid_depth + ask_depth;

    if total.is_zero() {
        return Decimal::ZERO;
    }

    (bid_depth - ask_depth) / total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PriceLevel;
    use poly_common::types::CryptoAsset;
    use rust_decimal_macros::dec;

    fn create_test_market_state(
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

    fn add_book_levels(state: &mut MarketState, yes_bid: Decimal, yes_ask: Decimal, no_bid: Decimal, no_ask: Decimal) {
        state.yes_book.apply_snapshot(
            vec![PriceLevel::new(yes_bid, dec!(1000))],
            vec![PriceLevel::new(yes_ask, dec!(1000))],
            0,
        );
        state.no_book.apply_snapshot(
            vec![PriceLevel::new(no_bid, dec!(1000))],
            vec![PriceLevel::new(no_ask, dec!(1000))],
            0,
        );
    }

    // =========================================================================
    // DirectionalSkipReason Tests
    // =========================================================================

    #[test]
    fn test_skip_reason_display() {
        assert_eq!(format!("{}", DirectionalSkipReason::NeutralSignal), "neutral signal");
        assert_eq!(format!("{}", DirectionalSkipReason::NoSpotPrice), "no spot price");
        assert_eq!(format!("{}", DirectionalSkipReason::InvalidStrike), "invalid strike");
        assert_eq!(format!("{}", DirectionalSkipReason::BookNotReady), "book not ready");
        assert_eq!(format!("{}", DirectionalSkipReason::InsufficientTime), "insufficient time");
        assert_eq!(format!("{}", DirectionalSkipReason::SpreadTooWide), "spread too wide");
    }

    // =========================================================================
    // DirectionalOpportunity Tests
    // =========================================================================

    #[test]
    fn test_opportunity_expected_profit() {
        let opp = DirectionalOpportunity {
            event_id: "test".to_string(),
            yes_token_id: "yes".to_string(),
            no_token_id: "no".to_string(),
            signal: Signal::StrongUp,
            up_ratio: dec!(0.78),
            down_ratio: dec!(0.22),
            confidence: Confidence::neutral(),
            spot_price: dec!(100500),
            strike_price: dec!(100000),
            distance: dec!(0.005),
            minutes_remaining: dec!(5),
            yes_ask: dec!(0.48),
            no_ask: dec!(0.50),
            combined_cost: dec!(0.98),
            yes_imbalance: dec!(0.1),
            no_imbalance: dec!(-0.1),
            detected_at_ms: 0,
        };

        assert_eq!(opp.expected_profit(), dec!(0.02));
    }

    #[test]
    fn test_opportunity_is_strong() {
        let strong_up = DirectionalOpportunity {
            event_id: "test".to_string(),
            yes_token_id: "yes".to_string(),
            no_token_id: "no".to_string(),
            signal: Signal::StrongUp,
            up_ratio: dec!(0.78),
            down_ratio: dec!(0.22),
            confidence: Confidence::neutral(),
            spot_price: dec!(100500),
            strike_price: dec!(100000),
            distance: dec!(0.005),
            minutes_remaining: dec!(5),
            yes_ask: dec!(0.48),
            no_ask: dec!(0.50),
            combined_cost: dec!(0.98),
            yes_imbalance: dec!(0),
            no_imbalance: dec!(0),
            detected_at_ms: 0,
        };

        assert!(strong_up.is_strong());

        let lean_up = DirectionalOpportunity {
            signal: Signal::LeanUp,
            up_ratio: dec!(0.60),
            down_ratio: dec!(0.40),
            ..strong_up.clone()
        };

        assert!(!lean_up.is_strong());
    }

    // =========================================================================
    // DirectionalConfig Tests
    // =========================================================================

    #[test]
    fn test_config_default() {
        let config = DirectionalConfig::default();
        assert_eq!(config.min_seconds_remaining, 60);
        assert_eq!(config.max_combined_cost, dec!(0.995));
        assert_eq!(config.max_spread_ratio, dec!(0.10));
        assert_eq!(config.min_favorable_depth, dec!(100));
    }

    // =========================================================================
    // DirectionalDetector Tests
    // =========================================================================

    #[test]
    fn test_detector_creation() {
        let detector = DirectionalDetector::new();
        assert_eq!(detector.config.min_seconds_remaining, 60);

        let custom_config = DirectionalConfig {
            min_seconds_remaining: 120,
            ..Default::default()
        };
        let detector2 = DirectionalDetector::with_config(custom_config);
        assert_eq!(detector2.config.min_seconds_remaining, 120);
    }

    #[test]
    fn test_detector_default() {
        let detector = DirectionalDetector::default();
        assert_eq!(detector.config.min_seconds_remaining, 60);
    }

    #[test]
    fn test_detect_no_spot_price() {
        let detector = DirectionalDetector::new();
        let state = create_test_market_state(None, dec!(100000), 300);

        let result = detector.detect(&state);
        assert_eq!(result.unwrap_err(), DirectionalSkipReason::NoSpotPrice);
    }

    #[test]
    fn test_detect_invalid_strike() {
        let detector = DirectionalDetector::new();
        let state = create_test_market_state(Some(dec!(100000)), Decimal::ZERO, 300);

        let result = detector.detect(&state);
        assert_eq!(result.unwrap_err(), DirectionalSkipReason::InvalidStrike);
    }

    #[test]
    fn test_detect_insufficient_time() {
        let detector = DirectionalDetector::new();
        let state = create_test_market_state(Some(dec!(100500)), dec!(100000), 30);

        let result = detector.detect(&state);
        assert_eq!(result.unwrap_err(), DirectionalSkipReason::InsufficientTime);
    }

    #[test]
    fn test_detect_book_not_ready() {
        let detector = DirectionalDetector::new();
        let state = create_test_market_state(Some(dec!(100500)), dec!(100000), 300);
        // No order book data

        let result = detector.detect(&state);
        assert_eq!(result.unwrap_err(), DirectionalSkipReason::BookNotReady);
    }

    #[test]
    fn test_detect_neutral_signal() {
        let detector = DirectionalDetector::new();
        let mut state = create_test_market_state(Some(dec!(100000)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));

        let result = detector.detect(&state);
        assert_eq!(result.unwrap_err(), DirectionalSkipReason::NeutralSignal);
    }

    #[test]
    fn test_detect_spread_too_wide() {
        let detector = DirectionalDetector::new();
        let mut state = create_test_market_state(Some(dec!(100500)), dec!(100000), 300);
        // Wide spread: bid 0.30, ask 0.50 = 40% spread
        state.yes_book.apply_snapshot(
            vec![PriceLevel::new(dec!(0.30), dec!(1000))],
            vec![PriceLevel::new(dec!(0.50), dec!(1000))],
            0,
        );
        state.no_book.apply_snapshot(
            vec![PriceLevel::new(dec!(0.45), dec!(1000))],
            vec![PriceLevel::new(dec!(0.48), dec!(1000))],
            0,
        );

        let result = detector.detect(&state);
        assert_eq!(result.unwrap_err(), DirectionalSkipReason::SpreadTooWide);
    }

    #[test]
    fn test_detect_strong_up_signal() {
        let detector = DirectionalDetector::new();
        // 0.25% above strike at 5 minutes -> StrongUp
        let mut state = create_test_market_state(Some(dec!(100250)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));

        let result = detector.detect(&state);
        assert!(result.is_ok());

        let opp = result.unwrap();
        assert_eq!(opp.signal, Signal::StrongUp);
        assert_eq!(opp.up_ratio, dec!(0.78));
        assert_eq!(opp.down_ratio, dec!(0.22));
    }

    #[test]
    fn test_detect_lean_up_signal() {
        let detector = DirectionalDetector::new();
        // 0.08% above strike at 5 minutes -> LeanUp
        let mut state = create_test_market_state(Some(dec!(100080)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));

        let result = detector.detect(&state);
        assert!(result.is_ok());

        let opp = result.unwrap();
        assert_eq!(opp.signal, Signal::LeanUp);
        assert_eq!(opp.up_ratio, dec!(0.60));
        assert_eq!(opp.down_ratio, dec!(0.40));
    }

    #[test]
    fn test_detect_strong_down_signal() {
        let detector = DirectionalDetector::new();
        // 0.25% below strike at 5 minutes -> StrongDown
        let mut state = create_test_market_state(Some(dec!(99750)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));

        let result = detector.detect(&state);
        assert!(result.is_ok());

        let opp = result.unwrap();
        assert_eq!(opp.signal, Signal::StrongDown);
        assert_eq!(opp.up_ratio, dec!(0.22));
        assert_eq!(opp.down_ratio, dec!(0.78));
    }

    #[test]
    fn test_detect_lean_down_signal() {
        let detector = DirectionalDetector::new();
        // 0.08% below strike at 5 minutes -> LeanDown
        let mut state = create_test_market_state(Some(dec!(99920)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));

        let result = detector.detect(&state);
        assert!(result.is_ok());

        let opp = result.unwrap();
        assert_eq!(opp.signal, Signal::LeanDown);
        assert_eq!(opp.up_ratio, dec!(0.40));
        assert_eq!(opp.down_ratio, dec!(0.60));
    }

    // =========================================================================
    // Signal Ratio Tests (Per Spec)
    // =========================================================================

    #[test]
    fn test_strong_up_returns_78_22_ratio() {
        let detector = DirectionalDetector::new();
        // 0.3% above strike at 5 minutes -> StrongUp
        let mut state = create_test_market_state(Some(dec!(100300)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));

        let opp = detector.detect(&state).unwrap();
        assert_eq!(opp.signal, Signal::StrongUp);
        assert_eq!(opp.up_ratio, dec!(0.78));
        assert_eq!(opp.down_ratio, dec!(0.22));
        assert_eq!(opp.up_ratio + opp.down_ratio, Decimal::ONE);
    }

    #[test]
    fn test_strong_down_returns_22_78_ratio() {
        let detector = DirectionalDetector::new();
        // 0.3% below strike at 5 minutes -> StrongDown
        let mut state = create_test_market_state(Some(dec!(99700)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));

        let opp = detector.detect(&state).unwrap();
        assert_eq!(opp.signal, Signal::StrongDown);
        assert_eq!(opp.up_ratio, dec!(0.22));
        assert_eq!(opp.down_ratio, dec!(0.78));
        assert_eq!(opp.up_ratio + opp.down_ratio, Decimal::ONE);
    }

    // =========================================================================
    // has_potential Tests
    // =========================================================================

    #[test]
    fn test_has_potential_no_spot() {
        let detector = DirectionalDetector::new();
        let state = create_test_market_state(None, dec!(100000), 300);
        assert!(!detector.has_potential(&state));
    }

    #[test]
    fn test_has_potential_no_book() {
        let detector = DirectionalDetector::new();
        let state = create_test_market_state(Some(dec!(100000)), dec!(100000), 300);
        assert!(!detector.has_potential(&state));
    }

    #[test]
    fn test_has_potential_insufficient_time() {
        let detector = DirectionalDetector::new();
        let mut state = create_test_market_state(Some(dec!(100000)), dec!(100000), 30);
        add_book_levels(&mut state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));
        assert!(!detector.has_potential(&state));
    }

    #[test]
    fn test_has_potential_valid() {
        let detector = DirectionalDetector::new();
        let mut state = create_test_market_state(Some(dec!(100000)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));
        assert!(detector.has_potential(&state));
    }

    // =========================================================================
    // Order Book Imbalance Tests
    // =========================================================================

    #[test]
    fn test_calculate_imbalance_equal() {
        let mut book = OrderBook::new("test".to_string());
        book.apply_snapshot(
            vec![PriceLevel::new(dec!(0.45), dec!(100))],
            vec![PriceLevel::new(dec!(0.50), dec!(100))],
            0,
        );
        let imbalance = calculate_imbalance(&book);
        // Bid size = 100, Ask size = 100
        // (100 - 100) / 200 = 0
        assert_eq!(imbalance, Decimal::ZERO);
    }

    #[test]
    fn test_calculate_imbalance_bid_heavy() {
        let mut book = OrderBook::new("test".to_string());
        book.apply_snapshot(
            vec![PriceLevel::new(dec!(0.45), dec!(200))],
            vec![PriceLevel::new(dec!(0.50), dec!(50))],
            0,
        );
        let imbalance = calculate_imbalance(&book);
        // Bid size = 200, Ask size = 50
        // (200 - 50) / 250 = 0.6
        assert!(imbalance > Decimal::ZERO);
    }

    #[test]
    fn test_calculate_imbalance_ask_heavy() {
        let mut book = OrderBook::new("test".to_string());
        book.apply_snapshot(
            vec![PriceLevel::new(dec!(0.45), dec!(50))],
            vec![PriceLevel::new(dec!(0.50), dec!(200))],
            0,
        );
        let imbalance = calculate_imbalance(&book);
        // Bid size = 50, Ask size = 200
        // (50 - 200) / 250 = -0.6
        assert!(imbalance < Decimal::ZERO);
    }

    #[test]
    fn test_calculate_imbalance_empty_book() {
        let book = OrderBook::new("test".to_string());
        let imbalance = calculate_imbalance(&book);
        assert_eq!(imbalance, Decimal::ZERO);
    }

    // =========================================================================
    // Confidence Integration Tests
    // =========================================================================

    #[test]
    fn test_opportunity_has_confidence() {
        let detector = DirectionalDetector::new();
        let mut state = create_test_market_state(Some(dec!(100250)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));

        let opp = detector.detect(&state).unwrap();

        // Should have valid confidence values
        assert!(opp.confidence.time > Decimal::ZERO);
        assert!(opp.confidence.distance > Decimal::ZERO);
        assert!(opp.confidence.signal > Decimal::ZERO);
        assert!(opp.confidence.book > Decimal::ZERO);

        // Multiplier should be in valid range
        let mult = opp.confidence_multiplier();
        assert!(mult >= dec!(0.5));
        assert!(mult <= dec!(3.0));
    }

    #[test]
    fn test_strong_signal_has_higher_confidence() {
        let detector = DirectionalDetector::new();

        // Strong signal (0.3% distance)
        let mut strong_state = create_test_market_state(Some(dec!(100300)), dec!(100000), 300);
        add_book_levels(&mut strong_state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));
        let strong_opp = detector.detect(&strong_state).unwrap();

        // Lean signal (0.08% distance)
        let mut lean_state = create_test_market_state(Some(dec!(100080)), dec!(100000), 300);
        add_book_levels(&mut lean_state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));
        let lean_opp = detector.detect(&lean_state).unwrap();

        // Strong signal should have higher signal confidence
        assert!(strong_opp.confidence.signal > lean_opp.confidence.signal);
    }

    // =========================================================================
    // Serialization Tests
    // =========================================================================

    #[test]
    fn test_opportunity_serialization() {
        let opp = DirectionalOpportunity {
            event_id: "test".to_string(),
            yes_token_id: "yes".to_string(),
            no_token_id: "no".to_string(),
            signal: Signal::StrongUp,
            up_ratio: dec!(0.78),
            down_ratio: dec!(0.22),
            confidence: Confidence::neutral(),
            spot_price: dec!(100500),
            strike_price: dec!(100000),
            distance: dec!(0.005),
            minutes_remaining: dec!(5),
            yes_ask: dec!(0.48),
            no_ask: dec!(0.50),
            combined_cost: dec!(0.98),
            yes_imbalance: dec!(0.1),
            no_imbalance: dec!(-0.1),
            detected_at_ms: 12345,
        };

        let json = serde_json::to_string(&opp).unwrap();
        assert!(json.contains("StrongUp"));
        assert!(json.contains("0.78"));
        assert!(json.contains("test"));

        let parsed: DirectionalOpportunity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.signal, Signal::StrongUp);
        assert_eq!(parsed.up_ratio, dec!(0.78));
    }

    #[test]
    fn test_skip_reason_serialization() {
        let reason = DirectionalSkipReason::NeutralSignal;
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(json, "\"NeutralSignal\"");

        let parsed: DirectionalSkipReason = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, DirectionalSkipReason::NeutralSignal);
    }

    // =========================================================================
    // All Signal Level Tests
    // =========================================================================

    #[test]
    fn test_all_signal_levels() {
        let detector = DirectionalDetector::new();

        // Test each signal level with appropriate distance at 5 minutes
        // Thresholds at 5 min: strong=0.15%, lean=0.05%
        let test_cases = [
            (dec!(100200), Signal::StrongUp),   // 0.2% above -> StrongUp
            (dec!(100070), Signal::LeanUp),     // 0.07% above -> LeanUp
            (dec!(99800), Signal::StrongDown),  // 0.2% below -> StrongDown
            (dec!(99930), Signal::LeanDown),    // 0.07% below -> LeanDown
        ];

        for (spot, expected_signal) in test_cases {
            let mut state = create_test_market_state(Some(spot), dec!(100000), 300);
            add_book_levels(&mut state, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));

            let result = detector.detect(&state);
            assert!(result.is_ok(), "Failed for spot={}", spot);
            let opp = result.unwrap();
            assert_eq!(opp.signal, expected_signal, "Wrong signal for spot={}", spot);
        }
    }

    // =========================================================================
    // Time Bracket Tests
    // =========================================================================

    #[test]
    fn test_signal_at_different_time_brackets() {
        let detector = DirectionalDetector::new();

        // Same 0.05% distance, different time remaining
        let spot = dec!(100050);
        let strike = dec!(100000);

        // At 12 min: strong=0.20%, lean=0.08%, 0.05% -> Neutral (skip)
        let mut state_early = create_test_market_state(Some(spot), strike, 720);
        add_book_levels(&mut state_early, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));
        assert!(detector.detect(&state_early).is_err());

        // At 3 min: strong=0.10%, lean=0.03%, 0.05% -> LeanUp
        let mut state_mid = create_test_market_state(Some(spot), strike, 180);
        add_book_levels(&mut state_mid, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));
        let mid_result = detector.detect(&state_mid);
        assert!(mid_result.is_ok());
        assert_eq!(mid_result.unwrap().signal, Signal::LeanUp);

        // At 0.5 min: strong=0.03%, lean=0.01%, 0.05% -> StrongUp
        // But we have min 60 seconds, so use 61 seconds
        let mut state_late = create_test_market_state(Some(spot), strike, 61);
        add_book_levels(&mut state_late, dec!(0.45), dec!(0.48), dec!(0.45), dec!(0.48));
        let late_result = detector.detect(&state_late);
        assert!(late_result.is_ok());
        // At ~1 min, thresholds are: strong=0.05%, lean=0.02%
        // 0.05% >= 0.05% strong -> StrongUp
        assert_eq!(late_result.unwrap().signal, Signal::StrongUp);
    }
}
