//! Order Manager - Layer between Strategy and Executor
//!
//! The OrderManager handles order lifecycle management:
//! - Accepts order intents from strategy
//! - Tracks pending orders per market (prevents duplicates)
//! - Manages exposure limits
//! - Delegates actual execution to Executor
//!
//! ## Design
//!
//! Strategy submits what it wants (OrderIntent), OrderManager decides if/how to execute:
//!
//! ```text
//! Strategy → OrderIntent → OrderManager → OrderRequest → Executor
//!                              ↓
//!                         Rejection (if pending order exists)
//! ```
//!
//! ## Implementations
//!
//! - `SimpleOrderManager`: Direct forwarding to executor (no chasing)
//! - `ChasingOrderManager`: Limit orders with price chasing (future)

mod types;
mod simple;
mod tracker;
mod chasing;
mod async_manager;

pub use types::{
    OrderIntent, OrderHandle, OrderHandleId, Rejected, RejectionReason,
    OrderStatus, OrderUpdate, Urgency, OrderMetadata,
};
pub use simple::SimpleOrderManager;
pub use tracker::{PendingOrderTracker, PendingOrderType};
pub use chasing::{ChasingOrderManager, ChasingOrderManagerConfig, ExecutionResult};
pub use async_manager::{
    AsyncOrderManager, AsyncOrderManagerConfig, OrderCommand, OrderManagerTask, OrderUpdateEvent,
};

use async_trait::async_trait;

/// Order manager trait - handles order lifecycle between strategy and executor.
///
/// The OrderManager is responsible for:
/// 1. Tracking pending orders per market (one at a time)
/// 2. Rejecting duplicate orders while one is in flight
/// 3. Managing exposure reservations
/// 4. Delegating to executor for actual order placement
#[async_trait]
pub trait OrderManager: Send + Sync {
    /// Submit an order intent.
    ///
    /// Returns Ok(handle) if the order was accepted and submitted.
    /// Returns Err(Rejected) if the order cannot be processed (e.g., already pending).
    ///
    /// # Arguments
    /// * `intent` - What the strategy wants to trade
    ///
    /// # Returns
    /// * `Ok(OrderHandle)` - Order accepted, handle for tracking
    /// * `Err(Rejected)` - Order rejected with reason
    async fn submit(&self, intent: OrderIntent) -> Result<OrderHandle, Rejected>;

    /// Check if a market has a pending order.
    ///
    /// Useful for strategy to check before building expensive order intents.
    fn has_pending_order(&self, market_id: &str) -> bool;

    /// Get status of an order by handle.
    async fn status(&self, handle: &OrderHandle) -> OrderStatus;

    /// Cancel a pending order.
    ///
    /// Returns Ok(()) if cancellation was requested.
    /// The order may still fill before cancellation takes effect.
    async fn cancel(&self, handle: &OrderHandle) -> Result<(), Rejected>;

    /// Called when an order completes (filled, rejected, cancelled).
    ///
    /// This clears the pending state for the market, allowing new orders.
    fn on_order_complete(&self, handle: &OrderHandle);

    /// Get current reserved exposure across all pending orders.
    fn reserved_exposure(&self) -> rust_decimal::Decimal;
}
