//! Maker rebate strategy detector.
//!
//! This module implements passive maker order detection for earning rebates:
//! - Analyzes spread to find optimal maker price placement
//! - Respects rewards_min_size and rewards_max_spread from the API
//! - Calculates expected rebates based on order size and reward config
//!
//! ## Strategy Overview
//!
//! Maker orders provide liquidity and earn rebates when filled:
//! - Place limit orders inside the spread
//! - Orders must meet minimum size requirements for rewards
//! - Spread must be within maximum allowed for rewards
//! - Expected rebate depends on daily rewards pool and market share
//!
//! ## Maker Price Calculation
//!
//! Price placement depends on spread width:
//! - Wide spread (>3%): Place 40% from bid toward ask
//! - Medium spread (1-3%): Place 30% from bid toward ask
//! - Tight spread (<1%): Place at bid (for buy) or ask (for sell)

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

use crate::api::RewardsConfig;
use crate::types::{MarketState, OrderBook};
use poly_common::types::Side;

/// Reason for skipping a maker opportunity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MakerSkipReason {
    /// Order book not ready (missing bids/asks).
    BookNotReady,
    /// Spread too wide to qualify for rewards.
    SpreadTooWide,
    /// Spread too narrow to profitably place maker order.
    SpreadTooNarrow,
    /// No rewards config available for this market.
    NoRewardsConfig,
    /// Rewards not active for this market.
    RewardsNotActive,
    /// Insufficient time remaining in window.
    InsufficientTime,
    /// Size would be below minimum for rewards.
    BelowMinSize,
    /// Book imbalance too extreme (risky to provide liquidity).
    ExtremeImbalance,
}

impl std::fmt::Display for MakerSkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BookNotReady => write!(f, "book not ready"),
            Self::SpreadTooWide => write!(f, "spread too wide for rewards"),
            Self::SpreadTooNarrow => write!(f, "spread too narrow"),
            Self::NoRewardsConfig => write!(f, "no rewards config"),
            Self::RewardsNotActive => write!(f, "rewards not active"),
            Self::InsufficientTime => write!(f, "insufficient time"),
            Self::BelowMinSize => write!(f, "below min size for rewards"),
            Self::ExtremeImbalance => write!(f, "extreme book imbalance"),
        }
    }
}

/// A detected maker rebate opportunity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MakerOpportunity {
    /// Event ID for this market.
    pub event_id: String,
    /// Token ID to place order on.
    pub token_id: String,
    /// Side of the order (Buy = provide bid liquidity, Sell = provide ask liquidity).
    pub side: Side,
    /// Optimal price to place the maker order.
    pub price: Decimal,
    /// Suggested order size (respects min_size requirement).
    pub size: Decimal,
    /// Expected rebate in USDC (estimated based on daily pool share).
    pub expected_rebate: Decimal,
    /// Current spread in basis points.
    pub spread_bps: u32,
    /// Whether this order qualifies for rewards.
    pub qualifies_for_rewards: bool,
    /// Minutes remaining in window.
    pub minutes_remaining: Decimal,
    /// Best bid price.
    pub best_bid: Decimal,
    /// Best ask price.
    pub best_ask: Decimal,
    /// Detection timestamp (milliseconds).
    pub detected_at_ms: i64,
}

impl MakerOpportunity {
    /// Check if this is a buy-side opportunity (providing bid liquidity).
    #[inline]
    pub fn is_buy(&self) -> bool {
        self.side == Side::Buy
    }

    /// Check if this is a sell-side opportunity (providing ask liquidity).
    #[inline]
    pub fn is_sell(&self) -> bool {
        self.side == Side::Sell
    }

    /// Get the spread as a ratio of mid price.
    #[inline]
    pub fn spread_ratio(&self) -> Decimal {
        let mid = (self.best_bid + self.best_ask) / Decimal::TWO;
        if mid <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        (self.best_ask - self.best_bid) / mid
    }
}

