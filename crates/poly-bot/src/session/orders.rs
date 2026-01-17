//! Order lifecycle tracking with proper state machine.
//!
//! Orders go through a well-defined lifecycle:
//!
//! ```text
//! Created → Pending → Filled (full or partial)
//!                  → Cancelled
//!                  → Rejected
//!                  → Expired
//! ```
//!
//! Budget is reserved when an order is created and released when
//! the order reaches a terminal state (filled, cancelled, rejected, expired).

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

use poly_common::types::{CryptoAsset, Outcome, Side};

/// Order lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderState {
    /// Order created but not yet sent to exchange.
    Created,
    /// Order sent to exchange, awaiting fill.
    Pending,
    /// Order fully filled.
    Filled,
    /// Order partially filled (some quantity remains).
    PartialFill,
    /// Order cancelled by user or system.
    Cancelled,
    /// Order rejected by exchange.
    Rejected,
    /// Order expired (timeout).
    Expired,
}

impl OrderState {
    /// Check if this is a terminal state (no further transitions possible).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OrderState::Filled | OrderState::Cancelled | OrderState::Rejected | OrderState::Expired
        )
    }

    /// Check if budget should be reserved for this state.
    pub fn reserves_budget(&self) -> bool {
        matches!(
            self,
            OrderState::Created | OrderState::Pending | OrderState::PartialFill
        )
    }
}

impl std::fmt::Display for OrderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderState::Created => write!(f, "created"),
            OrderState::Pending => write!(f, "pending"),
            OrderState::Filled => write!(f, "filled"),
            OrderState::PartialFill => write!(f, "partial"),
            OrderState::Cancelled => write!(f, "cancelled"),
            OrderState::Rejected => write!(f, "rejected"),
            OrderState::Expired => write!(f, "expired"),
        }
    }
}

/// Which engine originated this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderSource {
    /// Arbitrage engine (YES+NO pair).
    Arbitrage,
    /// Directional engine (biased allocation).
    Directional,
    /// Market maker engine (passive liquidity).
    Maker,
    /// Manual/test order.
    Manual,
}

impl std::fmt::Display for OrderSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderSource::Arbitrage => write!(f, "arb"),
            OrderSource::Directional => write!(f, "dir"),
            OrderSource::Maker => write!(f, "maker"),
            OrderSource::Manual => write!(f, "manual"),
        }
    }
}

/// A tracked order with full lifecycle information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedOrder {
    /// Unique order ID (internal).
    pub order_id: String,
    /// Exchange order ID (once submitted).
    pub exchange_order_id: Option<String>,
    /// Market event ID.
    pub event_id: String,
    /// Token ID (YES or NO token).
    pub token_id: String,
    /// Asset being traded.
    pub asset: CryptoAsset,
    /// YES or NO outcome.
    pub outcome: Outcome,
    /// Buy or Sell.
    pub side: Side,
    /// Requested size (USDC value).
    pub requested_size: Decimal,
    /// Requested price.
    pub requested_price: Decimal,
    /// Filled size so far (USDC value).
    pub filled_size: Decimal,
    /// Filled quantity (shares).
    pub filled_quantity: Decimal,
    /// Average fill price.
    pub avg_fill_price: Decimal,
    /// Total fees paid.
    pub fees: Decimal,
    /// Current state.
    pub state: OrderState,
    /// Source engine.
    pub source: OrderSource,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Fill timestamp (if filled).
    pub filled_at: Option<DateTime<Utc>>,
    /// Rejection/cancellation reason (if applicable).
    pub error_reason: Option<String>,
}

impl TrackedOrder {
    /// Create a new order in Created state.
    pub fn new(
        order_id: String,
        event_id: String,
        token_id: String,
        asset: CryptoAsset,
        outcome: Outcome,
        side: Side,
        requested_size: Decimal,
        requested_price: Decimal,
        source: OrderSource,
    ) -> Self {
        let now = Utc::now();
        Self {
            order_id,
            exchange_order_id: None,
            event_id,
            token_id,
            asset,
            outcome,
            side,
            requested_size,
            requested_price,
            filled_size: Decimal::ZERO,
            filled_quantity: Decimal::ZERO,
            avg_fill_price: Decimal::ZERO,
            fees: Decimal::ZERO,
            state: OrderState::Created,
            source,
            created_at: now,
            updated_at: now,
            filled_at: None,
            error_reason: None,
        }
    }

