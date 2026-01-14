//! Maker price calculation utilities.
//!
//! This module provides pricing logic for placing maker orders inside the spread.
//! The goal is to offer a better price than existing orders while still earning
//! maker rebates when filled.
//!
//! ## Pricing Strategy
//!
//! The maker price is positioned inside the bid-ask spread based on spread width:
//! - Wide spread (>3%): Place 40% into spread from reference price (aggressive)
//! - Medium spread (1-3%): Place 30% into spread from reference price (moderate)
//! - Tight spread (<1%): Place at reference price (conservative)
//!
//! For buy orders, the reference is the best bid (we place above it).
//! For sell orders, the reference is the best ask (we place below it).

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::types::OrderBook;
use poly_common::types::Side;

/// Result of a maker price calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MakerPriceResult {
    /// The calculated maker price.
    pub price: Decimal,
    /// The spread category used for placement.
    pub spread_category: SpreadCategory,
    /// The placement ratio applied (0.0 to 1.0).
    pub placement_ratio: Decimal,
    /// Current spread in basis points.
    pub spread_bps: u32,
}

/// Spread category for pricing decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpreadCategory {
    /// Spread > 3% - aggressive placement
    Wide,
    /// Spread 1-3% - moderate placement
    Medium,
    /// Spread < 1% - conservative placement
    Tight,
}

impl SpreadCategory {
    /// Get the placement ratio for this spread category.
    ///
    /// This is the fraction of the spread to move from the reference price.
    #[inline]
    pub fn placement_ratio(&self) -> Decimal {
        match self {
            SpreadCategory::Wide => dec!(0.40),
            SpreadCategory::Medium => dec!(0.30),
            SpreadCategory::Tight => dec!(0.0),
        }
    }

    /// Get a human-readable description.
    pub fn description(&self) -> &'static str {
        match self {
            SpreadCategory::Wide => "wide (>3%)",
            SpreadCategory::Medium => "medium (1-3%)",
            SpreadCategory::Tight => "tight (<1%)",
        }
    }
}

impl std::fmt::Display for SpreadCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description())
    }
}

/// Calculate optimal maker price inside the spread.
///
/// Determines the best price to place a passive maker order that:
/// - Offers improvement over existing best price
/// - Maximizes probability of fill
/// - Maintains maker status for rebates
///
/// # Arguments
///
/// * `book` - Order book with current market state
/// * `side` - Side of the order (Buy = provide bid liquidity, Sell = provide ask liquidity)
///
/// # Returns
///
/// * `Some(MakerPriceResult)` - The calculated price and metadata
/// * `None` - If order book doesn't have valid BBO
///
/// # Pricing Strategy
///
/// | Spread | Placement | Rationale |
/// |--------|-----------|-----------|
/// | >3%    | 40% into spread | Wide spread = low competition, can be aggressive |
/// | 1-3%   | 30% into spread | Moderate spread = balance fill rate vs improvement |
/// | <1%    | At reference | Tight spread = must match best price to compete |
///
/// # Example
///
/// ```ignore
/// let book = /* order book with bid=0.40, ask=0.50 */;
///
/// // For a buy order with 20% spread:
/// // spread = 0.10, wide category (>3%), placement = 40%
/// // price = bid + spread * 0.40 = 0.40 + 0.04 = 0.44
/// let result = get_maker_price(&book, Side::Buy);
/// assert_eq!(result.unwrap().price, dec!(0.44));
/// ```
pub fn get_maker_price(book: &OrderBook, side: Side) -> Option<MakerPriceResult> {
    let bid = book.best_bid()?;
    let ask = book.best_ask()?;
    let spread = ask - bid;

    // Validate prices are positive
    if bid <= Decimal::ZERO || ask <= Decimal::ZERO || spread < Decimal::ZERO {
        return None;
    }

    let mid = (bid + ask) / Decimal::TWO;
    if mid <= Decimal::ZERO {
        return None;
    }

    // Calculate spread as ratio of mid price
    let spread_ratio = spread / mid;

    // Determine spread category and placement ratio
    let category = if spread_ratio > dec!(0.03) {
        SpreadCategory::Wide
    } else if spread_ratio > dec!(0.01) {
        SpreadCategory::Medium
    } else {
        SpreadCategory::Tight
    };

    let placement_ratio = category.placement_ratio();

    // Calculate spread in basis points
    let spread_bps = ((spread / mid) * dec!(10000))
        .try_into()
        .unwrap_or(u32::MAX);

    // Calculate price based on side
    let price = match side {
        Side::Buy => {
            // For buying, place above bid (inside spread toward ask)
            let raw_price = bid + (spread * placement_ratio);
            // Round to 2 decimal places (Polymarket uses $0.01 ticks)
            raw_price.round_dp(2)
        }
        Side::Sell => {
            // For selling, place below ask (inside spread toward bid)
            let raw_price = ask - (spread * placement_ratio);
            // Round to 2 decimal places
            raw_price.round_dp(2)
        }
    };

    Some(MakerPriceResult {
        price,
        spread_category: category,
        placement_ratio,
        spread_bps,
    })
}