/// Configuration for the maker detector.
#[derive(Debug, Clone)]
pub struct MakerConfig {
    /// Minimum time remaining (seconds) to consider maker orders.
    /// We need time for the order to get filled before window closes.
    pub min_seconds_remaining: i64,
    /// Minimum spread (bps) required to place maker orders.
    /// Below this spread, maker orders are not profitable.
    pub min_spread_bps: u32,
    /// Maximum spread (bps) for maker strategy.
    /// Above this, there's too much uncertainty.
    pub max_spread_bps: u32,
    /// Default order size when no specific size is requested.
    pub default_order_size: Decimal,
    /// Maximum book imbalance ratio before skipping (0.0 to 1.0).
    /// Higher values mean one side is much larger than the other.
    pub max_imbalance_ratio: Decimal,
    /// Estimated daily pool share for rebate calculation (0.0 to 1.0).
    /// This is a rough estimate; actual share depends on competition.
    pub estimated_pool_share: Decimal,
}

impl Default for MakerConfig {
    fn default() -> Self {
        Self {
            min_seconds_remaining: 120,          // At least 2 minutes
            min_spread_bps: 50,                  // At least 0.5% spread
            max_spread_bps: 1000,                // Max 10% spread
            default_order_size: dec!(50),        // $50 default
            max_imbalance_ratio: dec!(0.80),     // Skip if 80% imbalanced
            estimated_pool_share: dec!(0.001),   // Assume 0.1% pool share
        }
    }
}

/// Detector for maker rebate opportunities.
///
/// Analyzes order books to find opportunities to place passive maker orders
/// that will earn rebates when filled. Considers spread, rewards config,
/// and book imbalance to determine optimal placement.
#[derive(Debug, Clone)]
pub struct MakerDetector {
    /// Configuration.
    config: MakerConfig,
}

impl MakerDetector {
    /// Create a new maker detector with default config.
    pub fn new() -> Self {
        Self {
            config: MakerConfig::default(),
        }
    }

    /// Create a new maker detector with custom config.
    pub fn with_config(config: MakerConfig) -> Self {
        Self { config }
    }

    /// Detect maker opportunities for a market.
    ///
    /// Analyzes both YES and NO order books and returns opportunities
    /// for placing passive maker orders.
    ///
    /// # Arguments
    ///
    /// * `state` - Current market state with order books
    /// * `reward_config` - Reward configuration from API (optional)
    /// * `suggested_size` - Suggested order size (None = use default)
    ///
    /// # Returns
    ///
    /// Vector of maker opportunities (may be empty, or have 1-2 opportunities)
    pub fn detect(
        &self,
        state: &MarketState,
        reward_config: Option<&RewardsConfig>,
        suggested_size: Option<Decimal>,
    ) -> Vec<MakerOpportunity> {
        let mut opportunities = Vec::with_capacity(2);

        // Check time remaining
        if state.seconds_remaining < self.config.min_seconds_remaining {
            return opportunities;
        }

        let minutes_remaining = Decimal::from(state.seconds_remaining) / dec!(60);
        let order_size = suggested_size.unwrap_or(self.config.default_order_size);

        // Check YES book
        if let Ok(opp) = self.detect_for_book(
            &state.event_id,
            &state.yes_book,
            reward_config,
            order_size,
            minutes_remaining,
        ) {
            opportunities.push(opp);
        }

        // Check NO book
        if let Ok(opp) = self.detect_for_book(
            &state.event_id,
            &state.no_book,
            reward_config,
            order_size,
            minutes_remaining,
        ) {
            opportunities.push(opp);
        }

        opportunities
    }

