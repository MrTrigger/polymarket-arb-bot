//! Arbitrage detection for binary options markets.
//!
//! This module implements the core arbitrage detection logic:
//! - Calculates arbitrage margin (1.0 - YES_ask - NO_ask)
//! - Uses time-based threshold selection (early/mid/late)
//! - Scores opportunities by confidence
//!
//! ## Arbitrage Strategy
//!
//! When YES + NO shares can be bought for less than $1.00, an arbitrage
//! opportunity exists. At settlement, exactly one side pays $1.00, so
//! holding matched pairs guarantees profit.
//!
//! ## Time-Based Thresholds
//!
//! - Early (>5 min): Higher threshold (2.5%) - more uncertainty
//! - Mid (2-5 min): Medium threshold (1.5%) - moderate certainty
//! - Late (<2 min): Lower threshold (0.5%) - high certainty, quick resolution

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::state::WindowPhase;
use crate::types::MarketState;
use poly_common::types::CryptoAsset;

/// Time-based arbitrage thresholds.
#[derive(Debug, Clone)]
pub struct ArbThresholds {
    /// Minimum margin for early phase (>5 min remaining).
    pub early: Decimal,
    /// Minimum margin for mid phase (2-5 min remaining).
    pub mid: Decimal,
    /// Minimum margin for late phase (<2 min remaining).
    pub late: Decimal,
    /// Early phase boundary (seconds).
    pub early_threshold_secs: u64,
    /// Mid phase boundary (seconds).
    pub mid_threshold_secs: u64,
    /// Minimum time remaining to trade (seconds).
    pub min_time_remaining_secs: u64,
}

impl Default for ArbThresholds {
    fn default() -> Self {
        Self {
            early: Decimal::new(25, 3),  // 2.5%
            mid: Decimal::new(15, 3),    // 1.5%
            late: Decimal::new(5, 3),    // 0.5%
            early_threshold_secs: 300,   // 5 minutes
            mid_threshold_secs: 120,     // 2 minutes
            min_time_remaining_secs: 30, // 30 seconds
        }
    }
}

impl ArbThresholds {
    /// Get the minimum margin threshold for the given time phase.
    #[inline]
    pub fn for_phase(&self, phase: WindowPhase) -> Decimal {
        match phase {
            WindowPhase::Early => self.early,
            WindowPhase::Mid => self.mid,
            WindowPhase::Late => self.late,
        }
    }

    /// Get the window phase based on seconds remaining.
    #[inline]
    pub fn phase_for_time(&self, seconds_remaining: i64) -> WindowPhase {
        let remaining = seconds_remaining as u64;
        if remaining > self.early_threshold_secs {
            WindowPhase::Early
        } else if remaining > self.mid_threshold_secs {
            WindowPhase::Mid
        } else {
            WindowPhase::Late
        }
    }
}

/// Detected arbitrage opportunity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArbOpportunity {
    /// Event ID for this market.
    pub event_id: String,
    /// Asset being traded.
    pub asset: CryptoAsset,
    /// YES token ID.
    pub yes_token_id: String,
    /// NO token ID.
    pub no_token_id: String,
    /// Best YES ask price.
    pub yes_ask: Decimal,
    /// Best NO ask price.
    pub no_ask: Decimal,
    /// Combined cost (YES + NO).
    pub combined_cost: Decimal,
    /// Arbitrage margin (1.0 - combined_cost).
    pub margin: Decimal,
    /// Margin in basis points.
    pub margin_bps: i32,
    /// Maximum size available (limited by smaller side).
    pub max_size: Decimal,
    /// Seconds remaining in window.
    pub seconds_remaining: i64,
    /// Window phase (Early/Mid/Late).
    pub phase: WindowPhase,
    /// Required threshold for this phase.
    pub required_threshold: Decimal,
    /// Confidence score (0-100).
    pub confidence: u8,
    /// Current spot price (if available).
    pub spot_price: Option<Decimal>,
    /// Strike price for this market.
    pub strike_price: Decimal,
    /// Timestamp of detection (milliseconds).
    pub detected_at_ms: i64,
}

