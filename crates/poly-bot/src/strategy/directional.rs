//! Directional trading strategy detector.
//!
//! This module implements directional trading based on price signals:
//! - Uses Signal system to determine market bias
//! - Computes confidence for position sizing
//! - Goes 100% directional on entry (no default hedge)
//! - Reactive hedging is handled separately in `check_reactive_hedge()`
//!
//! ## Strategy Overview
//!
//! When spot price diverges from strike, a directional opportunity exists:
//! - StrongUp/LeanUp: Buy YES shares only
//! - StrongDown/LeanDown: Buy NO shares only
//! - Neutral: Skip trading (no edge)

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
    /// Stale prices - YES+NO combined cost < 0.98 indicates one side hasn't updated.
    StalePrices,
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
            Self::StalePrices => write!(f, "stale prices"),
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
    /// Binance lead over Chainlink: positive = Binance further ahead in signal direction.
    /// When Binance has moved further than Chainlink, Chainlink will likely catch up.
    pub binance_lead: Option<Decimal>,
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

    /// Get the price of the favorable (dominant) side based on signal.
    ///
    /// For UP signals: we're buying more YES, so yes_ask is the favorable price.
    /// For DOWN signals: we're buying more NO, so no_ask is the favorable price.
    /// For Neutral: use the average.
    #[inline]
    pub fn favorable_price(&self) -> Decimal {
        match self.signal {
            Signal::StrongUp | Signal::LeanUp => self.yes_ask,
            Signal::StrongDown | Signal::LeanDown => self.no_ask,
            Signal::Neutral => (self.yes_ask + self.no_ask) / Decimal::TWO,
        }
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
        // YES + NO should always sum close to $1.00 on Polymarket.
        // If sum is too low (< 0.98), one side has stale prices - skip to avoid bad trades.
        // If sum is too high (> max_combined_cost), no profit margin exists.
        let combined_cost = yes_ask + no_ask;
        let min_combined_cost = Decimal::new(98, 2); // 0.98
        if combined_cost < min_combined_cost {
            // Stale prices - one side hasn't been updated yet
            return Err(DirectionalSkipReason::StalePrices);
        }
        if combined_cost > self.config.max_combined_cost {
            // No profit margin even without directional edge
            // Still allow if there's a strong signal (directional edge compensates)
        }

        // Use Chainlink as ground truth for signal detection when available
        let signal_price = state.chainlink_price.unwrap_or(spot_price);
        let signal_strike = state.chainlink_strike.unwrap_or(state.strike_price);

        // Calculate signal
        let minutes_remaining = Decimal::from(state.seconds_remaining) / dec!(60);
        let signal = get_signal(signal_price, signal_strike, minutes_remaining);

        // Skip if neutral - no directional edge
        if !signal.is_directional() {
            return Err(DirectionalSkipReason::NeutralSignal);
        }

        // Calculate distance using Chainlink (ground truth)
        let distance = calculate_distance(signal_price, signal_strike);
        let distance_dollars = (signal_price - signal_strike).abs();

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

        // Calculate Binance lead over Chainlink using each feed's OWN baseline.
        // This avoids systematic offset between feeds (Binance ~$30 higher than Chainlink).
        // binance_delta = movement from Binance's own open price
        // chainlink_delta = movement from Chainlink's own strike price
        // lead = binance_delta - chainlink_delta → positive means Binance moved further
        let binance_lead = if let (Some(cl), Some(cl_strike), Some(bn_open)) =
            (state.chainlink_price, state.chainlink_strike, state.binance_at_open)
        {
            let binance_delta = calculate_distance(spot_price, bn_open);
            let chainlink_delta = calculate_distance(cl, cl_strike);
            Some(binance_delta - chainlink_delta)
        } else {
            None
        };

        Ok(DirectionalOpportunity {
            event_id: state.event_id.clone(),
            yes_token_id: state.yes_book.token_id.clone(),
            no_token_id: state.no_book.token_id.clone(),
            signal,
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
            binance_lead,
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
            binance_lead: None,
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
            binance_lead: None,
        };

        assert!(strong_up.is_strong());

        let lean_up = DirectionalOpportunity {
            signal: Signal::LeanUp,
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
        add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));

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
        add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));

        let result = detector.detect(&state);
        assert!(result.is_ok());

        let opp = result.unwrap();
        assert_eq!(opp.signal, Signal::StrongUp);
    }

    #[test]
    fn test_detect_lean_up_signal() {
        let detector = DirectionalDetector::new();
        // 5 minutes is in 3-10 min bracket: strong=0.021%, lean=0.011%
        // 0.015% is between lean (0.011%) and strong (0.021%) -> LeanUp
        let mut state = create_test_market_state(Some(dec!(100015)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));

        let result = detector.detect(&state);
        assert!(result.is_ok());

        let opp = result.unwrap();
        assert_eq!(opp.signal, Signal::LeanUp);
    }

    #[test]
    fn test_detect_strong_down_signal() {
        let detector = DirectionalDetector::new();
        // 0.25% below strike at 5 minutes -> StrongDown
        let mut state = create_test_market_state(Some(dec!(99750)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));

        let result = detector.detect(&state);
        assert!(result.is_ok());

        let opp = result.unwrap();
        assert_eq!(opp.signal, Signal::StrongDown);
    }

    #[test]
    fn test_detect_lean_down_signal() {
        let detector = DirectionalDetector::new();
        // 5 minutes is in 3-10 min bracket: strong=0.021%, lean=0.011%
        // 0.015% is between lean (0.011%) and strong (0.021%) -> LeanDown
        let mut state = create_test_market_state(Some(dec!(99985)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));

        let result = detector.detect(&state);
        assert!(result.is_ok());

        let opp = result.unwrap();
        assert_eq!(opp.signal, Signal::LeanDown);
    }

    // =========================================================================
    // Signal Detection Tests (no ratios — single-leg directional)
    // =========================================================================

    #[test]
    fn test_strong_up_detected() {
        let detector = DirectionalDetector::new();
        let mut state = create_test_market_state(Some(dec!(100300)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));

        let opp = detector.detect(&state).unwrap();
        assert_eq!(opp.signal, Signal::StrongUp);
    }

    #[test]
    fn test_strong_down_detected() {
        let detector = DirectionalDetector::new();
        let mut state = create_test_market_state(Some(dec!(99700)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));

        let opp = detector.detect(&state).unwrap();
        assert_eq!(opp.signal, Signal::StrongDown);
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
        add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));
        assert!(!detector.has_potential(&state));
    }

    #[test]
    fn test_has_potential_valid() {
        let detector = DirectionalDetector::new();
        let mut state = create_test_market_state(Some(dec!(100000)), dec!(100000), 300);
        add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));
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
        add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));

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

        // Strong signal: 0.1% distance at 5 min (3-10 min bracket: strong=0.015%, lean=0.01%)
        // 0.1% > 0.015% -> StrongUp
        let mut strong_state = create_test_market_state(Some(dec!(100100)), dec!(100000), 300);
        add_book_levels(&mut strong_state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));
        let strong_opp = detector.detect(&strong_state).unwrap();
        assert_eq!(strong_opp.signal, Signal::StrongUp);

        // Lean signal: 0.012% distance at 5 min (3-10 min bracket: strong=0.015%, lean=0.01%)
        // 0.01% < 0.012% < 0.015% -> LeanUp
        let mut lean_state = create_test_market_state(Some(dec!(100012)), dec!(100000), 300);
        add_book_levels(&mut lean_state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));
        let lean_opp = detector.detect(&lean_state).unwrap();
        assert_eq!(lean_opp.signal, Signal::LeanUp);

        // Strong signal should have higher signal confidence component
        assert!(strong_opp.confidence.signal > lean_opp.confidence.signal,
            "Strong confidence {} should > Lean confidence {}",
            strong_opp.confidence.signal, lean_opp.confidence.signal);
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
            binance_lead: None,
        };

        let json = serde_json::to_string(&opp).unwrap();
        assert!(json.contains("StrongUp"));
        assert!(json.contains("test"));

        let parsed: DirectionalOpportunity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.signal, Signal::StrongUp);
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

        // Test each signal level with appropriate percentage distance at 5 minutes
        // Thresholds at 3-10 min bracket: strong=0.015%, lean=0.01%
        let test_cases = [
            (dec!(100050), Signal::StrongUp),   // 0.05% above > 0.015% -> StrongUp
            (dec!(100012), Signal::LeanUp),     // 0.012% above (0.01% < 0.012% < 0.015%) -> LeanUp
            (dec!(99950), Signal::StrongDown),  // 0.05% below > 0.015% -> StrongDown
            (dec!(99988), Signal::LeanDown),    // 0.012% below (0.01% < 0.012% < 0.015%) -> LeanDown
        ];

        for (spot, expected_signal) in test_cases {
            let mut state = create_test_market_state(Some(spot), dec!(100000), 300);
            add_book_levels(&mut state, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));

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

        // Same 0.008% distance, different time remaining
        let spot = dec!(100008);  // 0.008% above
        let strike = dec!(100000);

        // At 35 min (>30 min): strong=0.03%, lean=0.02%, 0.008% -> Neutral (skip)
        let mut state_early = create_test_market_state(Some(spot), strike, 2100);
        add_book_levels(&mut state_early, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));
        assert!(detector.detect(&state_early).is_err(), "Expected skip at 35 min with 0.008% distance");

        // At 2 min (<3 min): strong=0.009%, lean=0.006%, 0.008% -> LeanUp
        let mut state_late = create_test_market_state(Some(spot), strike, 120);
        add_book_levels(&mut state_late, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));
        let late_result = detector.detect(&state_late);
        assert!(late_result.is_ok(), "Expected LeanUp at 2 min with 0.008% distance");
        assert_eq!(late_result.unwrap().signal, Signal::LeanUp);

        // Use 0.02% distance to show Strong vs Lean across time
        let spot2 = dec!(100020);  // 0.02% above

        // At 2 min: strong=0.009%, lean=0.006%, 0.02% -> StrongUp
        let mut state_very_late = create_test_market_state(Some(spot2), strike, 120);
        add_book_levels(&mut state_very_late, dec!(0.48), dec!(0.50), dec!(0.48), dec!(0.50));
        let very_late_result = detector.detect(&state_very_late);
        assert!(very_late_result.is_ok());
        assert_eq!(very_late_result.unwrap().signal, Signal::StrongUp);
    }
}