    /// Detect a single maker opportunity for a specific order book.
    ///
    /// # Arguments
    ///
    /// * `event_id` - Market event ID
    /// * `book` - Order book to analyze
    /// * `reward_config` - Reward configuration from API
    /// * `order_size` - Desired order size
    /// * `minutes_remaining` - Minutes until window closes
    ///
    /// # Returns
    ///
    /// * `Ok(MakerOpportunity)` - Opportunity found
    /// * `Err(MakerSkipReason)` - Why no opportunity exists
    pub fn detect_for_book(
        &self,
        event_id: &str,
        book: &OrderBook,
        reward_config: Option<&RewardsConfig>,
        order_size: Decimal,
        minutes_remaining: Decimal,
    ) -> Result<MakerOpportunity, MakerSkipReason> {
        // Validate book has BBO
        let best_bid = book.best_bid().ok_or(MakerSkipReason::BookNotReady)?;
        let best_ask = book.best_ask().ok_or(MakerSkipReason::BookNotReady)?;

        // Calculate spread
        let spread_bps = book.spread_bps().ok_or(MakerSkipReason::BookNotReady)?;

        // Check spread is in valid range
        if spread_bps < self.config.min_spread_bps {
            return Err(MakerSkipReason::SpreadTooNarrow);
        }
        if spread_bps > self.config.max_spread_bps {
            return Err(MakerSkipReason::SpreadTooWide);
        }

        // Check rewards config
        let (qualifies_for_rewards, actual_size) = if let Some(config) = reward_config {
            if !config.active {
                return Err(MakerSkipReason::RewardsNotActive);
            }

            // Check if spread qualifies for rewards
            if spread_bps > config.max_spread_bps {
                return Err(MakerSkipReason::SpreadTooWide);
            }

            // Ensure size meets minimum
            let size = order_size.max(config.min_size);
            let qualifies = config.qualifies(size, spread_bps);

            (qualifies, size)
        } else {
            // No rewards config - can still place maker orders but won't get rebates
            (false, order_size)
        };

        // Check book imbalance
        let imbalance = calculate_imbalance(book);
        if imbalance.abs() > self.config.max_imbalance_ratio {
            return Err(MakerSkipReason::ExtremeImbalance);
        }

        // Determine which side to place order on
        // Positive imbalance = more bids = prefer selling (providing ask liquidity)
        // Negative imbalance = more asks = prefer buying (providing bid liquidity)
        let side = if imbalance > Decimal::ZERO {
            Side::Sell
        } else {
            Side::Buy
        };

        // Calculate maker price
        let price = calculate_maker_price(book, side)
            .ok_or(MakerSkipReason::BookNotReady)?;

        // Calculate expected rebate
        let expected_rebate = if qualifies_for_rewards {
            if let Some(config) = reward_config {
                // Estimate: daily_rate * pool_share * (size / typical_daily_volume)
                // This is a rough approximation
                config.daily_rate * self.config.estimated_pool_share
            } else {
                Decimal::ZERO
            }
        } else {
            Decimal::ZERO
        };

        Ok(MakerOpportunity {
            event_id: event_id.to_string(),
            token_id: book.token_id.clone(),
            side,
            price,
            size: actual_size,
            expected_rebate,
            spread_bps,
            qualifies_for_rewards,
            minutes_remaining,
            best_bid,
            best_ask,
            detected_at_ms: chrono::Utc::now().timestamp_millis(),
        })
    }

    /// Quick check if maker opportunities might exist.
    ///
    /// This is a fast filter to avoid full detection when clearly no opportunity.
    #[inline]
    pub fn has_potential(&self, state: &MarketState) -> bool {
        // Must have time remaining
        if state.seconds_remaining < self.config.min_seconds_remaining {
            return false;
        }

        // Check if at least one book has valid spread
        if let Some(yes_spread) = state.yes_book.spread_bps()
            && yes_spread >= self.config.min_spread_bps
            && yes_spread <= self.config.max_spread_bps
        {
            return true;
        }

        if let Some(no_spread) = state.no_book.spread_bps()
            && no_spread >= self.config.min_spread_bps
            && no_spread <= self.config.max_spread_bps
        {
            return true;
        }

        false
    }

    /// Get the minimum order size for rewards.
    ///
    /// Returns the min_size from rewards config, or the default order size
    /// if no rewards config is available.
    pub fn min_order_size(&self, reward_config: Option<&RewardsConfig>) -> Decimal {
        reward_config
            .filter(|c| c.active)
            .map(|c| c.min_size)
            .unwrap_or(self.config.default_order_size)
    }
}