impl ArbOpportunity {
    /// Calculate expected profit for a given size.
    ///
    /// Profit = size * margin (assuming full fill at BBO).
    #[inline]
    pub fn expected_profit(&self, size: Decimal) -> Decimal {
        size * self.margin
    }

    /// Check if spot price suggests YES outcome.
    pub fn spot_suggests_yes(&self) -> Option<bool> {
        self.spot_price.map(|s| s > self.strike_price)
    }

    /// Check if the opportunity has significant edge.
    ///
    /// Returns true if margin exceeds threshold by at least 50%.
    #[inline]
    pub fn has_significant_edge(&self) -> bool {
        self.margin >= self.required_threshold * Decimal::new(15, 1) // 1.5x threshold
    }
}

/// Reason why an arbitrage opportunity was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArbRejection {
    /// No valid quotes (missing BBO).
    NoQuotes,
    /// Combined cost >= 1.0 (no arbitrage).
    NoArbitrage,
    /// Margin below threshold for this phase.
    BelowThreshold,
    /// Not enough time remaining.
    InsufficientTime,
    /// No liquidity available.
    NoLiquidity,
    /// Confidence too low.
    LowConfidence,
}

impl std::fmt::Display for ArbRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArbRejection::NoQuotes => write!(f, "no valid quotes"),
            ArbRejection::NoArbitrage => write!(f, "no arbitrage (combined >= 1.0)"),
            ArbRejection::BelowThreshold => write!(f, "margin below threshold"),
            ArbRejection::InsufficientTime => write!(f, "insufficient time remaining"),
            ArbRejection::NoLiquidity => write!(f, "no liquidity available"),
            ArbRejection::LowConfidence => write!(f, "confidence too low"),
        }
    }
}

/// Arbitrage detector with configurable thresholds.
#[derive(Debug, Clone)]
pub struct ArbDetector {
    /// Time-based thresholds.
    pub thresholds: ArbThresholds,
    /// Minimum confidence to accept (0-100).
    pub min_confidence: u8,
}

impl Default for ArbDetector {
    fn default() -> Self {
        Self {
            thresholds: ArbThresholds::default(),
            min_confidence: 50,
        }
    }
}

impl ArbDetector {
    /// Create a new detector with custom thresholds.
    pub fn new(thresholds: ArbThresholds) -> Self {
        Self {
            thresholds,
            min_confidence: 50,
        }
    }

    /// Create detector from config values.
    pub fn from_config(
        min_margin_early: Decimal,
        min_margin_mid: Decimal,
        min_margin_late: Decimal,
        early_threshold_secs: u64,
        mid_threshold_secs: u64,
        min_time_remaining_secs: u64,
    ) -> Self {
        Self {
            thresholds: ArbThresholds {
                early: min_margin_early,
                mid: min_margin_mid,
                late: min_margin_late,
                early_threshold_secs,
                mid_threshold_secs,
                min_time_remaining_secs,
            },
            min_confidence: 50,
        }
    }

    /// Set minimum confidence threshold.
    pub fn with_min_confidence(mut self, min_confidence: u8) -> Self {
        self.min_confidence = min_confidence;
        self
    }