/// Calculate maker prices for both sides of the book.
///
/// Returns buy and sell prices for a market maker providing liquidity on both sides.
///
/// # Arguments
///
/// * `book` - Order book with current market state
///
/// # Returns
///
/// * `Some((buy_price, sell_price))` - Prices for both sides
/// * `None` - If order book doesn't have valid BBO
pub fn get_maker_prices_both_sides(book: &OrderBook) -> Option<(MakerPriceResult, MakerPriceResult)> {
    let buy_result = get_maker_price(book, Side::Buy)?;
    let sell_result = get_maker_price(book, Side::Sell)?;
    Some((buy_result, sell_result))
}

/// Check if a price is inside the spread (between bid and ask).
///
/// A price inside the spread can act as a maker order.
#[inline]
pub fn is_inside_spread(book: &OrderBook, price: Decimal) -> bool {
    if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
        price > bid && price < ask
    } else {
        false
    }
}

/// Check if a price would cross the spread (take liquidity).
///
/// Buy orders at or above ask, or sell orders at or below bid, cross the spread.
#[inline]
pub fn would_cross_spread(book: &OrderBook, side: Side, price: Decimal) -> bool {
    match side {
        Side::Buy => {
            if let Some(ask) = book.best_ask() {
                price >= ask
            } else {
                false
            }
        }
        Side::Sell => {
            if let Some(bid) = book.best_bid() {
                price <= bid
            } else {
                false
            }
        }
    }
}

