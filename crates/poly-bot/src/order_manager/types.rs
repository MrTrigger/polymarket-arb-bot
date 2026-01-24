//! Types for the OrderManager layer.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use poly_common::types::{Outcome, Side};

/// Urgency level for order execution.
///
/// Affects how aggressively the OrderManager pursues fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    /// Low urgency - patient execution, prefer better price
    Low,
    /// Medium urgency - balanced (default)
    #[default]
    Medium,
    /// High urgency - aggressive execution, accept worse price
    High,
}

/// What the strategy wants to trade.
///
/// This is the strategy's intent - the OrderManager decides HOW to execute.
/// Strategy doesn't specify limit vs market, chase settings, etc.
#[derive(Debug, Clone)]
pub struct OrderIntent {
    /// Market/event ID
    pub market_id: String,
    /// Token ID to trade
    pub token_id: String,
    /// YES or NO outcome
    pub outcome: Outcome,
    /// Buy or Sell
    pub side: Side,
    /// Size to trade (in shares)
    pub size: Decimal,
    /// Maximum price willing to pay (ceiling)
    /// For buys: don't pay more than this
    /// For sells: don't accept less than this
    pub max_price: Decimal,
    /// How urgently this needs to fill
    pub urgency: Urgency,
    /// Optional metadata for tracking/observability
    pub metadata: Option<OrderMetadata>,
}

/// Optional metadata attached to order intents.
#[derive(Debug, Clone, Default)]
pub struct OrderMetadata {
    /// Which engine generated this order
    pub engine: Option<String>,
    /// Seconds remaining in market window
    pub seconds_remaining: Option<i64>,
    /// Expected profit if filled at max_price
    pub expected_profit: Option<Decimal>,
    /// Signal strength that triggered this order
    pub signal: Option<String>,
}

impl OrderIntent {
    /// Create a new order intent.
    pub fn new(
        market_id: String,
        token_id: String,
        outcome: Outcome,
        side: Side,
        size: Decimal,
        max_price: Decimal,
    ) -> Self {
        Self {
            market_id,
            token_id,
            outcome,
            side,
            size,
            max_price,
            urgency: Urgency::default(),
            metadata: None,
        }
    }

    /// Set urgency level.
    pub fn with_urgency(mut self, urgency: Urgency) -> Self {
        self.urgency = urgency;
        self
    }

    /// Set metadata.
    pub fn with_metadata(mut self, metadata: OrderMetadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Calculate maximum cost for this intent.
    pub fn max_cost(&self) -> Decimal {
        match self.side {
            Side::Buy => self.max_price * self.size,
            Side::Sell => Decimal::ZERO,
        }
    }
}

/// Unique identifier for an order handle.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrderHandleId(pub String);

impl OrderHandleId {
    /// Generate a new unique handle ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Create from existing ID string.
    pub fn from_string(id: String) -> Self {
        Self(id)
    }
}

impl Default for OrderHandleId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for OrderHandleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Handle to track a submitted order.
///
/// Returned when an OrderIntent is accepted.
#[derive(Debug, Clone)]
pub struct OrderHandle {
    /// Unique handle ID
    pub id: OrderHandleId,
    /// Market this order is for
    pub market_id: String,
    /// Token being traded
    pub token_id: String,
    /// Original intent
    pub intent: OrderIntent,
    /// When the order was submitted
    pub submitted_at: DateTime<Utc>,
    /// Underlying order ID from executor (if available)
    pub executor_order_id: Option<String>,
}

impl OrderHandle {
    /// Create a new order handle.
    pub fn new(intent: OrderIntent) -> Self {
        Self {
            id: OrderHandleId::new(),
            market_id: intent.market_id.clone(),
            token_id: intent.token_id.clone(),
            submitted_at: Utc::now(),
            executor_order_id: None,
            intent,
        }
    }

    /// Set the executor's order ID.
    pub fn with_executor_order_id(mut self, order_id: String) -> Self {
        self.executor_order_id = Some(order_id);
        self
    }
}

/// Reason an order was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectionReason {
    /// Market already has a pending order
    AlreadyPending,
    /// Would exceed market exposure limit
    MarketExposureLimit,
    /// Would exceed total exposure limit
    TotalExposureLimit,
    /// Order size below minimum
    BelowMinSize,
    /// Market is closed or not tradeable
    MarketClosed,
    /// Executor rejected the order
    ExecutorRejected(String),
    /// Other reason
    Other(String),
}

impl std::fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectionReason::AlreadyPending => write!(f, "market already has pending order"),
            RejectionReason::MarketExposureLimit => write!(f, "would exceed market exposure limit"),
            RejectionReason::TotalExposureLimit => write!(f, "would exceed total exposure limit"),
            RejectionReason::BelowMinSize => write!(f, "order size below minimum"),
            RejectionReason::MarketClosed => write!(f, "market closed"),
            RejectionReason::ExecutorRejected(msg) => write!(f, "executor rejected: {}", msg),
            RejectionReason::Other(msg) => write!(f, "{}", msg),
        }
    }
}

/// Order rejection with context.
#[derive(Debug, Clone)]
pub struct Rejected {
    /// Why the order was rejected
    pub reason: RejectionReason,
    /// The intent that was rejected
    pub intent: OrderIntent,
    /// If there's an existing order, its handle
    pub existing_handle: Option<OrderHandle>,
}