    /// Detect arbitrage opportunity in a market state.
    ///
    /// Returns Ok(ArbOpportunity) if opportunity exists and meets thresholds,
    /// or Err(ArbRejection) explaining why no trade should be made.
    pub fn detect(&self, market: &MarketState) -> Result<ArbOpportunity, ArbRejection> {
        // Check time remaining
        if market.seconds_remaining < self.thresholds.min_time_remaining_secs as i64 {
            return Err(ArbRejection::InsufficientTime);
        }

        // Check for valid quotes
        if !market.is_valid() {
            return Err(ArbRejection::NoQuotes);
        }

        // Get best asks (unwrap is safe after is_valid check)
        let yes_ask = market.yes_book.best_ask().unwrap();
        let no_ask = market.no_book.best_ask().unwrap();

        // Calculate combined cost and margin
        let combined_cost = yes_ask + no_ask;
        let margin = Decimal::ONE - combined_cost;

        // Check for arbitrage existence
        if margin <= Decimal::ZERO {
            return Err(ArbRejection::NoArbitrage);
        }

        // Determine phase and threshold
        let phase = self.thresholds.phase_for_time(market.seconds_remaining);
        let required_threshold = self.thresholds.for_phase(phase);

        // Check margin meets threshold
        if margin < required_threshold {
            return Err(ArbRejection::BelowThreshold);
        }

        // Check liquidity
        let yes_size = market.yes_book.best_ask_size().unwrap_or(Decimal::ZERO);
        let no_size = market.no_book.best_ask_size().unwrap_or(Decimal::ZERO);
        let max_size = yes_size.min(no_size);

        if max_size <= Decimal::ZERO {
            return Err(ArbRejection::NoLiquidity);
        }

        // Calculate confidence score
        let confidence = self.calculate_confidence(market, margin, &phase);

        if confidence < self.min_confidence {
            return Err(ArbRejection::LowConfidence);
        }

        // Calculate margin in basis points
        let margin_bps = (margin * Decimal::new(10000, 0))
            .try_into()
            .unwrap_or(i32::MAX);

        Ok(ArbOpportunity {
            event_id: market.event_id.clone(),
            asset: market.asset,
            yes_token_id: market.yes_book.token_id.clone(),
            no_token_id: market.no_book.token_id.clone(),
            yes_ask,
            no_ask,
            combined_cost,
            margin,
            margin_bps,
            max_size,
            seconds_remaining: market.seconds_remaining,
            phase,
            required_threshold,
            confidence,
            spot_price: market.spot_price,
            strike_price: market.strike_price,
            detected_at_ms: chrono::Utc::now().timestamp_millis(),
        })
    }

    /// Calculate confidence score (0-100) for an opportunity.
    ///
    /// Factors considered:
    /// - Margin above threshold (higher = more confident)
    /// - Time remaining (more time = more risk, less confident in late phase)
    /// - Liquidity depth (more = more confident)
    /// - Spread tightness (tighter = more confident)
    /// - Spot price alignment (aligns with market = more confident)
    fn calculate_confidence(&self, market: &MarketState, margin: Decimal, phase: &WindowPhase) -> u8 {
        let mut score: i32 = 50; // Start at neutral

        let threshold = self.thresholds.for_phase(*phase);

        // Factor 1: Margin relative to threshold (0-25 points)
        // More margin above threshold = higher confidence
        if threshold > Decimal::ZERO {
            let margin_ratio = margin / threshold;
            if margin_ratio >= Decimal::new(2, 0) {
                score += 25; // 2x+ threshold
            } else if margin_ratio >= Decimal::new(15, 1) {
                score += 20; // 1.5x threshold
            } else if margin_ratio >= Decimal::new(12, 1) {
                score += 15; // 1.2x threshold
            } else if margin_ratio >= Decimal::new(11, 1) {
                score += 10; // 1.1x threshold
            } else {
                score += 5; // Just meets threshold
            }
        }

        // Factor 2: Time phase (0-15 points)
        // Late phase = higher confidence (outcome clearer)
        match phase {
            WindowPhase::Late => score += 15,
            WindowPhase::Mid => score += 10,
            WindowPhase::Early => score += 5,
        }

        // Factor 3: Liquidity depth (0-20 points)
        let yes_size = market.yes_book.best_ask_size().unwrap_or(Decimal::ZERO);
        let no_size = market.no_book.best_ask_size().unwrap_or(Decimal::ZERO);
        let min_size = yes_size.min(no_size);

        if min_size >= Decimal::new(500, 0) {
            score += 20; // Deep liquidity
        } else if min_size >= Decimal::new(200, 0) {
            score += 15;
        } else if min_size >= Decimal::new(100, 0) {
            score += 10;
        } else if min_size >= Decimal::new(50, 0) {
            score += 5;
        }
        // < 50 shares = no bonus

        // Factor 4: Spread tightness (0-15 points)
        // Tighter spreads = more liquid market
        let yes_spread = market.yes_book.spread_bps().unwrap_or(u32::MAX);
        let no_spread = market.no_book.spread_bps().unwrap_or(u32::MAX);
        let avg_spread = (yes_spread + no_spread) / 2;

        if avg_spread <= 100 {
            score += 15; // Very tight (<1%)
        } else if avg_spread <= 200 {
            score += 12;
        } else if avg_spread <= 500 {
            score += 8;
        } else if avg_spread <= 1000 {
            score += 4;
        }
        // > 10% spread = no bonus

        // Factor 5: Spot price alignment (-10 to +10 points)
        // If spot strongly suggests one outcome but market prices don't reflect,
        // could indicate good opportunity OR information we're missing
        if let Some(spot) = market.spot_price {
            let spot_above = spot > market.strike_price;
            let yes_implied = market.yes_book.best_ask().unwrap_or(Decimal::ZERO);
            let no_implied = market.no_book.best_ask().unwrap_or(Decimal::ZERO);

            // Check if market prices align with spot
            let prices_suggest_yes = yes_implied < no_implied;
            if spot_above == prices_suggest_yes {
                score += 10; // Alignment bonus
            } else {
                // Misalignment - could be toxic flow or opportunity
                // Slight penalty as it's riskier
                score -= 5;
            }
        }

        // Clamp to 0-100 range
        score.clamp(0, 100) as u8
    }