    /// Transition to Pending state (order submitted to exchange).
    pub fn mark_pending(&mut self, exchange_order_id: String) {
        self.exchange_order_id = Some(exchange_order_id);
        self.state = OrderState::Pending;
        self.updated_at = Utc::now();
    }

    /// Record a fill (partial or complete).
    pub fn record_fill(&mut self, quantity: Decimal, price: Decimal, fee: Decimal) {
        let fill_cost = quantity * price;

        // Update weighted average price
        let total_quantity = self.filled_quantity + quantity;
        if total_quantity > Decimal::ZERO {
            self.avg_fill_price = (self.filled_quantity * self.avg_fill_price + quantity * price)
                / total_quantity;
        }

        self.filled_quantity += quantity;
        self.filled_size += fill_cost;
        self.fees += fee;
        self.updated_at = Utc::now();

        // Check if fully filled
        if self.filled_size >= self.requested_size {
            self.state = OrderState::Filled;
            self.filled_at = Some(Utc::now());
        } else {
            self.state = OrderState::PartialFill;
        }
    }

    /// Mark order as cancelled.
    pub fn mark_cancelled(&mut self, reason: Option<String>) {
        self.state = OrderState::Cancelled;
        self.error_reason = reason;
        self.updated_at = Utc::now();
    }

    /// Mark order as rejected.
    pub fn mark_rejected(&mut self, reason: String) {
        self.state = OrderState::Rejected;
        self.error_reason = Some(reason);
        self.updated_at = Utc::now();
    }

    /// Mark order as expired.
    pub fn mark_expired(&mut self) {
        self.state = OrderState::Expired;
        self.updated_at = Utc::now();
    }

    /// Get the unfilled size (budget still reserved).
    pub fn unfilled_size(&self) -> Decimal {
        (self.requested_size - self.filled_size).max(Decimal::ZERO)
    }

    /// Check if order is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// Get latency from creation to fill (if filled).
    pub fn fill_latency_ms(&self) -> Option<i64> {
        self.filled_at.map(|filled| (filled - self.created_at).num_milliseconds())
    }
}

/// Centralized order tracking across all engines.
///
/// Thread-safe via DashMap for concurrent access.
pub struct OrderTracker {
    /// All orders by internal order ID.
    orders: DashMap<String, TrackedOrder>,
    /// Order ID counter for generating unique IDs.
    order_counter: AtomicU64,
    /// Total budget reserved for pending orders.
    reserved_budget: DashMap<String, Decimal>, // per event_id
}

impl OrderTracker {
    /// Create a new order tracker.
    pub fn new() -> Self {
        Self {
            orders: DashMap::new(),
            order_counter: AtomicU64::new(1),
            reserved_budget: DashMap::new(),
        }
    }

    /// Generate a unique order ID.
    pub fn next_order_id(&self, source: OrderSource) -> String {
        let counter = self.order_counter.fetch_add(1, Ordering::SeqCst);
        format!("{}-{}", source, counter)
    }

    /// Create and track a new order.
    ///
    /// Returns the order ID and reserves budget for the market.
    pub fn create_order(
        &self,
        event_id: String,
        token_id: String,
        asset: CryptoAsset,
        outcome: Outcome,
        side: Side,
        requested_size: Decimal,
        requested_price: Decimal,
        source: OrderSource,
    ) -> String {
        let order_id = self.next_order_id(source);
        let order = TrackedOrder::new(
            order_id.clone(),
            event_id.clone(),
            token_id,
            asset,
            outcome,
            side,
            requested_size,
            requested_price,
            source,
        );

        // Reserve budget for this market
        self.reserved_budget
            .entry(event_id)
            .and_modify(|b| *b += requested_size)
            .or_insert(requested_size);

        self.orders.insert(order_id.clone(), order);
        order_id
    }