impl Rejected {
    /// Create a rejection for already pending order.
    pub fn already_pending(intent: OrderIntent, existing: OrderHandle) -> Self {
        Self {
            reason: RejectionReason::AlreadyPending,
            intent,
            existing_handle: Some(existing),
        }
    }

    /// Create a rejection for exposure limit.
    pub fn exposure_limit(intent: OrderIntent, is_market_limit: bool) -> Self {
        Self {
            reason: if is_market_limit {
                RejectionReason::MarketExposureLimit
            } else {
                RejectionReason::TotalExposureLimit
            },
            intent,
            existing_handle: None,
        }
    }

    /// Create a rejection for executor error.
    pub fn executor_rejected(intent: OrderIntent, message: String) -> Self {
        Self {
            reason: RejectionReason::ExecutorRejected(message),
            intent,
            existing_handle: None,
        }
    }

    /// Create a generic rejection.
    pub fn other(intent: OrderIntent, message: String) -> Self {
        Self {
            reason: RejectionReason::Other(message),
            intent,
            existing_handle: None,
        }
    }
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Order rejected: {}", self.reason)
    }
}

impl std::error::Error for Rejected {}

/// Current status of an order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderStatus {
    /// Order is pending execution
    Pending,
    /// Order is being actively managed (e.g., chasing)
    Working,
    /// Order was fully filled
    Filled {
        filled_size: Decimal,
        avg_price: Decimal,
    },
    /// Order was partially filled then cancelled/expired
    PartialFill {
        filled_size: Decimal,
        avg_price: Decimal,
        remaining: Decimal,
    },
    /// Order was rejected
    Rejected { reason: String },
    /// Order was cancelled
    Cancelled,
    /// Status unknown
    Unknown,
}

impl OrderStatus {
    /// Returns true if the order is still active (pending or working).
    pub fn is_active(&self) -> bool {
        matches!(self, OrderStatus::Pending | OrderStatus::Working)
    }

    /// Returns true if the order is complete (filled, rejected, or cancelled).
    pub fn is_complete(&self) -> bool {
        !self.is_active() && !matches!(self, OrderStatus::Unknown)
    }

    /// Returns true if the order was at least partially filled.
    pub fn is_filled(&self) -> bool {
        matches!(self, OrderStatus::Filled { .. } | OrderStatus::PartialFill { .. })
    }

    /// Get the filled size, if any.
    pub fn filled_size(&self) -> Option<Decimal> {
        match self {
            OrderStatus::Filled { filled_size, .. } => Some(*filled_size),
            OrderStatus::PartialFill { filled_size, .. } => Some(*filled_size),
            _ => None,
        }
    }
}

/// Update notification for order status changes.
#[derive(Debug, Clone)]
pub struct OrderUpdate {
    /// Handle of the order that changed
    pub handle: OrderHandle,
    /// Previous status
    pub previous: OrderStatus,
    /// New status
    pub current: OrderStatus,
    /// When the update occurred
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_order_intent_new() {
        let intent = OrderIntent::new(
            "market-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.55),
        );

        assert_eq!(intent.market_id, "market-1");
        assert_eq!(intent.size, dec!(100));
        assert_eq!(intent.max_price, dec!(0.55));
        assert_eq!(intent.urgency, Urgency::Medium);
    }

    #[test]
    fn test_order_intent_max_cost() {
        let buy_intent = OrderIntent::new(
            "market-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.55),
        );
        assert_eq!(buy_intent.max_cost(), dec!(55)); // 100 * 0.55

        let sell_intent = OrderIntent::new(
            "market-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Sell,
            dec!(100),
            dec!(0.55),
        );
        assert_eq!(sell_intent.max_cost(), dec!(0)); // Sells don't cost
    }

    #[test]
    fn test_order_handle_id_unique() {
        let id1 = OrderHandleId::new();
        let id2 = OrderHandleId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_order_status_is_active() {
        assert!(OrderStatus::Pending.is_active());
        assert!(OrderStatus::Working.is_active());
        assert!(!OrderStatus::Filled { filled_size: dec!(100), avg_price: dec!(0.5) }.is_active());
        assert!(!OrderStatus::Rejected { reason: "test".to_string() }.is_active());
        assert!(!OrderStatus::Cancelled.is_active());
    }

    #[test]
    fn test_order_status_is_filled() {
        assert!(OrderStatus::Filled { filled_size: dec!(100), avg_price: dec!(0.5) }.is_filled());
        assert!(OrderStatus::PartialFill {
            filled_size: dec!(50),
            avg_price: dec!(0.5),
            remaining: dec!(50),
        }.is_filled());
        assert!(!OrderStatus::Pending.is_filled());
        assert!(!OrderStatus::Cancelled.is_filled());
    }

    #[test]
    fn test_rejection_display() {
        let intent = OrderIntent::new(
            "market-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.55),
        );

        let rejected = Rejected {
            reason: RejectionReason::AlreadyPending,
            intent,
            existing_handle: None,
        };

        assert!(rejected.to_string().contains("already has pending order"));
    }
}
