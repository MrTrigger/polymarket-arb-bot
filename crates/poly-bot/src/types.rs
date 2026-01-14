//! Market state types for arbitrage detection and trading.
//!
//! CRITICAL: All prices and quantities use `rust_decimal::Decimal`.
//! NEVER use f64 for financial math.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use poly_common::types::{CryptoAsset, Outcome, Side};

// ============================================================================
// Engine and Trade Decision Types
// ============================================================================

/// Trading engine type identifier.
///
/// Each engine has a distinct strategy and priority in the multi-engine system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EngineType {
    /// Engine 1: Pure arbitrage (buy YES + NO when sum < $1.00).
    /// Highest priority - takes precedence when arb opportunity exists.
    Arbitrage,
    /// Engine 2: Directional betting based on spot vs strike.
    /// Medium priority - runs when no arb but directional signal exists.
    Directional,
    /// Engine 3: Passive maker orders for rebates.
    /// Lowest priority - background liquidity provision.
    MakerRebates,
}

impl EngineType {
    /// Get the default priority (lower number = higher priority).
    pub fn default_priority(&self) -> u8 {
        match self {
            EngineType::Arbitrage => 0,    // Highest
            EngineType::Directional => 1,  // Medium
            EngineType::MakerRebates => 2, // Lowest
        }
    }

    /// Get a short display name.
    pub fn short_name(&self) -> &'static str {
        match self {
            EngineType::Arbitrage => "ARB",
            EngineType::Directional => "DIR",
            EngineType::MakerRebates => "MKR",
        }
    }
}

impl std::fmt::Display for EngineType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineType::Arbitrage => write!(f, "Arbitrage"),
            EngineType::Directional => write!(f, "Directional"),
            EngineType::MakerRebates => write!(f, "MakerRebates"),
        }
    }
}

/// Trade decision from risk manager.
///
/// Returned by `PnlRiskManager::check_trade()` to indicate whether
/// a proposed trade is allowed and any required actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradeDecision {
    /// Trade is approved to proceed.
    Approve,
    /// Trade is rejected with a reason.
    Reject {
        /// Human-readable reason for rejection.
        reason: String,
    },
    /// Trade can proceed but with a reduced size.
    ReduceSize {
        /// Maximum allowed size in USDC.
        max_allowed: Decimal,
    },
    /// Position needs rebalancing before trading.
    RebalanceRequired {
        /// Current hedge ratio (min_side / total).
        current_ratio: Decimal,
        /// Target hedge ratio to achieve.
        target_ratio: Decimal,
    },
}

impl TradeDecision {
    /// Create a rejection with a message.
    pub fn reject(reason: impl Into<String>) -> Self {
        TradeDecision::Reject {
            reason: reason.into(),
        }
    }

    /// Check if the decision allows trading (Approve or ReduceSize).
    pub fn allows_trading(&self) -> bool {
        matches!(self, TradeDecision::Approve | TradeDecision::ReduceSize { .. })
    }

    /// Check if trading is completely blocked.
    pub fn is_blocked(&self) -> bool {
        matches!(self, TradeDecision::Reject { .. })
    }

    /// Check if rebalancing is required.
    pub fn requires_rebalance(&self) -> bool {
        matches!(self, TradeDecision::RebalanceRequired { .. })
    }
}

impl std::fmt::Display for TradeDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TradeDecision::Approve => write!(f, "Approve"),
            TradeDecision::Reject { reason } => write!(f, "Reject: {}", reason),
            TradeDecision::ReduceSize { max_allowed } => {
                write!(f, "ReduceSize(max=${})", max_allowed)
            }
            TradeDecision::RebalanceRequired {
                current_ratio,
                target_ratio,
            } => {
                write!(
                    f,
                    "RebalanceRequired({}% -> {}%)",
                    current_ratio * Decimal::ONE_HUNDRED,
                    target_ratio * Decimal::ONE_HUNDRED
                )
            }
        }
    }
}

// ============================================================================
// Position Tracking
// ============================================================================

/// Position tracking for a single market.
///
/// Tracks UP (YES) and DOWN (NO) shares with their cost basis
/// for P&L calculation and hedge ratio management.
///
/// Unlike `Inventory` which is more general, `Position` is specifically
/// designed for the three-engine strategy with directional betting.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Position {
    /// UP (YES) shares held.
    pub up_shares: Decimal,
    /// DOWN (NO) shares held.
    pub down_shares: Decimal,
    /// Total cost paid for UP shares.
    pub up_cost: Decimal,
    /// Total cost paid for DOWN shares.
    pub down_cost: Decimal,
}

impl Position {
    /// Create a new empty position.
    pub fn new() -> Self {
        Self::default()
    }

    /// Total cost basis of the position.
    #[inline]
    pub fn total_cost(&self) -> Decimal {
        self.up_cost + self.down_cost
    }

    /// Total shares held (UP + DOWN).
    #[inline]
    pub fn total_shares(&self) -> Decimal {
        self.up_shares + self.down_shares
    }

    /// UP allocation ratio (0.0 to 1.0).
    ///
    /// Returns the fraction of total cost allocated to UP.
    /// Returns 0.5 if no position exists.
    #[inline]
    pub fn up_ratio(&self) -> Decimal {
        let total = self.total_cost();
        if total <= Decimal::ZERO {
            Decimal::new(5, 1) // 0.5 default
        } else {
            self.up_cost / total
        }
    }

    /// DOWN allocation ratio (0.0 to 1.0).
    #[inline]
    pub fn down_ratio(&self) -> Decimal {
        Decimal::ONE - self.up_ratio()
    }

    /// Minimum side ratio (hedge ratio).
    ///
    /// Returns min(up_ratio, down_ratio). A ratio of 0.5 means
    /// perfectly balanced, lower values indicate more directional exposure.
    #[inline]
    pub fn min_side_ratio(&self) -> Decimal {
        self.up_ratio().min(self.down_ratio())
    }