/// Calculate the improvement offered by a maker price.
///
/// Returns the improvement in basis points over the current best price.
/// For buy orders, this is how much better than best bid.
/// For sell orders, this is how much better than best ask.
pub fn price_improvement_bps(book: &OrderBook, side: Side, price: Decimal) -> Option<u32> {
    let (reference, mid) = match side {
        Side::Buy => {
            let bid = book.best_bid()?;
            let mid = book.mid_price()?;
            (bid, mid)
        }
        Side::Sell => {
            let ask = book.best_ask()?;
            let mid = book.mid_price()?;
            (ask, mid)
        }
    };

    if mid <= Decimal::ZERO {
        return None;
    }

    let improvement = match side {
        Side::Buy => price - reference, // Higher = better for buyers
        Side::Sell => reference - price, // Lower = better for sellers
    };

    let bps = (improvement / mid) * dec!(10000);
    Some(bps.abs().try_into().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PriceLevel;
    use rust_decimal_macros::dec;

    fn create_book_with_bbo(bid: Decimal, ask: Decimal) -> OrderBook {
        let mut book = OrderBook::new("test".to_string());
        book.apply_snapshot(
            vec![PriceLevel::new(bid, dec!(100))],
            vec![PriceLevel::new(ask, dec!(100))],
            0,
        );
        book
    }

    // =========================================================================
    // SpreadCategory Tests
    // =========================================================================

    #[test]
    fn test_spread_category_placement_ratio() {
        assert_eq!(SpreadCategory::Wide.placement_ratio(), dec!(0.40));
        assert_eq!(SpreadCategory::Medium.placement_ratio(), dec!(0.30));
        assert_eq!(SpreadCategory::Tight.placement_ratio(), dec!(0.0));
    }

    #[test]
    fn test_spread_category_description() {
        assert_eq!(SpreadCategory::Wide.description(), "wide (>3%)");
        assert_eq!(SpreadCategory::Medium.description(), "medium (1-3%)");
        assert_eq!(SpreadCategory::Tight.description(), "tight (<1%)");
    }

    #[test]
    fn test_spread_category_display() {
        assert_eq!(format!("{}", SpreadCategory::Wide), "wide (>3%)");
        assert_eq!(format!("{}", SpreadCategory::Medium), "medium (1-3%)");
        assert_eq!(format!("{}", SpreadCategory::Tight), "tight (<1%)");
    }

    // =========================================================================
    // get_maker_price Tests - Wide Spread (>3%)
    // =========================================================================

    #[test]
    fn test_wide_spread_buy() {
        // bid=0.40, ask=0.50
        // spread = 0.10, mid = 0.45, spread_ratio = 22.2% > 3%
        let book = create_book_with_bbo(dec!(0.40), dec!(0.50));
        let result = get_maker_price(&book, Side::Buy).unwrap();

        assert_eq!(result.spread_category, SpreadCategory::Wide);
        assert_eq!(result.placement_ratio, dec!(0.40));

        // price = bid + spread * 0.40 = 0.40 + 0.10 * 0.40 = 0.44
        assert_eq!(result.price, dec!(0.44));
    }

    #[test]
    fn test_wide_spread_sell() {
        // bid=0.40, ask=0.50
        let book = create_book_with_bbo(dec!(0.40), dec!(0.50));
        let result = get_maker_price(&book, Side::Sell).unwrap();

        assert_eq!(result.spread_category, SpreadCategory::Wide);

        // price = ask - spread * 0.40 = 0.50 - 0.10 * 0.40 = 0.46
        assert_eq!(result.price, dec!(0.46));
    }

    #[test]
    fn test_wide_spread_very_wide() {
        // bid=0.20, ask=0.80
        // spread = 0.60, mid = 0.50, spread_ratio = 120% >> 3%
        let book = create_book_with_bbo(dec!(0.20), dec!(0.80));
        let result = get_maker_price(&book, Side::Buy).unwrap();

        assert_eq!(result.spread_category, SpreadCategory::Wide);

        // price = bid + spread * 0.40 = 0.20 + 0.60 * 0.40 = 0.44
        assert_eq!(result.price, dec!(0.44));
    }

    // =========================================================================
    // get_maker_price Tests - Medium Spread (1-3%)
    // =========================================================================

    #[test]
    fn test_medium_spread_buy() {
        // Need spread ratio between 1-3%
        // bid=0.49, ask=0.51 => spread=0.02, mid=0.50, ratio=4% (too high)
        // bid=0.493, ask=0.507 => spread=0.014, mid=0.50, ratio=2.8% (in range)
        let book = create_book_with_bbo(dec!(0.493), dec!(0.507));
        let result = get_maker_price(&book, Side::Buy).unwrap();

        assert_eq!(result.spread_category, SpreadCategory::Medium);
        assert_eq!(result.placement_ratio, dec!(0.30));

        // price = bid + spread * 0.30 = 0.493 + 0.014 * 0.30 = 0.4972 -> 0.50 rounded
        assert_eq!(result.price, dec!(0.50));
    }

    #[test]
    fn test_medium_spread_sell() {
        let book = create_book_with_bbo(dec!(0.493), dec!(0.507));
        let result = get_maker_price(&book, Side::Sell).unwrap();

        assert_eq!(result.spread_category, SpreadCategory::Medium);

        // price = ask - spread * 0.30 = 0.507 - 0.014 * 0.30 = 0.5028 -> 0.50 rounded
        assert_eq!(result.price, dec!(0.50));
    }

    #[test]
    fn test_medium_spread_boundary_high() {
        // Just above 1%: spread_ratio = 1.01%
        // mid = 0.50, spread_ratio = 0.0101, spread = 0.00505
        // bid = 0.50 - 0.00505/2 = 0.497475
        // ask = 0.50 + 0.00505/2 = 0.502525
        // Let's use: bid=0.4975, ask=0.5025 => spread=0.005, ratio=1%
        // That's exactly 1%, so it should still be medium
        // Try bid=0.497, ask=0.503 => spread=0.006, mid=0.50, ratio=1.2%
        let book = create_book_with_bbo(dec!(0.497), dec!(0.503));
        let result = get_maker_price(&book, Side::Buy).unwrap();

        assert_eq!(result.spread_category, SpreadCategory::Medium);
    }

    // =========================================================================
    // get_maker_price Tests - Tight Spread (<1%)
    // =========================================================================

    #[test]
    fn test_tight_spread_buy() {
        // bid=0.498, ask=0.502
        // spread = 0.004, mid = 0.50, ratio = 0.8% < 1%
        let book = create_book_with_bbo(dec!(0.498), dec!(0.502));
        let result = get_maker_price(&book, Side::Buy).unwrap();

        assert_eq!(result.spread_category, SpreadCategory::Tight);
        assert_eq!(result.placement_ratio, dec!(0.0));

        // price = bid + spread * 0 = bid = 0.498 -> 0.50 rounded
        assert_eq!(result.price, dec!(0.50));
    }

    #[test]
    fn test_tight_spread_sell() {
        let book = create_book_with_bbo(dec!(0.498), dec!(0.502));
        let result = get_maker_price(&book, Side::Sell).unwrap();

        assert_eq!(result.spread_category, SpreadCategory::Tight);

        // price = ask - spread * 0 = ask = 0.502 -> 0.50 rounded
        assert_eq!(result.price, dec!(0.50));
    }

    #[test]
    fn test_tight_spread_very_tight() {
        // bid=0.4995, ask=0.5005
        // spread = 0.001, mid = 0.50, ratio = 0.2% < 1%
        let book = create_book_with_bbo(dec!(0.4995), dec!(0.5005));
        let result = get_maker_price(&book, Side::Buy).unwrap();

        assert_eq!(result.spread_category, SpreadCategory::Tight);
        // price = 0.4995 -> 0.50 rounded
        assert_eq!(result.price, dec!(0.50));
    }

    // =========================================================================
    // get_maker_price Tests - Edge Cases
    // =========================================================================

    #[test]
    fn test_empty_book_returns_none() {
        let book = OrderBook::new("test".to_string());
        assert!(get_maker_price(&book, Side::Buy).is_none());
        assert!(get_maker_price(&book, Side::Sell).is_none());
    }

    #[test]
    fn test_no_bid_returns_none() {
        let mut book = OrderBook::new("test".to_string());
        book.asks.push(PriceLevel::new(dec!(0.50), dec!(100)));
        assert!(get_maker_price(&book, Side::Buy).is_none());
    }

    #[test]
    fn test_no_ask_returns_none() {
        let mut book = OrderBook::new("test".to_string());
        book.bids.push(PriceLevel::new(dec!(0.45), dec!(100)));
        assert!(get_maker_price(&book, Side::Buy).is_none());
    }

    #[test]
    fn test_spread_bps_calculation() {
        // bid=0.40, ask=0.50, spread=0.10, mid=0.45
        // spread_bps = (0.10 / 0.45) * 10000 = 2222
        let book = create_book_with_bbo(dec!(0.40), dec!(0.50));
        let result = get_maker_price(&book, Side::Buy).unwrap();

        assert_eq!(result.spread_bps, 2222);
    }

    #[test]
    fn test_rounding_to_cents() {
        // bid=0.412, ask=0.588
        // spread = 0.176, mid = 0.50, ratio = 35.2% > 3%
        // Buy: 0.412 + 0.176 * 0.40 = 0.412 + 0.0704 = 0.4824 -> 0.48
        let book = create_book_with_bbo(dec!(0.412), dec!(0.588));
        let result = get_maker_price(&book, Side::Buy).unwrap();

        assert_eq!(result.price, dec!(0.48));

        // Sell: 0.588 - 0.176 * 0.40 = 0.588 - 0.0704 = 0.5176 -> 0.52
        let sell_result = get_maker_price(&book, Side::Sell).unwrap();
        assert_eq!(sell_result.price, dec!(0.52));
    }

    // =========================================================================
    // get_maker_prices_both_sides Tests
    // =========================================================================

    #[test]
    fn test_both_sides() {
        let book = create_book_with_bbo(dec!(0.40), dec!(0.50));
        let (buy, sell) = get_maker_prices_both_sides(&book).unwrap();

        assert_eq!(buy.price, dec!(0.44));
        assert_eq!(sell.price, dec!(0.46));
        assert_eq!(buy.spread_category, sell.spread_category);
    }

    #[test]
    fn test_both_sides_empty_book() {
        let book = OrderBook::new("test".to_string());
        assert!(get_maker_prices_both_sides(&book).is_none());
    }

    // =========================================================================
    // is_inside_spread Tests
    // =========================================================================

    #[test]
    fn test_is_inside_spread_true() {
        let book = create_book_with_bbo(dec!(0.40), dec!(0.50));
        assert!(is_inside_spread(&book, dec!(0.45)));
        assert!(is_inside_spread(&book, dec!(0.41)));
        assert!(is_inside_spread(&book, dec!(0.49)));
    }

    #[test]
    fn test_is_inside_spread_false_at_edges() {
        let book = create_book_with_bbo(dec!(0.40), dec!(0.50));
        assert!(!is_inside_spread(&book, dec!(0.40))); // At bid
        assert!(!is_inside_spread(&book, dec!(0.50))); // At ask
        assert!(!is_inside_spread(&book, dec!(0.39))); // Below bid
        assert!(!is_inside_spread(&book, dec!(0.51))); // Above ask
    }

    #[test]
    fn test_is_inside_spread_empty_book() {
        let book = OrderBook::new("test".to_string());
        assert!(!is_inside_spread(&book, dec!(0.45)));
    }

    // =========================================================================
    // would_cross_spread Tests
    // =========================================================================

    #[test]
    fn test_would_cross_spread_buy() {
        let book = create_book_with_bbo(dec!(0.40), dec!(0.50));

        // Buy at or above ask crosses
        assert!(would_cross_spread(&book, Side::Buy, dec!(0.50)));
        assert!(would_cross_spread(&book, Side::Buy, dec!(0.55)));

        // Buy below ask doesn't cross
        assert!(!would_cross_spread(&book, Side::Buy, dec!(0.49)));
        assert!(!would_cross_spread(&book, Side::Buy, dec!(0.45)));
    }

    #[test]
    fn test_would_cross_spread_sell() {
        let book = create_book_with_bbo(dec!(0.40), dec!(0.50));

        // Sell at or below bid crosses
        assert!(would_cross_spread(&book, Side::Sell, dec!(0.40)));
        assert!(would_cross_spread(&book, Side::Sell, dec!(0.35)));

        // Sell above bid doesn't cross
        assert!(!would_cross_spread(&book, Side::Sell, dec!(0.41)));
        assert!(!would_cross_spread(&book, Side::Sell, dec!(0.45)));
    }

    #[test]
    fn test_would_cross_spread_empty_book() {
        let book = OrderBook::new("test".to_string());
        assert!(!would_cross_spread(&book, Side::Buy, dec!(0.50)));
        assert!(!would_cross_spread(&book, Side::Sell, dec!(0.40)));
    }

    // =========================================================================
    // price_improvement_bps Tests
    // =========================================================================

    #[test]
    fn test_price_improvement_buy() {
        // bid=0.40, ask=0.50, mid=0.45
        // Buy at 0.44 improves 0.04 from bid
        // improvement_bps = (0.04 / 0.45) * 10000 = 888
        let book = create_book_with_bbo(dec!(0.40), dec!(0.50));
        let improvement = price_improvement_bps(&book, Side::Buy, dec!(0.44)).unwrap();

        assert_eq!(improvement, 888);
    }

    #[test]
    fn test_price_improvement_sell() {
        // bid=0.40, ask=0.50, mid=0.45
        // Sell at 0.46 improves 0.04 from ask
        // improvement_bps = (0.04 / 0.45) * 10000 = 888
        let book = create_book_with_bbo(dec!(0.40), dec!(0.50));
        let improvement = price_improvement_bps(&book, Side::Sell, dec!(0.46)).unwrap();

        assert_eq!(improvement, 888);
    }

    #[test]
    fn test_price_improvement_zero() {
        let book = create_book_with_bbo(dec!(0.40), dec!(0.50));

        // No improvement when at reference price
        let buy_at_bid = price_improvement_bps(&book, Side::Buy, dec!(0.40)).unwrap();
        assert_eq!(buy_at_bid, 0);

        let sell_at_ask = price_improvement_bps(&book, Side::Sell, dec!(0.50)).unwrap();
        assert_eq!(sell_at_ask, 0);
    }

    #[test]
    fn test_price_improvement_empty_book() {
        let book = OrderBook::new("test".to_string());
        assert!(price_improvement_bps(&book, Side::Buy, dec!(0.45)).is_none());
        assert!(price_improvement_bps(&book, Side::Sell, dec!(0.45)).is_none());
    }

    // =========================================================================
    // MakerPriceResult Tests
    // =========================================================================

    #[test]
    fn test_maker_price_result_debug() {
        let result = MakerPriceResult {
            price: dec!(0.44),
            spread_category: SpreadCategory::Wide,
            placement_ratio: dec!(0.40),
            spread_bps: 2222,
        };

        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("0.44"));
        assert!(debug_str.contains("Wide"));
        assert!(debug_str.contains("2222"));
    }

    #[test]
    fn test_maker_price_result_equality() {
        let result1 = MakerPriceResult {
            price: dec!(0.44),
            spread_category: SpreadCategory::Wide,
            placement_ratio: dec!(0.40),
            spread_bps: 2222,
        };

        let result2 = MakerPriceResult {
            price: dec!(0.44),
            spread_category: SpreadCategory::Wide,
            placement_ratio: dec!(0.40),
            spread_bps: 2222,
        };

        assert_eq!(result1, result2);

        let result3 = MakerPriceResult {
            price: dec!(0.45),
            ..result1
        };

        assert_ne!(result1, result3);
    }

    #[test]
    fn test_maker_price_result_clone() {
        let result = MakerPriceResult {
            price: dec!(0.44),
            spread_category: SpreadCategory::Wide,
            placement_ratio: dec!(0.40),
            spread_bps: 2222,
        };

        let cloned = result;
        assert_eq!(result.price, cloned.price);
        assert_eq!(result.spread_category, cloned.spread_category);
    }
}