impl Default for MakerDetector {
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

/// Calculate maker price by placing right at the best bid (for buys) or best ask (for sells).
/// With POST_ONLY orders, the order is rejected if it would fill immediately,
/// so we're guaranteed to be a maker. Price chasing will update if the market moves.
fn calculate_maker_price(book: &OrderBook, side: Side) -> Option<Decimal> {
    match side {
        Side::Buy => {
            // For buying, place at best bid to be a maker
            let bid = book.best_bid()?;
            Some(bid.round_dp(2))
        }
        Side::Sell => {
            // For selling, place at best ask to be a maker
            let ask = book.best_ask()?;
            Some(ask.round_dp(2))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PriceLevel;
    use poly_common::types::CryptoAsset;
    use rust_decimal_macros::dec;

    fn create_test_market_state(seconds_remaining: i64) -> MarketState {
        MarketState::new(
            "test-event".to_string(),
            CryptoAsset::Btc,
            "yes-token".to_string(),
            "no-token".to_string(),
            dec!(100000),
            seconds_remaining,
        )
    }

    fn add_book_levels(
        book: &mut OrderBook,
        bid_price: Decimal,
        bid_size: Decimal,
        ask_price: Decimal,
        ask_size: Decimal,
    ) {
        book.apply_snapshot(
            vec![PriceLevel::new(bid_price, bid_size)],
            vec![PriceLevel::new(ask_price, ask_size)],
            0,
        );
    }

    // =========================================================================
    // MakerSkipReason Tests
    // =========================================================================

    #[test]
    fn test_skip_reason_display() {
        assert_eq!(format!("{}", MakerSkipReason::BookNotReady), "book not ready");
        assert_eq!(format!("{}", MakerSkipReason::SpreadTooWide), "spread too wide for rewards");
        assert_eq!(format!("{}", MakerSkipReason::SpreadTooNarrow), "spread too narrow");
        assert_eq!(format!("{}", MakerSkipReason::NoRewardsConfig), "no rewards config");
        assert_eq!(format!("{}", MakerSkipReason::RewardsNotActive), "rewards not active");
        assert_eq!(format!("{}", MakerSkipReason::InsufficientTime), "insufficient time");
        assert_eq!(format!("{}", MakerSkipReason::BelowMinSize), "below min size for rewards");
        assert_eq!(format!("{}", MakerSkipReason::ExtremeImbalance), "extreme book imbalance");
    }

    // =========================================================================
    // MakerOpportunity Tests
    // =========================================================================

    #[test]
    fn test_opportunity_is_buy_sell() {
        let buy_opp = MakerOpportunity {
            event_id: "test".to_string(),
            token_id: "token".to_string(),
            side: Side::Buy,
            price: dec!(0.45),
            size: dec!(50),
            expected_rebate: dec!(0.01),
            spread_bps: 100,
            qualifies_for_rewards: true,
            minutes_remaining: dec!(5),
            best_bid: dec!(0.44),
            best_ask: dec!(0.46),
            detected_at_ms: 0,
        };

        assert!(buy_opp.is_buy());
        assert!(!buy_opp.is_sell());

        let sell_opp = MakerOpportunity {
            side: Side::Sell,
            ..buy_opp.clone()
        };

        assert!(!sell_opp.is_buy());
        assert!(sell_opp.is_sell());
    }

    #[test]
    fn test_opportunity_spread_ratio() {
        let opp = MakerOpportunity {
            event_id: "test".to_string(),
            token_id: "token".to_string(),
            side: Side::Buy,
            price: dec!(0.45),
            size: dec!(50),
            expected_rebate: dec!(0),
            spread_bps: 444, // ~4.4% spread
            qualifies_for_rewards: false,
            minutes_remaining: dec!(5),
            best_bid: dec!(0.44),
            best_ask: dec!(0.46),
            detected_at_ms: 0,
        };

        // spread = 0.02, mid = 0.45
        // ratio = 0.02 / 0.45 = 0.0444...
        let ratio = opp.spread_ratio();
        assert!(ratio > dec!(0.044) && ratio < dec!(0.045));
    }

    // =========================================================================
    // MakerConfig Tests
    // =========================================================================

    #[test]
    fn test_config_default() {
        let config = MakerConfig::default();
        assert_eq!(config.min_seconds_remaining, 120);
        assert_eq!(config.min_spread_bps, 50);
        assert_eq!(config.max_spread_bps, 1000);
        assert_eq!(config.default_order_size, dec!(50));
        assert_eq!(config.max_imbalance_ratio, dec!(0.80));
        assert_eq!(config.estimated_pool_share, dec!(0.001));
    }

    // =========================================================================
    // MakerDetector Tests
    // =========================================================================

    #[test]
    fn test_detector_creation() {
        let detector = MakerDetector::new();
        assert_eq!(detector.config.min_seconds_remaining, 120);

        let custom_config = MakerConfig {
            min_seconds_remaining: 300,
            ..Default::default()
        };
        let detector2 = MakerDetector::with_config(custom_config);
        assert_eq!(detector2.config.min_seconds_remaining, 300);
    }

    #[test]
    fn test_detector_default() {
        let detector = MakerDetector::default();
        assert_eq!(detector.config.min_seconds_remaining, 120);
    }

    #[test]
    fn test_detect_for_book_no_bbo() {
        let detector = MakerDetector::new();
        let book = OrderBook::new("test".to_string());

        let result = detector.detect_for_book(
            "event",
            &book,
            None,
            dec!(50),
            dec!(5),
        );
        assert_eq!(result.unwrap_err(), MakerSkipReason::BookNotReady);
    }

    #[test]
    fn test_detect_for_book_spread_too_narrow() {
        let detector = MakerDetector::new();
        let mut book = OrderBook::new("test".to_string());
        // Very tight spread: 0.498 bid, 0.502 ask = ~0.8% spread = ~80 bps
        // Default min is 50 bps, but let's make it even tighter
        // Actually ~80 bps > 50 bps so should pass
        // Use 0.499 and 0.501 for ~40 bps spread
        add_book_levels(&mut book, dec!(0.4995), dec!(100), dec!(0.5005), dec!(100));

        let result = detector.detect_for_book(
            "event",
            &book,
            None,
            dec!(50),
            dec!(5),
        );
        // Spread = 0.001, mid = 0.50, spread_bps = 20
        // 20 < 50 (min_spread_bps) -> SpreadTooNarrow
        assert_eq!(result.unwrap_err(), MakerSkipReason::SpreadTooNarrow);
    }

    #[test]
    fn test_detect_for_book_spread_too_wide() {
        let detector = MakerDetector::new();
        let mut book = OrderBook::new("test".to_string());
        // Very wide spread: 0.30 bid, 0.70 ask = ~57% spread
        add_book_levels(&mut book, dec!(0.30), dec!(100), dec!(0.70), dec!(100));

        let result = detector.detect_for_book(
            "event",
            &book,
            None,
            dec!(50),
            dec!(5),
        );
        // Spread = 0.40, mid = 0.50, spread_bps = 8000
        // 8000 > 1000 (max_spread_bps) -> SpreadTooWide
        assert_eq!(result.unwrap_err(), MakerSkipReason::SpreadTooWide);
    }

    #[test]
    fn test_detect_for_book_rewards_not_active() {
        let detector = MakerDetector::new();
        let mut book = OrderBook::new("test".to_string());
        // Use a spread that is within our config limits but would pass all other checks
        // We need spread 50-1000 bps, and rewards max_spread >= spread for this to reach rewards check
        add_book_levels(&mut book, dec!(0.485), dec!(100), dec!(0.50), dec!(100));
        // spread = 0.015, mid = 0.4925, spread_bps = ~305

        // Set inactive rewards with high max_spread so we don't fail on spread check
        let inactive_rewards = RewardsConfig::new(dec!(10), 500, dec!(100), false);

        let result = detector.detect_for_book(
            "event",
            &book,
            Some(&inactive_rewards),
            dec!(50),
            dec!(5),
        );
        assert_eq!(result.unwrap_err(), MakerSkipReason::RewardsNotActive);
    }

    #[test]
    fn test_detect_for_book_spread_exceeds_rewards_max() {
        let detector = MakerDetector::new();
        let mut book = OrderBook::new("test".to_string());
        // Spread: ~10% = 1000 bps
        add_book_levels(&mut book, dec!(0.45), dec!(100), dec!(0.55), dec!(100));

        // Rewards max spread is 200 bps
        let rewards = RewardsConfig::new(dec!(10), 200, dec!(100), true);

        let result = detector.detect_for_book(
            "event",
            &book,
            Some(&rewards),
            dec!(50),
            dec!(5),
        );
        // Spread = 0.10, mid = 0.50, spread_bps = 2000
        // 2000 > 200 (rewards max) -> SpreadTooWide
        assert_eq!(result.unwrap_err(), MakerSkipReason::SpreadTooWide);
    }

    #[test]
    fn test_detect_for_book_extreme_imbalance() {
        let config = MakerConfig {
            max_imbalance_ratio: dec!(0.50), // Stricter threshold for test
            ..Default::default()
        };
        let detector = MakerDetector::with_config(config);

        let mut book = OrderBook::new("test".to_string());
        // Extreme imbalance: 1000 bids vs 50 asks
        add_book_levels(&mut book, dec!(0.47), dec!(1000), dec!(0.50), dec!(50));

        let result = detector.detect_for_book(
            "event",
            &book,
            None,
            dec!(50),
            dec!(5),
        );
        // Imbalance = (1000 - 50) / 1050 = 0.905
        // 0.905 > 0.50 -> ExtremeImbalance
        assert_eq!(result.unwrap_err(), MakerSkipReason::ExtremeImbalance);
    }

    #[test]
    fn test_detect_for_book_success_no_rewards() {
        let detector = MakerDetector::new();
        let mut book = OrderBook::new("test".to_string());
        // Good spread: ~5.3% = 530 bps
        add_book_levels(&mut book, dec!(0.47), dec!(100), dec!(0.50), dec!(100));

        let result = detector.detect_for_book(
            "event",
            &book,
            None,
            dec!(50),
            dec!(5),
        );

        assert!(result.is_ok());
        let opp = result.unwrap();
        assert_eq!(opp.event_id, "event");
        assert_eq!(opp.token_id, "test");
        assert_eq!(opp.size, dec!(50));
        assert!(!opp.qualifies_for_rewards);
        assert_eq!(opp.expected_rebate, Decimal::ZERO);
        assert_eq!(opp.best_bid, dec!(0.47));
        assert_eq!(opp.best_ask, dec!(0.50));
    }

    #[test]
    fn test_detect_for_book_success_with_rewards() {
        let detector = MakerDetector::new();
        let mut book = OrderBook::new("test".to_string());
        // Good spread: ~3.1% = 310 bps (under 500 bps reward max)
        add_book_levels(&mut book, dec!(0.485), dec!(100), dec!(0.50), dec!(100));

        // Rewards config: min_size=10, max_spread=500, active=true
        let rewards = RewardsConfig::new(dec!(10), 500, dec!(100), true);

        let result = detector.detect_for_book(
            "event",
            &book,
            Some(&rewards),
            dec!(50),
            dec!(5),
        );

        assert!(result.is_ok());
        let opp = result.unwrap();
        assert!(opp.qualifies_for_rewards);
        assert!(opp.expected_rebate > Decimal::ZERO);
        assert_eq!(opp.size, dec!(50)); // Max of requested and min_size
    }

    #[test]
    fn test_detect_for_book_respects_min_size() {
        let detector = MakerDetector::new();
        let mut book = OrderBook::new("test".to_string());
        add_book_levels(&mut book, dec!(0.485), dec!(100), dec!(0.50), dec!(100));

        // Rewards config with high min_size
        let rewards = RewardsConfig::new(dec!(100), 500, dec!(1000), true);

        let result = detector.detect_for_book(
            "event",
            &book,
            Some(&rewards),
            dec!(50), // Request 50, but min is 100
            dec!(5),
        );

        assert!(result.is_ok());
        let opp = result.unwrap();
        assert_eq!(opp.size, dec!(100)); // Should be bumped to min_size
    }

    #[test]
    fn test_detect_for_book_side_selection() {
        let detector = MakerDetector::new();

        // Bid-heavy book -> should suggest Sell
        let mut bid_heavy = OrderBook::new("test".to_string());
        add_book_levels(&mut bid_heavy, dec!(0.47), dec!(200), dec!(0.50), dec!(50));

        let result = detector.detect_for_book(
            "event",
            &bid_heavy,
            None,
            dec!(50),
            dec!(5),
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap().side, Side::Sell);

        // Ask-heavy book -> should suggest Buy
        let mut ask_heavy = OrderBook::new("test".to_string());
        add_book_levels(&mut ask_heavy, dec!(0.47), dec!(50), dec!(0.50), dec!(200));

        let result = detector.detect_for_book(
            "event",
            &ask_heavy,
            None,
            dec!(50),
            dec!(5),
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap().side, Side::Buy);
    }

    // =========================================================================
    // detect() Method Tests
    // =========================================================================

    #[test]
    fn test_detect_insufficient_time() {
        let detector = MakerDetector::new();
        let mut state = create_test_market_state(60); // Only 1 minute left
        add_book_levels(&mut state.yes_book, dec!(0.47), dec!(100), dec!(0.50), dec!(100));
        add_book_levels(&mut state.no_book, dec!(0.47), dec!(100), dec!(0.50), dec!(100));

        let opportunities = detector.detect(&state, None, None);
        assert!(opportunities.is_empty()); // Need 2+ minutes
    }

    #[test]
    fn test_detect_returns_multiple_opportunities() {
        let detector = MakerDetector::new();
        let mut state = create_test_market_state(300);
        add_book_levels(&mut state.yes_book, dec!(0.47), dec!(100), dec!(0.50), dec!(100));
        add_book_levels(&mut state.no_book, dec!(0.47), dec!(100), dec!(0.50), dec!(100));

        let opportunities = detector.detect(&state, None, None);
        // Should get opportunities for both YES and NO books
        assert_eq!(opportunities.len(), 2);
    }

    #[test]
    fn test_detect_uses_suggested_size() {
        let detector = MakerDetector::new();
        let mut state = create_test_market_state(300);
        add_book_levels(&mut state.yes_book, dec!(0.47), dec!(100), dec!(0.50), dec!(100));

        let opportunities = detector.detect(&state, None, Some(dec!(100)));
        assert!(!opportunities.is_empty());
        assert_eq!(opportunities[0].size, dec!(100));
    }

    // =========================================================================
    // has_potential() Tests
    // =========================================================================

    #[test]
    fn test_has_potential_insufficient_time() {
        let detector = MakerDetector::new();
        let state = create_test_market_state(60); // Only 1 minute
        assert!(!detector.has_potential(&state));
    }

    #[test]
    fn test_has_potential_no_books() {
        let detector = MakerDetector::new();
        let state = create_test_market_state(300);
        assert!(!detector.has_potential(&state));
    }

    #[test]
    fn test_has_potential_spread_out_of_range() {
        let detector = MakerDetector::new();
        let mut state = create_test_market_state(300);
        // Very tight spread
        add_book_levels(&mut state.yes_book, dec!(0.499), dec!(100), dec!(0.501), dec!(100));
        assert!(!detector.has_potential(&state));
    }

    #[test]
    fn test_has_potential_valid() {
        let detector = MakerDetector::new();
        let mut state = create_test_market_state(300);
        add_book_levels(&mut state.yes_book, dec!(0.47), dec!(100), dec!(0.50), dec!(100));
        assert!(detector.has_potential(&state));
    }

    // =========================================================================
    // min_order_size() Tests
    // =========================================================================

    #[test]
    fn test_min_order_size_no_config() {
        let detector = MakerDetector::new();
        let size = detector.min_order_size(None);
        assert_eq!(size, dec!(50)); // Default order size
    }

    #[test]
    fn test_min_order_size_inactive_rewards() {
        let detector = MakerDetector::new();
        let inactive = RewardsConfig::new(dec!(100), 200, dec!(1000), false);
        let size = detector.min_order_size(Some(&inactive));
        assert_eq!(size, dec!(50)); // Falls back to default
    }

    #[test]
    fn test_min_order_size_active_rewards() {
        let detector = MakerDetector::new();
        let rewards = RewardsConfig::new(dec!(25), 200, dec!(1000), true);
        let size = detector.min_order_size(Some(&rewards));
        assert_eq!(size, dec!(25)); // Uses rewards min_size
    }

    // =========================================================================
    // calculate_imbalance() Tests
    // =========================================================================

    #[test]
    fn test_calculate_imbalance_equal() {
        let mut book = OrderBook::new("test".to_string());
        add_book_levels(&mut book, dec!(0.45), dec!(100), dec!(0.50), dec!(100));
        let imbalance = calculate_imbalance(&book);
        assert_eq!(imbalance, Decimal::ZERO);
    }

    #[test]
    fn test_calculate_imbalance_bid_heavy() {
        let mut book = OrderBook::new("test".to_string());
        add_book_levels(&mut book, dec!(0.45), dec!(300), dec!(0.50), dec!(100));
        let imbalance = calculate_imbalance(&book);
        // (300 - 100) / 400 = 0.5
        assert_eq!(imbalance, dec!(0.5));
    }

    #[test]
    fn test_calculate_imbalance_ask_heavy() {
        let mut book = OrderBook::new("test".to_string());
        add_book_levels(&mut book, dec!(0.45), dec!(100), dec!(0.50), dec!(300));
        let imbalance = calculate_imbalance(&book);
        // (100 - 300) / 400 = -0.5
        assert_eq!(imbalance, dec!(-0.5));
    }

    #[test]
    fn test_calculate_imbalance_empty() {
        let book = OrderBook::new("test".to_string());
        let imbalance = calculate_imbalance(&book);
        assert_eq!(imbalance, Decimal::ZERO);
    }

    // =========================================================================
    // calculate_maker_price() Tests
    // =========================================================================

    #[test]
    fn test_calculate_maker_price_wide_spread() {
        let mut book = OrderBook::new("test".to_string());
        add_book_levels(&mut book, dec!(0.40), dec!(100), dec!(0.50), dec!(100));
        // spread = 0.10, mid = 0.45, spread_ratio = 22% > 3%

        // Buy: bid + spread * 0.40 = 0.40 + 0.10 * 0.40 = 0.44
        let buy_price = calculate_maker_price(&book, Side::Buy);
        assert_eq!(buy_price, Some(dec!(0.44)));

        // Sell: ask - spread * 0.40 = 0.50 - 0.10 * 0.40 = 0.46
        let sell_price = calculate_maker_price(&book, Side::Sell);
        assert_eq!(sell_price, Some(dec!(0.46)));
    }

    #[test]
    fn test_calculate_maker_price_medium_spread() {
        let mut book = OrderBook::new("test".to_string());
        add_book_levels(&mut book, dec!(0.49), dec!(100), dec!(0.51), dec!(100));
        // spread = 0.02, mid = 0.50, spread_ratio = 4% -- actually 4% > 3%
        // Let's use 0.493 and 0.507 for ~2.8% spread
        book.bids.clear();
        book.asks.clear();
        add_book_levels(&mut book, dec!(0.493), dec!(100), dec!(0.507), dec!(100));
        // spread = 0.014, mid = 0.50, spread_ratio = 2.8% (between 1-3%)

        // Buy: bid + spread * 0.30 = 0.493 + 0.014 * 0.30 = 0.4972
        let buy_price = calculate_maker_price(&book, Side::Buy);
        assert_eq!(buy_price, Some(dec!(0.50))); // Rounded

        // Sell: ask - spread * 0.30 = 0.507 - 0.014 * 0.30 = 0.5028
        let sell_price = calculate_maker_price(&book, Side::Sell);
        assert_eq!(sell_price, Some(dec!(0.50))); // Rounded
    }

    #[test]
    fn test_calculate_maker_price_tight_spread() {
        let mut book = OrderBook::new("test".to_string());
        add_book_levels(&mut book, dec!(0.498), dec!(100), dec!(0.502), dec!(100));
        // spread = 0.004, mid = 0.50, spread_ratio = 0.8% < 1%

        // Buy: bid + spread * 0 = bid = 0.498
        let buy_price = calculate_maker_price(&book, Side::Buy);
        assert_eq!(buy_price, Some(dec!(0.50))); // Rounded

        // Sell: ask - spread * 0 = ask = 0.502
        let sell_price = calculate_maker_price(&book, Side::Sell);
        assert_eq!(sell_price, Some(dec!(0.50))); // Rounded
    }

    #[test]
    fn test_calculate_maker_price_no_bbo() {
        let book = OrderBook::new("test".to_string());
        assert!(calculate_maker_price(&book, Side::Buy).is_none());
        assert!(calculate_maker_price(&book, Side::Sell).is_none());
    }

    // =========================================================================
    // Serialization Tests
    // =========================================================================

    #[test]
    fn test_opportunity_serialization() {
        let opp = MakerOpportunity {
            event_id: "test".to_string(),
            token_id: "token".to_string(),
            side: Side::Buy,
            price: dec!(0.45),
            size: dec!(50),
            expected_rebate: dec!(0.01),
            spread_bps: 100,
            qualifies_for_rewards: true,
            minutes_remaining: dec!(5),
            best_bid: dec!(0.44),
            best_ask: dec!(0.46),
            detected_at_ms: 12345,
        };

        let json = serde_json::to_string(&opp).unwrap();
        // Side serializes as UPPERCASE due to serde rename_all in poly_common
        assert!(json.contains("BUY"));
        assert!(json.contains("test"));
        assert!(json.contains("0.45"));

        let parsed: MakerOpportunity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_id, opp.event_id);
        assert_eq!(parsed.side, opp.side);
        assert_eq!(parsed.price, opp.price);
    }

    #[test]
    fn test_skip_reason_serialization() {
        let reason = MakerSkipReason::SpreadTooWide;
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(json, "\"SpreadTooWide\"");

        let parsed: MakerSkipReason = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, MakerSkipReason::SpreadTooWide);
    }
}