    /// Check if the position has any holdings.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.total_shares() <= Decimal::ZERO
    }

    /// Average price paid for UP shares.
    pub fn up_avg_price(&self) -> Option<Decimal> {
        if self.up_shares > Decimal::ZERO {
            Some(self.up_cost / self.up_shares)
        } else {
            None
        }
    }

    /// Average price paid for DOWN shares.
    pub fn down_avg_price(&self) -> Option<Decimal> {
        if self.down_shares > Decimal::ZERO {
            Some(self.down_cost / self.down_shares)
        } else {
            None
        }
    }

    /// Add UP shares to the position.
    pub fn add_up(&mut self, shares: Decimal, cost: Decimal) {
        self.up_shares += shares;
        self.up_cost += cost;
    }

    /// Add DOWN shares to the position.
    pub fn add_down(&mut self, shares: Decimal, cost: Decimal) {
        self.down_shares += shares;
        self.down_cost += cost;
    }

    /// Calculate P&L at settlement.
    ///
    /// # Arguments
    ///
    /// * `up_wins` - true if UP (YES) wins, false if DOWN (NO) wins
    ///
    /// # Returns
    ///
    /// The profit (positive) or loss (negative) in USDC.
    /// Winner pays $1.00 per share, loser pays $0.00.
    pub fn calculate_pnl(&self, up_wins: bool) -> Decimal {
        if up_wins {
            // UP shares pay $1.00 each, DOWN shares pay $0.00
            let settlement_value = self.up_shares * Decimal::ONE;
            settlement_value - self.total_cost()
        } else {
            // DOWN shares pay $1.00 each, UP shares pay $0.00
            let settlement_value = self.down_shares * Decimal::ONE;
            settlement_value - self.total_cost()
        }
    }

    /// Calculate guaranteed P&L from matched pairs.
    ///
    /// Matched pairs (min of UP and DOWN shares) always pay $1.00
    /// regardless of outcome.
    pub fn guaranteed_pnl(&self) -> Decimal {
        let matched_pairs = self.up_shares.min(self.down_shares);
        if matched_pairs <= Decimal::ZERO {
            return Decimal::ZERO;
        }

        // Calculate proportional cost for matched pairs
        let total_shares = self.total_shares();
        if total_shares <= Decimal::ZERO {
            return Decimal::ZERO;
        }

        let matched_up_cost = self.up_cost * (matched_pairs / self.up_shares.max(Decimal::ONE));
        let matched_down_cost =
            self.down_cost * (matched_pairs / self.down_shares.max(Decimal::ONE));
        let matched_cost = matched_up_cost + matched_down_cost;

        // Each matched pair pays $1.00 at settlement
        matched_pairs - matched_cost
    }

    /// Calculate unrealized P&L at current prices.
    ///
    /// # Arguments
    ///
    /// * `up_price` - Current UP (YES) price (0.0 to 1.0)
    /// * `down_price` - Current DOWN (NO) price (0.0 to 1.0)
    pub fn unrealized_pnl(&self, up_price: Decimal, down_price: Decimal) -> Decimal {
        let current_value = (self.up_shares * up_price) + (self.down_shares * down_price);
        current_value - self.total_cost()
    }

    /// Reset the position to empty.
    pub fn reset(&mut self) {
        self.up_shares = Decimal::ZERO;
        self.down_shares = Decimal::ZERO;
        self.up_cost = Decimal::ZERO;
        self.down_cost = Decimal::ZERO;
    }

    /// Convert to Inventory type for compatibility.
    pub fn to_inventory(&self, event_id: String) -> Inventory {
        Inventory {
            event_id,
            yes_shares: self.up_shares,
            no_shares: self.down_shares,
            yes_cost_basis: self.up_cost,
            no_cost_basis: self.down_cost,
            realized_pnl: Decimal::ZERO,
        }
    }

    /// Create from an Inventory.
    pub fn from_inventory(inv: &Inventory) -> Self {
        Self {
            up_shares: inv.yes_shares,
            down_shares: inv.no_shares,
            up_cost: inv.yes_cost_basis,
            down_cost: inv.no_cost_basis,
        }
    }
}

// ============================================================================
// Order Book Types
// ============================================================================

/// A single price level in an order book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceLevel {
    /// Price (0.00 to 1.00 for Polymarket).
    pub price: Decimal,
    /// Size available at this price.
    pub size: Decimal,
}

impl PriceLevel {
    /// Create a new price level.
    pub fn new(price: Decimal, size: Decimal) -> Self {
        Self { price, size }
    }

    /// Total cost to fill this level.
    #[inline]
    pub fn cost(&self) -> Decimal {
        self.price * self.size
    }
}

/// Full order book with multiple price levels.
///
/// Unlike `LiveOrderBook` (BBO-only), this maintains the full depth
/// for more accurate fill simulation and sizing calculations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    /// Token ID this book represents.
    pub token_id: String,
    /// Bid levels sorted by price descending (best bid first).
    pub bids: Vec<PriceLevel>,
    /// Ask levels sorted by price ascending (best ask first).
    pub asks: Vec<PriceLevel>,
    /// Last update timestamp (milliseconds since epoch).
    pub last_update_ms: i64,
}

impl OrderBook {
    /// Create an empty order book.
    pub fn new(token_id: String) -> Self {
        Self {
            token_id,
            bids: Vec::new(),
            asks: Vec::new(),
            last_update_ms: 0,
        }
    }