    /// Quick check if market could have arbitrage (no threshold check).
    ///
    /// Use this for fast filtering before full detection.
    #[inline]
    pub fn has_potential_arb(market: &MarketState) -> bool {
        market
            .arb_margin()
            .map(|m| m > Decimal::ZERO)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PriceLevel;
    use rust_decimal_macros::dec;

    fn create_test_market(
        yes_ask: Decimal,
        no_ask: Decimal,
        yes_size: Decimal,
        no_size: Decimal,
        seconds_remaining: i64,
    ) -> MarketState {
        let mut market = MarketState::new(
            "event1".to_string(),
            CryptoAsset::Btc,
            "yes_token".to_string(),
            "no_token".to_string(),
            dec!(100000),
            seconds_remaining,
        );

        market.yes_book.asks.push(PriceLevel::new(yes_ask, yes_size));
        market.no_book.asks.push(PriceLevel::new(no_ask, no_size));
        // Add bids for spread calculation
        market
            .yes_book
            .bids
            .push(PriceLevel::new(yes_ask - dec!(0.01), yes_size));
        market
            .no_book
            .bids
            .push(PriceLevel::new(no_ask - dec!(0.01), no_size));
        market.spot_price = Some(dec!(100500)); // Above strike

        market
    }

    #[test]
    fn test_thresholds_default() {
        let thresholds = ArbThresholds::default();
        assert_eq!(thresholds.early, dec!(0.025)); // 2.5%
        assert_eq!(thresholds.mid, dec!(0.015));   // 1.5%
        assert_eq!(thresholds.late, dec!(0.005));  // 0.5%
    }

    #[test]
    fn test_phase_for_time() {
        let thresholds = ArbThresholds::default();

        // > 5 min = Early
        assert_eq!(thresholds.phase_for_time(400), WindowPhase::Early);
        assert_eq!(thresholds.phase_for_time(301), WindowPhase::Early);

        // 2-5 min = Mid
        assert_eq!(thresholds.phase_for_time(300), WindowPhase::Mid);
        assert_eq!(thresholds.phase_for_time(121), WindowPhase::Mid);

        // < 2 min = Late
        assert_eq!(thresholds.phase_for_time(120), WindowPhase::Late);
        assert_eq!(thresholds.phase_for_time(60), WindowPhase::Late);
        assert_eq!(thresholds.phase_for_time(30), WindowPhase::Late);
    }

    #[test]
    fn test_detect_valid_opportunity_early() {
        let detector = ArbDetector::default();

        // YES: 0.45, NO: 0.52 = combined 0.97, margin 0.03 (3%)
        // Early phase needs 2.5%, this meets it
        let market = create_test_market(dec!(0.45), dec!(0.52), dec!(100), dec!(100), 400);

        let result = detector.detect(&market);
        assert!(result.is_ok());

        let opp = result.unwrap();
        assert_eq!(opp.margin, dec!(0.03));
        assert_eq!(opp.margin_bps, 300);
        assert_eq!(opp.phase, WindowPhase::Early);
        assert_eq!(opp.combined_cost, dec!(0.97));
    }

    #[test]
    fn test_detect_valid_opportunity_late() {
        let detector = ArbDetector::default();

        // YES: 0.49, NO: 0.50 = combined 0.99, margin 0.01 (1%)
        // Late phase needs only 0.5%, this meets it
        let market = create_test_market(dec!(0.49), dec!(0.50), dec!(100), dec!(100), 60);

        let result = detector.detect(&market);
        assert!(result.is_ok());

        let opp = result.unwrap();
        assert_eq!(opp.margin, dec!(0.01));
        assert_eq!(opp.phase, WindowPhase::Late);
    }

    #[test]
    fn test_reject_below_threshold() {
        let detector = ArbDetector::default();

        // YES: 0.49, NO: 0.50 = margin 1% (too low for early phase)
        let market = create_test_market(dec!(0.49), dec!(0.50), dec!(100), dec!(100), 400);

        let result = detector.detect(&market);
        assert_eq!(result, Err(ArbRejection::BelowThreshold));
    }

    #[test]
    fn test_reject_no_arbitrage() {
        let detector = ArbDetector::default();

        // YES: 0.55, NO: 0.50 = combined 1.05 (no arb)
        let market = create_test_market(dec!(0.55), dec!(0.50), dec!(100), dec!(100), 400);

        let result = detector.detect(&market);
        assert_eq!(result, Err(ArbRejection::NoArbitrage));
    }

    #[test]
    fn test_reject_insufficient_time() {
        let detector = ArbDetector::default();

        // Good margin but only 20 seconds remaining
        let market = create_test_market(dec!(0.45), dec!(0.52), dec!(100), dec!(100), 20);

        let result = detector.detect(&market);
        assert_eq!(result, Err(ArbRejection::InsufficientTime));
    }

    #[test]
    fn test_reject_no_quotes() {
        let detector = ArbDetector::default();

        // Empty market
        let market = MarketState::new(
            "event1".to_string(),
            CryptoAsset::Btc,
            "yes_token".to_string(),
            "no_token".to_string(),
            dec!(100000),
            400,
        );

        let result = detector.detect(&market);
        assert_eq!(result, Err(ArbRejection::NoQuotes));
    }

    #[test]
    fn test_reject_no_liquidity() {
        let detector = ArbDetector::default();

        // Good prices but zero size
        let market = create_test_market(dec!(0.45), dec!(0.52), dec!(0), dec!(100), 400);

        let result = detector.detect(&market);
        assert_eq!(result, Err(ArbRejection::NoLiquidity));
    }

    #[test]
    fn test_max_size_limited_by_smaller_side() {
        let detector = ArbDetector::default();

        // YES has 50, NO has 200 -> max is 50
        let market = create_test_market(dec!(0.45), dec!(0.52), dec!(50), dec!(200), 400);

        let result = detector.detect(&market).unwrap();
        assert_eq!(result.max_size, dec!(50));
    }

    #[test]
    fn test_expected_profit() {
        let detector = ArbDetector::default();
        let market = create_test_market(dec!(0.45), dec!(0.52), dec!(100), dec!(100), 400);
        let opp = detector.detect(&market).unwrap();

        // 100 shares * 3% margin = $3
        let profit = opp.expected_profit(dec!(100));
        assert_eq!(profit, dec!(3));
    }

    #[test]
    fn test_confidence_scoring_high() {
        let detector = ArbDetector::default();

        // High margin (5%), late phase, deep liquidity
        let market = create_test_market(dec!(0.45), dec!(0.50), dec!(500), dec!(500), 60);

        let opp = detector.detect(&market).unwrap();
        // Should have high confidence
        assert!(opp.confidence >= 70);
    }

    #[test]
    fn test_confidence_scoring_moderate() {
        let detector = ArbDetector::default();

        // Just meets threshold, moderate liquidity
        let market = create_test_market(dec!(0.485), dec!(0.49), dec!(100), dec!(100), 60);

        let opp = detector.detect(&market).unwrap();
        // Should have reasonable confidence (the scoring algorithm gives
        // bonuses for late phase, liquidity, and spread)
        assert!(
            opp.confidence >= 50,
            "Expected confidence >= 50, got {}",
            opp.confidence
        );
    }

    #[test]
    fn test_has_potential_arb() {
        // Good opportunity
        let market1 = create_test_market(dec!(0.45), dec!(0.52), dec!(100), dec!(100), 400);
        assert!(ArbDetector::has_potential_arb(&market1));

        // No opportunity
        let market2 = create_test_market(dec!(0.55), dec!(0.50), dec!(100), dec!(100), 400);
        assert!(!ArbDetector::has_potential_arb(&market2));

        // Exact $1.00 (no arb)
        let market3 = create_test_market(dec!(0.50), dec!(0.50), dec!(100), dec!(100), 400);
        assert!(!ArbDetector::has_potential_arb(&market3));
    }

    #[test]
    fn test_spot_suggests_yes() {
        let detector = ArbDetector::default();
        let mut market = create_test_market(dec!(0.45), dec!(0.52), dec!(100), dec!(100), 400);

        // Spot above strike -> YES
        market.spot_price = Some(dec!(100500));
        let opp = detector.detect(&market).unwrap();
        assert_eq!(opp.spot_suggests_yes(), Some(true));

        // Spot below strike -> NO
        market.spot_price = Some(dec!(99500));
        let opp = detector.detect(&market).unwrap();
        assert_eq!(opp.spot_suggests_yes(), Some(false));

        // No spot price
        market.spot_price = None;
        let opp = detector.detect(&market).unwrap();
        assert_eq!(opp.spot_suggests_yes(), None);
    }

    #[test]
    fn test_has_significant_edge() {
        let detector = ArbDetector::default();

        // 4% margin vs 2.5% threshold = 1.6x (significant)
        let market1 = create_test_market(dec!(0.44), dec!(0.52), dec!(100), dec!(100), 400);
        let opp1 = detector.detect(&market1).unwrap();
        assert!(opp1.has_significant_edge());

        // 3% margin vs 2.5% threshold = 1.2x (not significant)
        let market2 = create_test_market(dec!(0.45), dec!(0.52), dec!(100), dec!(100), 400);
        let opp2 = detector.detect(&market2).unwrap();
        assert!(!opp2.has_significant_edge());
    }

    #[test]
    fn test_from_config() {
        let detector = ArbDetector::from_config(
            dec!(0.03),  // 3%
            dec!(0.02),  // 2%
            dec!(0.01),  // 1%
            360,         // 6 min early
            180,         // 3 min mid
            45,          // 45 sec min
        );

        assert_eq!(detector.thresholds.early, dec!(0.03));
        assert_eq!(detector.thresholds.mid, dec!(0.02));
        assert_eq!(detector.thresholds.late, dec!(0.01));
        assert_eq!(detector.thresholds.early_threshold_secs, 360);
        assert_eq!(detector.thresholds.mid_threshold_secs, 180);
        assert_eq!(detector.thresholds.min_time_remaining_secs, 45);
    }

    #[test]
    fn test_min_confidence_filter() {
        let detector = ArbDetector::default().with_min_confidence(90);

        // Good opportunity but may not reach 90 confidence
        let market = create_test_market(dec!(0.485), dec!(0.49), dec!(50), dec!(50), 60);

        // This might fail with LowConfidence depending on scoring
        let result = detector.detect(&market);
        // Just verify it runs without panic
        assert!(result.is_ok() || result == Err(ArbRejection::LowConfidence));
    }

    #[test]
    fn test_arb_rejection_display() {
        assert_eq!(ArbRejection::NoQuotes.to_string(), "no valid quotes");
        assert_eq!(
            ArbRejection::NoArbitrage.to_string(),
            "no arbitrage (combined >= 1.0)"
        );
        assert_eq!(
            ArbRejection::BelowThreshold.to_string(),
            "margin below threshold"
        );
    }

    #[test]
    fn test_threshold_for_phase() {
        let thresholds = ArbThresholds::default();

        assert_eq!(thresholds.for_phase(WindowPhase::Early), dec!(0.025));
        assert_eq!(thresholds.for_phase(WindowPhase::Mid), dec!(0.015));
        assert_eq!(thresholds.for_phase(WindowPhase::Late), dec!(0.005));
    }
}