    /// Mark an order as pending (submitted to exchange).
    pub fn mark_pending(&self, order_id: &str, exchange_order_id: String) {
        if let Some(mut order) = self.orders.get_mut(order_id) {
            order.mark_pending(exchange_order_id);
        }
    }

    /// Record a fill for an order.
    ///
    /// Returns the filled size for position tracking.
    pub fn record_fill(
        &self,
        order_id: &str,
        quantity: Decimal,
        price: Decimal,
        fee: Decimal,
    ) -> Option<(String, Outcome, Decimal, Decimal)> {
        if let Some(mut order) = self.orders.get_mut(order_id) {
            let event_id = order.event_id.clone();
            let outcome = order.outcome;
            let prev_filled = order.filled_size;

            order.record_fill(quantity, price, fee);

            let new_filled = order.filled_size - prev_filled;

            // If terminal, release remaining reserved budget
            if order.is_terminal() {
                let unfilled = order.unfilled_size();
                if unfilled > Decimal::ZERO {
                    if let Some(mut reserved) = self.reserved_budget.get_mut(&event_id) {
                        *reserved = (*reserved - unfilled).max(Decimal::ZERO);
                    }
                }
            }

            Some((event_id, outcome, quantity, new_filled))
        } else {
            None
        }
    }

    /// Mark an order as cancelled and release reserved budget.
    pub fn mark_cancelled(&self, order_id: &str, reason: Option<String>) {
        if let Some(mut order) = self.orders.get_mut(order_id) {
            let event_id = order.event_id.clone();
            let unfilled = order.unfilled_size();

            order.mark_cancelled(reason);

            // Release reserved budget
            if unfilled > Decimal::ZERO {
                if let Some(mut reserved) = self.reserved_budget.get_mut(&event_id) {
                    *reserved = (*reserved - unfilled).max(Decimal::ZERO);
                }
            }
        }
    }

    /// Mark an order as rejected and release reserved budget.
    pub fn mark_rejected(&self, order_id: &str, reason: String) {
        if let Some(mut order) = self.orders.get_mut(order_id) {
            let event_id = order.event_id.clone();
            let unfilled = order.unfilled_size();

            order.mark_rejected(reason);

            // Release reserved budget
            if unfilled > Decimal::ZERO {
                if let Some(mut reserved) = self.reserved_budget.get_mut(&event_id) {
                    *reserved = (*reserved - unfilled).max(Decimal::ZERO);
                }
            }
        }
    }

    /// Get an order by ID.
    pub fn get_order(&self, order_id: &str) -> Option<TrackedOrder> {
        self.orders.get(order_id).map(|r| r.clone())
    }

    /// Get all pending orders for a market.
    pub fn pending_orders(&self, event_id: &str) -> Vec<TrackedOrder> {
        self.orders
            .iter()
            .filter(|r| {
                r.event_id == event_id
                    && matches!(
                        r.state,
                        OrderState::Created | OrderState::Pending | OrderState::PartialFill
                    )
            })
            .map(|r| r.clone())
            .collect()
    }

    /// Get reserved budget for a market.
    pub fn reserved_budget(&self, event_id: &str) -> Decimal {
        self.reserved_budget
            .get(event_id)
            .map(|r| *r)
            .unwrap_or(Decimal::ZERO)
    }

    /// Get total reserved budget across all markets.
    pub fn total_reserved_budget(&self) -> Decimal {
        self.reserved_budget.iter().map(|r| *r).sum()
    }

    /// Count orders by state.
    pub fn count_by_state(&self, state: OrderState) -> usize {
        self.orders.iter().filter(|r| r.state == state).count()
    }

    /// Get all filled orders.
    pub fn filled_orders(&self) -> Vec<TrackedOrder> {
        self.orders
            .iter()
            .filter(|r| r.state == OrderState::Filled)
            .map(|r| r.clone())
            .collect()
    }

    /// Clear all orders (for session reset).
    pub fn clear(&self) {
        self.orders.clear();
        self.reserved_budget.clear();
    }