    /// Best bid price (None if no bids).
    #[inline]
    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.first().map(|l| l.price)
    }

    /// Best ask price (None if no asks).
    #[inline]
    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.first().map(|l| l.price)
    }

    /// Best bid size.
    #[inline]
    pub fn best_bid_size(&self) -> Option<Decimal> {
        self.bids.first().map(|l| l.size)
    }

    /// Best ask size.
    #[inline]
    pub fn best_ask_size(&self) -> Option<Decimal> {
        self.asks.first().map(|l| l.size)
    }

    /// Spread in basis points.
    pub fn spread_bps(&self) -> Option<u32> {
        let bid = self.best_bid()?;
        let ask = self.best_ask()?;
        if bid <= Decimal::ZERO || ask <= Decimal::ZERO {
            return None;
        }
        let spread = ask - bid;
        let mid = (bid + ask) / Decimal::TWO;
        if mid <= Decimal::ZERO {
            return None;
        }
        let bps = (spread * Decimal::new(10000, 0)) / mid;
        Some(bps.try_into().unwrap_or(u32::MAX))
    }

    /// Mid price.
    pub fn mid_price(&self) -> Option<Decimal> {
        let bid = self.best_bid()?;
        let ask = self.best_ask()?;
        Some((bid + ask) / Decimal::TWO)
    }

    /// Check if the book has valid BBO (both bid and ask present).
    pub fn is_valid(&self) -> bool {
        self.best_bid().is_some() && self.best_ask().is_some()
    }

    /// Total bid depth (sum of all bid sizes).
    pub fn bid_depth(&self) -> Decimal {
        self.bids.iter().map(|l| l.size).sum()
    }

    /// Total ask depth (sum of all ask sizes).
    pub fn ask_depth(&self) -> Decimal {
        self.asks.iter().map(|l| l.size).sum()
    }

    /// Calculate cost to buy `size` shares by walking the ask book.
    ///
    /// Returns (shares_filled, total_cost, avg_price).
    pub fn cost_to_buy(&self, target_size: Decimal) -> (Decimal, Decimal, Option<Decimal>) {
        let mut remaining = target_size;
        let mut total_cost = Decimal::ZERO;
        let mut filled = Decimal::ZERO;

        for level in &self.asks {
            if remaining <= Decimal::ZERO {
                break;
            }
            let fill_size = remaining.min(level.size);
            total_cost += fill_size * level.price;
            filled += fill_size;
            remaining -= fill_size;
        }

        let avg_price = if filled > Decimal::ZERO {
            Some(total_cost / filled)
        } else {
            None
        };

        (filled, total_cost, avg_price)
    }

    /// Calculate proceeds from selling `size` shares by walking the bid book.
    ///
    /// Returns (shares_filled, total_proceeds, avg_price).
    pub fn proceeds_to_sell(&self, target_size: Decimal) -> (Decimal, Decimal, Option<Decimal>) {
        let mut remaining = target_size;
        let mut total_proceeds = Decimal::ZERO;
        let mut filled = Decimal::ZERO;

        for level in &self.bids {
            if remaining <= Decimal::ZERO {
                break;
            }
            let fill_size = remaining.min(level.size);
            total_proceeds += fill_size * level.price;
            filled += fill_size;
            remaining -= fill_size;
        }

        let avg_price = if filled > Decimal::ZERO {
            Some(total_proceeds / filled)
        } else {
            None
        };

        (filled, total_proceeds, avg_price)
    }

    /// Apply a book snapshot (replace all levels).
    pub fn apply_snapshot(&mut self, bids: Vec<PriceLevel>, asks: Vec<PriceLevel>, timestamp_ms: i64) {
        self.bids = bids;
        self.asks = asks;
        self.last_update_ms = timestamp_ms;
        self.sort_levels();
    }

    /// Apply a delta update to a single level.
    ///
    /// If size is zero, the level is removed.
    pub fn apply_delta(&mut self, side: Side, price: Decimal, size: Decimal, timestamp_ms: i64) {
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };

        // Remove existing level at this price
        levels.retain(|l| l.price != price);

        // Add new level if size > 0
        if size > Decimal::ZERO {
            levels.push(PriceLevel::new(price, size));
        }

        self.last_update_ms = timestamp_ms;
        self.sort_levels();
    }

    /// Sort bid/ask levels properly.
    fn sort_levels(&mut self) {
        // Bids: highest price first
        self.bids.sort_by(|a, b| b.price.cmp(&a.price));
        // Asks: lowest price first
        self.asks.sort_by(|a, b| a.price.cmp(&b.price));
    }
}

impl Default for OrderBook {
    fn default() -> Self {
        Self::new(String::new())
    }
}

/// Combined market state for a YES/NO pair.
///
/// Aggregates both sides of a binary market and calculates
/// derived fields for arbitrage detection.
#[derive(Debug, Clone)]
pub struct MarketState {
    /// Event ID for this market.
    pub event_id: String,
    /// Asset this market tracks.
    pub asset: CryptoAsset,
    /// YES side order book.
    pub yes_book: OrderBook,
    /// NO side order book.
    pub no_book: OrderBook,
    /// Current spot price (from Binance).
    pub spot_price: Option<Decimal>,
    /// Strike price for the up/down market.
    pub strike_price: Decimal,
    /// Seconds remaining in the window.
    pub seconds_remaining: i64,
}

impl MarketState {
    /// Create a new market state.
    pub fn new(
        event_id: String,
        asset: CryptoAsset,
        yes_token_id: String,
        no_token_id: String,
        strike_price: Decimal,
        seconds_remaining: i64,
    ) -> Self {
        Self {
            event_id,
            asset,
            yes_book: OrderBook::new(yes_token_id),
            no_book: OrderBook::new(no_token_id),
            spot_price: None,
            strike_price,
            seconds_remaining,
        }
    }

    /// Combined cost to buy one unit of YES + one unit of NO.
    ///
    /// For arbitrage, this should be < $1.00.
    /// Returns None if either book is missing valid quotes.
    pub fn combined_cost(&self) -> Option<Decimal> {
        let yes_ask = self.yes_book.best_ask()?;
        let no_ask = self.no_book.best_ask()?;
        Some(yes_ask + no_ask)
    }

    /// Arbitrage margin = 1.0 - combined_cost.
    ///
    /// Positive margin means profit opportunity.
    /// Returns None if combined cost cannot be calculated.
    pub fn arb_margin(&self) -> Option<Decimal> {
        let combined = self.combined_cost()?;
        Some(Decimal::ONE - combined)
    }

    /// Arbitrage margin in basis points.
    pub fn arb_margin_bps(&self) -> Option<i32> {
        let margin = self.arb_margin()?;
        let bps = margin * Decimal::new(10000, 0);
        Some(bps.try_into().unwrap_or(i32::MAX))
    }

    /// Check if both order books are valid (have BBO).
    pub fn is_valid(&self) -> bool {
        self.yes_book.is_valid() && self.no_book.is_valid()
    }

    /// Get the maximum size that can be arbitraged.
    ///
    /// Limited by the smaller of YES ask size and NO ask size.
    pub fn max_arb_size(&self) -> Option<Decimal> {
        let yes_size = self.yes_book.best_ask_size()?;
        let no_size = self.no_book.best_ask_size()?;
        Some(yes_size.min(no_size))
    }

    /// Calculate total cost to buy `size` shares of both YES and NO.
    ///
    /// Returns (yes_filled, no_filled, total_cost).
    pub fn cost_to_arb(&self, size: Decimal) -> (Decimal, Decimal, Decimal) {
        let (yes_filled, yes_cost, _) = self.yes_book.cost_to_buy(size);
        let (no_filled, no_cost, _) = self.no_book.cost_to_buy(size);
        (yes_filled, no_filled, yes_cost + no_cost)
    }

    /// Check if spot price is above strike (YES should win).
    pub fn spot_above_strike(&self) -> Option<bool> {
        self.spot_price.map(|s| s > self.strike_price)
    }

    /// Implied probability from YES ask price.
    pub fn implied_yes_prob(&self) -> Option<Decimal> {
        self.yes_book.best_ask()
    }

    /// Implied probability from NO ask price.
    pub fn implied_no_prob(&self) -> Option<Decimal> {
        self.no_book.best_ask()
    }
}

/// Inventory position for a single market.
///
/// Tracks shares held and cost basis for P&L calculation.
/// Re-exported from state.rs for backwards compatibility.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    /// Event ID.
    pub event_id: String,
    /// YES shares held.
    pub yes_shares: Decimal,
    /// NO shares held.
    pub no_shares: Decimal,
    /// Cost basis for YES shares.
    pub yes_cost_basis: Decimal,
    /// Cost basis for NO shares.
    pub no_cost_basis: Decimal,
    /// Realized P&L from closed positions.
    pub realized_pnl: Decimal,
}

impl Inventory {
    /// Create a new empty inventory.
    pub fn new(event_id: String) -> Self {
        Self {
            event_id,
            yes_shares: Decimal::ZERO,
            no_shares: Decimal::ZERO,
            yes_cost_basis: Decimal::ZERO,
            no_cost_basis: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
        }
    }

    /// Total shares held (YES + NO).
    pub fn total_shares(&self) -> Decimal {
        self.yes_shares + self.no_shares
    }

    /// Total exposure (cost basis of both sides).
    pub fn total_exposure(&self) -> Decimal {
        self.yes_cost_basis + self.no_cost_basis
    }

    /// Net position (YES - NO shares).
    pub fn net_position(&self) -> Decimal {
        self.yes_shares - self.no_shares
    }

    /// Imbalance ratio (0.0 = balanced, 1.0 = fully one-sided).
    pub fn imbalance_ratio(&self) -> Decimal {
        let total = self.total_shares();
        if total <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let max_side = self.yes_shares.max(self.no_shares);
        let min_side = self.yes_shares.min(self.no_shares);
        (max_side - min_side) / total
    }

    /// Get inventory state classification.
    pub fn state(&self) -> InventoryState {
        let ratio = self.imbalance_ratio();
        if ratio <= Decimal::new(2, 1) {
            // <= 0.2
            InventoryState::Balanced
        } else if ratio <= Decimal::new(5, 1) {
            // <= 0.5
            InventoryState::Skewed
        } else if ratio <= Decimal::new(8, 1) {
            // <= 0.8
            InventoryState::Exposed
        } else {
            InventoryState::Crisis
        }
    }

    /// Matched pairs (min of YES and NO shares).
    ///
    /// Each matched pair guarantees $1.00 at settlement.
    pub fn matched_pairs(&self) -> Decimal {
        self.yes_shares.min(self.no_shares)
    }

    /// Unmatched YES shares (exposure to YES outcome).
    pub fn unmatched_yes(&self) -> Decimal {
        (self.yes_shares - self.no_shares).max(Decimal::ZERO)
    }

    /// Unmatched NO shares (exposure to NO outcome).
    pub fn unmatched_no(&self) -> Decimal {
        (self.no_shares - self.yes_shares).max(Decimal::ZERO)
    }

    /// Add YES shares.
    pub fn add_yes(&mut self, shares: Decimal, cost: Decimal) {
        self.yes_shares += shares;
        self.yes_cost_basis += cost;
    }

    /// Add NO shares.
    pub fn add_no(&mut self, shares: Decimal, cost: Decimal) {
        self.no_shares += shares;
        self.no_cost_basis += cost;
    }

    /// Record a fill for a specific outcome.
    pub fn record_fill(&mut self, outcome: Outcome, shares: Decimal, cost: Decimal) {
        match outcome {
            Outcome::Yes => self.add_yes(shares, cost),
            Outcome::No => self.add_no(shares, cost),
        }
    }

    /// Calculate unrealized P&L given current prices.
    ///
    /// Assumes settlement pays $1.00 for winning side.
    pub fn unrealized_pnl(&self, yes_price: Decimal, no_price: Decimal) -> Decimal {
        // Value at current prices
        let yes_value = self.yes_shares * yes_price;
        let no_value = self.no_shares * no_price;
        let current_value = yes_value + no_value;

        // Cost basis
        let total_cost = self.yes_cost_basis + self.no_cost_basis;

        current_value - total_cost
    }

    /// Calculate P&L at settlement (YES wins).
    pub fn pnl_if_yes_wins(&self) -> Decimal {
        // YES pays $1.00, NO pays $0.00
        let settlement_value = self.yes_shares * Decimal::ONE;
        let total_cost = self.yes_cost_basis + self.no_cost_basis;
        settlement_value - total_cost + self.realized_pnl
    }

    /// Calculate P&L at settlement (NO wins).
    pub fn pnl_if_no_wins(&self) -> Decimal {
        // NO pays $1.00, YES pays $0.00
        let settlement_value = self.no_shares * Decimal::ONE;
        let total_cost = self.yes_cost_basis + self.no_cost_basis;
        settlement_value - total_cost + self.realized_pnl
    }