    /// Get summary statistics.
    pub fn summary(&self) -> OrderSummary {
        let mut total_orders = 0u64;
        let mut pending = 0u64;
        let mut filled = 0u64;
        let mut cancelled = 0u64;
        let mut rejected = 0u64;
        let mut total_volume = Decimal::ZERO;
        let mut total_fees = Decimal::ZERO;

        for order in self.orders.iter() {
            total_orders += 1;
            match order.state {
                OrderState::Created | OrderState::Pending | OrderState::PartialFill => pending += 1,
                OrderState::Filled => {
                    filled += 1;
                    total_volume += order.filled_size;
                    total_fees += order.fees;
                }
                OrderState::Cancelled => cancelled += 1,
                OrderState::Rejected | OrderState::Expired => rejected += 1,
            }
        }

        OrderSummary {
            total_orders,
            pending,
            filled,
            cancelled,
            rejected,
            total_volume,
            total_fees,
            reserved_budget: self.total_reserved_budget(),
        }
    }
}

impl Default for OrderTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Summary of order statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderSummary {
    pub total_orders: u64,
    pub pending: u64,
    pub filled: u64,
    pub cancelled: u64,
    pub rejected: u64,
    pub total_volume: Decimal,
    pub total_fees: Decimal,
    pub reserved_budget: Decimal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_order_lifecycle() {
        let tracker = OrderTracker::new();

        // Create order
        let order_id = tracker.create_order(
            "event1".to_string(),
            "token1".to_string(),
            CryptoAsset::Btc,
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.50),
            OrderSource::Directional,
        );

        // Check reserved budget
        assert_eq!(tracker.reserved_budget("event1"), dec!(100));

        // Mark pending
        tracker.mark_pending(&order_id, "exch-123".to_string());
        let order = tracker.get_order(&order_id).unwrap();
        assert_eq!(order.state, OrderState::Pending);

        // Record partial fill
        tracker.record_fill(&order_id, dec!(50), dec!(0.50), dec!(0.25));
        let order = tracker.get_order(&order_id).unwrap();
        assert_eq!(order.state, OrderState::PartialFill);
        assert_eq!(order.filled_quantity, dec!(50));

        // Record remaining fill
        tracker.record_fill(&order_id, dec!(150), dec!(0.50), dec!(0.75));
        let order = tracker.get_order(&order_id).unwrap();
        assert_eq!(order.state, OrderState::Filled);

        // Budget should be released
        assert_eq!(tracker.reserved_budget("event1"), dec!(0));
    }

    #[test]
    fn test_order_cancellation() {
        let tracker = OrderTracker::new();

        let order_id = tracker.create_order(
            "event1".to_string(),
            "token1".to_string(),
            CryptoAsset::Btc,
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.50),
            OrderSource::Directional,
        );

        assert_eq!(tracker.reserved_budget("event1"), dec!(100));

        tracker.mark_cancelled(&order_id, Some("user requested".to_string()));

        let order = tracker.get_order(&order_id).unwrap();
        assert_eq!(order.state, OrderState::Cancelled);
        assert_eq!(tracker.reserved_budget("event1"), dec!(0));
    }

    #[test]
    fn test_order_summary() {
        let tracker = OrderTracker::new();

        // Create and fill one order
        let order_id = tracker.create_order(
            "event1".to_string(),
            "token1".to_string(),
            CryptoAsset::Btc,
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.50),
            OrderSource::Directional,
        );
        tracker.record_fill(&order_id, dec!(200), dec!(0.50), dec!(1));

        // Create a pending order
        tracker.create_order(
            "event1".to_string(),
            "token2".to_string(),
            CryptoAsset::Btc,
            Outcome::No,
            Side::Buy,
            dec!(50),
            dec!(0.50),
            OrderSource::Directional,
        );

        let summary = tracker.summary();
        assert_eq!(summary.total_orders, 2);
        assert_eq!(summary.filled, 1);
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.total_volume, dec!(100));
        assert_eq!(summary.total_fees, dec!(1));
        assert_eq!(summary.reserved_budget, dec!(50));
    }
}