    /// Guaranteed P&L from matched pairs.
    ///
    /// If we hold N pairs, we get N * $1.00 regardless of outcome.
    pub fn guaranteed_pnl(&self) -> Decimal {
        let pairs = self.matched_pairs();
        let pair_cost = if pairs > Decimal::ZERO {
            // Proportional cost basis for matched pairs
            let yes_ratio = pairs / self.yes_shares.max(Decimal::ONE);
            let no_ratio = pairs / self.no_shares.max(Decimal::ONE);
            (self.yes_cost_basis * yes_ratio) + (self.no_cost_basis * no_ratio)
        } else {
            Decimal::ZERO
        };
        pairs - pair_cost
    }
}

/// Inventory state classification.
///
/// Used to adjust position sizing and risk limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InventoryState {
    /// Well-balanced YES/NO position (imbalance <= 0.2).
    Balanced,
    /// Slightly imbalanced (0.2 < imbalance <= 0.5).
    Skewed,
    /// Significantly imbalanced, should reduce (0.5 < imbalance <= 0.8).
    Exposed,
    /// Critical imbalance, must hedge immediately (imbalance > 0.8).
    Crisis,
}

impl InventoryState {
    /// Size multiplier for this state.
    ///
    /// Reduces position size as inventory becomes more imbalanced.
    pub fn size_multiplier(&self) -> Decimal {
        match self {
            InventoryState::Balanced => Decimal::ONE,
            InventoryState::Skewed => Decimal::new(75, 2),   // 0.75
            InventoryState::Exposed => Decimal::new(50, 2),  // 0.50
            InventoryState::Crisis => Decimal::new(25, 2),   // 0.25
        }
    }

    /// Whether new positions should prefer the opposite side.
    pub fn should_rebalance(&self) -> bool {
        matches!(self, InventoryState::Exposed | InventoryState::Crisis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_price_level() {
        let level = PriceLevel::new(dec!(0.45), dec!(100));
        assert_eq!(level.price, dec!(0.45));
        assert_eq!(level.size, dec!(100));
        assert_eq!(level.cost(), dec!(45));
    }

    #[test]
    fn test_order_book_bbo() {
        let mut book = OrderBook::new("token123".to_string());
        assert!(!book.is_valid());
        assert!(book.best_bid().is_none());
        assert!(book.best_ask().is_none());

        book.bids.push(PriceLevel::new(dec!(0.45), dec!(100)));
        book.bids.push(PriceLevel::new(dec!(0.44), dec!(200)));
        book.asks.push(PriceLevel::new(dec!(0.55), dec!(150)));
        book.asks.push(PriceLevel::new(dec!(0.56), dec!(250)));
        book.sort_levels();

        assert!(book.is_valid());
        assert_eq!(book.best_bid(), Some(dec!(0.45)));
        assert_eq!(book.best_ask(), Some(dec!(0.55)));
        assert_eq!(book.bid_depth(), dec!(300));
        assert_eq!(book.ask_depth(), dec!(400));
    }

    #[test]
    fn test_order_book_spread() {
        let mut book = OrderBook::new("test".to_string());
        book.bids.push(PriceLevel::new(dec!(0.45), dec!(100)));
        book.asks.push(PriceLevel::new(dec!(0.55), dec!(100)));

        // spread = 0.10, mid = 0.50
        // bps = (0.10 / 0.50) * 10000 = 2000
        assert_eq!(book.spread_bps(), Some(2000));
        assert_eq!(book.mid_price(), Some(dec!(0.50)));
    }

    #[test]
    fn test_cost_to_buy() {
        let mut book = OrderBook::new("test".to_string());
        book.asks.push(PriceLevel::new(dec!(0.50), dec!(100)));
        book.asks.push(PriceLevel::new(dec!(0.51), dec!(100)));
        book.asks.push(PriceLevel::new(dec!(0.52), dec!(100)));

        // Buy 150 shares: 100 @ 0.50 + 50 @ 0.51 = 50 + 25.5 = 75.5
        let (filled, cost, avg) = book.cost_to_buy(dec!(150));
        assert_eq!(filled, dec!(150));
        assert_eq!(cost, dec!(75.5));
        // avg = 75.5 / 150 = 0.5033...
        assert!(avg.unwrap() > dec!(0.503) && avg.unwrap() < dec!(0.504));
    }

    #[test]
    fn test_proceeds_to_sell() {
        let mut book = OrderBook::new("test".to_string());
        book.bids.push(PriceLevel::new(dec!(0.50), dec!(100)));
        book.bids.push(PriceLevel::new(dec!(0.49), dec!(100)));
        book.sort_levels();

        // Sell 150 shares: 100 @ 0.50 + 50 @ 0.49 = 50 + 24.5 = 74.5
        let (filled, proceeds, avg) = book.proceeds_to_sell(dec!(150));
        assert_eq!(filled, dec!(150));
        assert_eq!(proceeds, dec!(74.5));
        assert!(avg.unwrap() > dec!(0.496) && avg.unwrap() < dec!(0.497));
    }

    #[test]
    fn test_apply_delta() {
        let mut book = OrderBook::new("test".to_string());
        book.bids.push(PriceLevel::new(dec!(0.45), dec!(100)));
        book.sort_levels();

        // Update existing level
        book.apply_delta(Side::Buy, dec!(0.45), dec!(150), 1000);
        assert_eq!(book.bids.len(), 1);
        assert_eq!(book.bids[0].size, dec!(150));

        // Add new level
        book.apply_delta(Side::Buy, dec!(0.46), dec!(50), 1001);
        assert_eq!(book.bids.len(), 2);
        assert_eq!(book.best_bid(), Some(dec!(0.46)));

        // Remove level
        book.apply_delta(Side::Buy, dec!(0.46), dec!(0), 1002);
        assert_eq!(book.bids.len(), 1);
        assert_eq!(book.best_bid(), Some(dec!(0.45)));
    }

    #[test]
    fn test_market_state_arb_margin() {
        let mut state = MarketState::new(
            "event1".to_string(),
            CryptoAsset::Btc,
            "yes_token".to_string(),
            "no_token".to_string(),
            dec!(100000),
            300,
        );

        // No valid quotes yet
        assert!(state.arb_margin().is_none());

        // Add quotes
        state.yes_book.asks.push(PriceLevel::new(dec!(0.45), dec!(100)));
        state.no_book.asks.push(PriceLevel::new(dec!(0.52), dec!(100)));

        // combined = 0.45 + 0.52 = 0.97
        // margin = 1.0 - 0.97 = 0.03 (3%)
        assert_eq!(state.combined_cost(), Some(dec!(0.97)));
        assert_eq!(state.arb_margin(), Some(dec!(0.03)));
        assert_eq!(state.arb_margin_bps(), Some(300));
    }

    #[test]
    fn test_market_state_max_arb_size() {
        let mut state = MarketState::new(
            "event1".to_string(),
            CryptoAsset::Btc,
            "yes_token".to_string(),
            "no_token".to_string(),
            dec!(100000),
            300,
        );

        state.yes_book.asks.push(PriceLevel::new(dec!(0.45), dec!(100)));
        state.no_book.asks.push(PriceLevel::new(dec!(0.52), dec!(50)));

        // Limited by smaller side
        assert_eq!(state.max_arb_size(), Some(dec!(50)));
    }

    #[test]
    fn test_inventory_basics() {
        let mut inv = Inventory::new("event1".to_string());
        assert_eq!(inv.total_shares(), Decimal::ZERO);
        assert_eq!(inv.imbalance_ratio(), Decimal::ZERO);
        assert_eq!(inv.state(), InventoryState::Balanced);

        inv.add_yes(dec!(100), dec!(45));
        inv.add_no(dec!(100), dec!(52));

        assert_eq!(inv.total_shares(), dec!(200));
        assert_eq!(inv.total_exposure(), dec!(97));
        assert_eq!(inv.imbalance_ratio(), Decimal::ZERO);
        assert_eq!(inv.state(), InventoryState::Balanced);
    }

    #[test]
    fn test_inventory_imbalance() {
        let mut inv = Inventory::new("event1".to_string());

        // Skewed: 70 YES, 30 NO => imbalance = 0.4
        inv.yes_shares = dec!(70);
        inv.no_shares = dec!(30);
        assert_eq!(inv.imbalance_ratio(), dec!(0.4));
        assert_eq!(inv.state(), InventoryState::Skewed);

        // Exposed: 85 YES, 15 NO => imbalance = 0.7
        inv.yes_shares = dec!(85);
        inv.no_shares = dec!(15);
        assert_eq!(inv.imbalance_ratio(), dec!(0.7));
        assert_eq!(inv.state(), InventoryState::Exposed);

        // Crisis: 95 YES, 5 NO => imbalance = 0.9
        inv.yes_shares = dec!(95);
        inv.no_shares = dec!(5);
        assert_eq!(inv.imbalance_ratio(), dec!(0.9));
        assert_eq!(inv.state(), InventoryState::Crisis);
    }

    #[test]
    fn test_inventory_matched_pairs() {
        let mut inv = Inventory::new("event1".to_string());
        inv.yes_shares = dec!(100);
        inv.no_shares = dec!(75);

        assert_eq!(inv.matched_pairs(), dec!(75));
        assert_eq!(inv.unmatched_yes(), dec!(25));
        assert_eq!(inv.unmatched_no(), dec!(0));
    }

    #[test]
    fn test_inventory_pnl() {
        let mut inv = Inventory::new("event1".to_string());
        inv.add_yes(dec!(100), dec!(45));    // 100 YES @ 0.45 = $45
        inv.add_no(dec!(100), dec!(52));     // 100 NO @ 0.52 = $52
        // Total cost = $97

        // If YES wins: get 100 * $1.00 = $100, profit = $100 - $97 = $3
        assert_eq!(inv.pnl_if_yes_wins(), dec!(3));

        // If NO wins: get 100 * $1.00 = $100, profit = $100 - $97 = $3
        assert_eq!(inv.pnl_if_no_wins(), dec!(3));

        // Matched pairs = 100, guaranteed $100 - $97 = $3
        assert_eq!(inv.guaranteed_pnl(), dec!(3));
    }

    #[test]
    fn test_inventory_unrealized_pnl() {
        let mut inv = Inventory::new("event1".to_string());
        inv.add_yes(dec!(100), dec!(45));
        inv.add_no(dec!(100), dec!(52));

        // Current prices: YES=0.48, NO=0.50
        // Value = 100*0.48 + 100*0.50 = 98
        // Cost = 97
        // Unrealized P&L = 1
        let pnl = inv.unrealized_pnl(dec!(0.48), dec!(0.50));
        assert_eq!(pnl, dec!(1));
    }

    #[test]
    fn test_inventory_state_multipliers() {
        assert_eq!(InventoryState::Balanced.size_multiplier(), Decimal::ONE);
        assert_eq!(InventoryState::Skewed.size_multiplier(), dec!(0.75));
        assert_eq!(InventoryState::Exposed.size_multiplier(), dec!(0.50));
        assert_eq!(InventoryState::Crisis.size_multiplier(), dec!(0.25));

        assert!(!InventoryState::Balanced.should_rebalance());
        assert!(!InventoryState::Skewed.should_rebalance());
        assert!(InventoryState::Exposed.should_rebalance());
        assert!(InventoryState::Crisis.should_rebalance());
    }

    #[test]
    fn test_record_fill() {
        let mut inv = Inventory::new("event1".to_string());

        inv.record_fill(Outcome::Yes, dec!(100), dec!(45));
        assert_eq!(inv.yes_shares, dec!(100));
        assert_eq!(inv.yes_cost_basis, dec!(45));

        inv.record_fill(Outcome::No, dec!(100), dec!(52));
        assert_eq!(inv.no_shares, dec!(100));
        assert_eq!(inv.no_cost_basis, dec!(52));
    }

    // =========================================================================
    // EngineType Tests
    // =========================================================================

    #[test]
    fn test_engine_type_priority() {
        assert_eq!(EngineType::Arbitrage.default_priority(), 0);
        assert_eq!(EngineType::Directional.default_priority(), 1);
        assert_eq!(EngineType::MakerRebates.default_priority(), 2);

        // Arbitrage should have highest priority (lowest number)
        assert!(EngineType::Arbitrage.default_priority() < EngineType::Directional.default_priority());
        assert!(EngineType::Directional.default_priority() < EngineType::MakerRebates.default_priority());
    }

    #[test]
    fn test_engine_type_short_name() {
        assert_eq!(EngineType::Arbitrage.short_name(), "ARB");
        assert_eq!(EngineType::Directional.short_name(), "DIR");
        assert_eq!(EngineType::MakerRebates.short_name(), "MKR");
    }

    #[test]
    fn test_engine_type_display() {
        assert_eq!(format!("{}", EngineType::Arbitrage), "Arbitrage");
        assert_eq!(format!("{}", EngineType::Directional), "Directional");
        assert_eq!(format!("{}", EngineType::MakerRebates), "MakerRebates");
    }

    #[test]
    fn test_engine_type_serialization() {
        let arb = EngineType::Arbitrage;
        let json = serde_json::to_string(&arb).unwrap();
        assert_eq!(json, "\"Arbitrage\"");

        let parsed: EngineType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, EngineType::Arbitrage);
    }

    // =========================================================================
    // TradeDecision Tests
    // =========================================================================

    #[test]
    fn test_trade_decision_approve() {
        let decision = TradeDecision::Approve;
        assert!(decision.allows_trading());
        assert!(!decision.is_blocked());
        assert!(!decision.requires_rebalance());
        assert_eq!(format!("{}", decision), "Approve");
    }

    #[test]
    fn test_trade_decision_reject() {
        let decision = TradeDecision::reject("Daily loss limit hit");
        assert!(!decision.allows_trading());
        assert!(decision.is_blocked());
        assert!(!decision.requires_rebalance());

        if let TradeDecision::Reject { reason } = &decision {
            assert_eq!(reason, "Daily loss limit hit");
        } else {
            panic!("Expected Reject variant");
        }

        assert!(format!("{}", decision).contains("Daily loss limit hit"));
    }

    #[test]
    fn test_trade_decision_reduce_size() {
        let decision = TradeDecision::ReduceSize {
            max_allowed: dec!(25.50),
        };
        assert!(decision.allows_trading());
        assert!(!decision.is_blocked());
        assert!(!decision.requires_rebalance());

        if let TradeDecision::ReduceSize { max_allowed } = &decision {
            assert_eq!(*max_allowed, dec!(25.50));
        } else {
            panic!("Expected ReduceSize variant");
        }

        assert!(format!("{}", decision).contains("25.5"));
    }

    #[test]
    fn test_trade_decision_rebalance_required() {
        let decision = TradeDecision::RebalanceRequired {
            current_ratio: dec!(0.15),
            target_ratio: dec!(0.20),
        };
        assert!(!decision.allows_trading());
        assert!(!decision.is_blocked());
        assert!(decision.requires_rebalance());

        if let TradeDecision::RebalanceRequired {
            current_ratio,
            target_ratio,
        } = &decision
        {
            assert_eq!(*current_ratio, dec!(0.15));
            assert_eq!(*target_ratio, dec!(0.20));
        } else {
            panic!("Expected RebalanceRequired variant");
        }
    }

    #[test]
    fn test_trade_decision_serialization() {
        let decision = TradeDecision::Approve;
        let json = serde_json::to_string(&decision).unwrap();
        let parsed: TradeDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, TradeDecision::Approve);

        let decision = TradeDecision::ReduceSize {
            max_allowed: dec!(100),
        };
        let json = serde_json::to_string(&decision).unwrap();
        let parsed: TradeDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, decision);
    }

    // =========================================================================
    // Position Tests
    // =========================================================================

    #[test]
    fn test_position_new() {
        let pos = Position::new();
        assert_eq!(pos.up_shares, Decimal::ZERO);
        assert_eq!(pos.down_shares, Decimal::ZERO);
        assert_eq!(pos.up_cost, Decimal::ZERO);
        assert_eq!(pos.down_cost, Decimal::ZERO);
        assert!(pos.is_empty());
    }

    #[test]
    fn test_position_total_cost() {
        let pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(100),
            up_cost: dec!(45),
            down_cost: dec!(52),
        };
        assert_eq!(pos.total_cost(), dec!(97));
    }

    #[test]
    fn test_position_total_shares() {
        let pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(75),
            up_cost: dec!(0),
            down_cost: dec!(0),
        };
        assert_eq!(pos.total_shares(), dec!(175));
    }

    #[test]
    fn test_position_up_ratio() {
        // Empty position defaults to 0.5
        let empty = Position::new();
        assert_eq!(empty.up_ratio(), dec!(0.5));

        // 60% UP, 40% DOWN by cost
        let pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(100),
            up_cost: dec!(60),
            down_cost: dec!(40),
        };
        assert_eq!(pos.up_ratio(), dec!(0.6));
        assert_eq!(pos.down_ratio(), dec!(0.4));
    }

    #[test]
    fn test_position_min_side_ratio() {
        // Balanced: 50/50
        let balanced = Position {
            up_shares: dec!(100),
            down_shares: dec!(100),
            up_cost: dec!(50),
            down_cost: dec!(50),
        };
        assert_eq!(balanced.min_side_ratio(), dec!(0.5));

        // Skewed: 70/30
        let skewed = Position {
            up_shares: dec!(100),
            down_shares: dec!(100),
            up_cost: dec!(70),
            down_cost: dec!(30),
        };
        assert_eq!(skewed.min_side_ratio(), dec!(0.3));
    }

    #[test]
    fn test_position_add_up_down() {
        let mut pos = Position::new();

        pos.add_up(dec!(100), dec!(45));
        assert_eq!(pos.up_shares, dec!(100));
        assert_eq!(pos.up_cost, dec!(45));

        pos.add_down(dec!(100), dec!(52));
        assert_eq!(pos.down_shares, dec!(100));
        assert_eq!(pos.down_cost, dec!(52));

        // Add more
        pos.add_up(dec!(50), dec!(25));
        assert_eq!(pos.up_shares, dec!(150));
        assert_eq!(pos.up_cost, dec!(70));
    }

    #[test]
    fn test_position_avg_price() {
        let pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(200),
            up_cost: dec!(45),   // avg = 0.45
            down_cost: dec!(100), // avg = 0.50
        };
        assert_eq!(pos.up_avg_price(), Some(dec!(0.45)));
        assert_eq!(pos.down_avg_price(), Some(dec!(0.50)));

        // Empty side returns None
        let empty_up = Position {
            up_shares: Decimal::ZERO,
            down_shares: dec!(100),
            up_cost: Decimal::ZERO,
            down_cost: dec!(50),
        };
        assert_eq!(empty_up.up_avg_price(), None);
        assert_eq!(empty_up.down_avg_price(), Some(dec!(0.50)));
    }

    #[test]
    fn test_position_calculate_pnl_up_wins() {
        // Buy 100 UP @ 0.45, 100 DOWN @ 0.52
        // Total cost = $97
        // If UP wins: 100 * $1.00 = $100
        // P&L = $100 - $97 = $3
        let pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(100),
            up_cost: dec!(45),
            down_cost: dec!(52),
        };
        assert_eq!(pos.calculate_pnl(true), dec!(3));
    }

    #[test]
    fn test_position_calculate_pnl_down_wins() {
        // Buy 100 UP @ 0.45, 100 DOWN @ 0.52
        // Total cost = $97
        // If DOWN wins: 100 * $1.00 = $100
        // P&L = $100 - $97 = $3
        let pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(100),
            up_cost: dec!(45),
            down_cost: dec!(52),
        };
        assert_eq!(pos.calculate_pnl(false), dec!(3));
    }

    #[test]
    fn test_position_calculate_pnl_unbalanced() {
        // Buy 150 UP @ 0.60, 50 DOWN @ 0.40
        // Total cost = $90 + $20 = $110
        // If UP wins: 150 * $1.00 = $150, P&L = $40
        // If DOWN wins: 50 * $1.00 = $50, P&L = -$60
        let pos = Position {
            up_shares: dec!(150),
            down_shares: dec!(50),
            up_cost: dec!(90),
            down_cost: dec!(20),
        };
        assert_eq!(pos.calculate_pnl(true), dec!(40));
        assert_eq!(pos.calculate_pnl(false), dec!(-60));
    }

    #[test]
    fn test_position_guaranteed_pnl() {
        // Buy 100 UP @ 0.45, 100 DOWN @ 0.52
        // Matched pairs = 100
        // Settlement value = $100
        // Cost for matched = $45 + $52 = $97
        // Guaranteed P&L = $100 - $97 = $3
        let pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(100),
            up_cost: dec!(45),
            down_cost: dec!(52),
        };
        assert_eq!(pos.guaranteed_pnl(), dec!(3));
    }

    #[test]
    fn test_position_guaranteed_pnl_unbalanced() {
        // Buy 100 UP @ 0.45, 50 DOWN @ 0.52
        // Matched pairs = 50
        // Settlement for matched = $50
        // Cost for matched UP = 45 * (50/100) = $22.50
        // Cost for matched DOWN = 52 * (50/50) = $52 -- wait, 50 down shares matched
        // Actually: matched_down_cost = 52 * (50/50) = $52
        // Total matched cost = $22.50 + $52 = $74.50
        // Wait, down_shares is 50, so matched_pairs = 50
        // matched_down_cost = down_cost * (matched_pairs / down_shares) = 52 * (50/50) = 52
        // Hmm, that seems wrong. Let me recalculate.
        // If we have 50 DOWN shares and match all 50:
        // matched_up_cost = up_cost * (50 / up_shares) = 45 * (50/100) = 22.50
        // matched_down_cost = down_cost * (50 / down_shares) = down_cost * (50/50) = down_cost = 26 (if 50 shares cost 26)
        // Let's use a clearer example
        let pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(50),
            up_cost: dec!(50),  // avg 0.50
            down_cost: dec!(25), // avg 0.50
        };
        // Matched pairs = 50
        // matched_up_cost = 50 * (50/100) = 25
        // matched_down_cost = 25 * (50/50) = 25
        // Total matched cost = 50
        // Settlement = 50
        // Guaranteed P&L = 0
        assert_eq!(pos.guaranteed_pnl(), dec!(0));
    }

    #[test]
    fn test_position_unrealized_pnl() {
        let pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(100),
            up_cost: dec!(45),
            down_cost: dec!(52),
        };
        // Cost = 97
        // Current value at (0.48, 0.50) = 48 + 50 = 98
        // Unrealized P&L = 1
        assert_eq!(pos.unrealized_pnl(dec!(0.48), dec!(0.50)), dec!(1));
    }

    #[test]
    fn test_position_reset() {
        let mut pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(100),
            up_cost: dec!(45),
            down_cost: dec!(52),
        };
        pos.reset();
        assert!(pos.is_empty());
        assert_eq!(pos.total_cost(), Decimal::ZERO);
    }

    #[test]
    fn test_position_inventory_conversion() {
        let pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(75),
            up_cost: dec!(45),
            down_cost: dec!(35),
        };

        let inv = pos.to_inventory("event123".to_string());
        assert_eq!(inv.event_id, "event123");
        assert_eq!(inv.yes_shares, dec!(100));
        assert_eq!(inv.no_shares, dec!(75));
        assert_eq!(inv.yes_cost_basis, dec!(45));
        assert_eq!(inv.no_cost_basis, dec!(35));
        assert_eq!(inv.realized_pnl, Decimal::ZERO);

        // Convert back
        let pos2 = Position::from_inventory(&inv);
        assert_eq!(pos2.up_shares, pos.up_shares);
        assert_eq!(pos2.down_shares, pos.down_shares);
        assert_eq!(pos2.up_cost, pos.up_cost);
        assert_eq!(pos2.down_cost, pos.down_cost);
    }

    #[test]
    fn test_position_serialization() {
        let pos = Position {
            up_shares: dec!(100),
            down_shares: dec!(75),
            up_cost: dec!(45),
            down_cost: dec!(35),
        };

        let json = serde_json::to_string(&pos).unwrap();
        let parsed: Position = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.up_shares, pos.up_shares);
        assert_eq!(parsed.down_shares, pos.down_shares);
        assert_eq!(parsed.up_cost, pos.up_cost);
        assert_eq!(parsed.down_cost, pos.down_cost);
    }
}
