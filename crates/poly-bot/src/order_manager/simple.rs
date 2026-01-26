//! Simple OrderManager - Direct forwarding to executor.
//!
//! This is the basic implementation that:
//! - Tracks pending orders per market (one at a time)
//! - Forwards directly to executor (no chasing)
//! - Manages exposure reservations

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::executor::{Executor, OrderRequest, OrderResult};

use super::{
    OrderHandle, OrderIntent, OrderManager, OrderStatus, Rejected, RejectionReason,
};

/// Configuration for SimpleOrderManager.
#[derive(Debug, Clone)]
pub struct SimpleOrderManagerConfig {
    /// Maximum exposure per market (USDC)
    pub max_market_exposure: Decimal,
    /// Maximum total exposure across all markets (USDC)
    pub max_total_exposure: Decimal,
    /// Minimum order size (USDC)
    pub min_order_size: Decimal,
    /// Default to market orders (vs limit)
    pub use_market_orders: bool,
}

impl Default for SimpleOrderManagerConfig {
    fn default() -> Self {
        Self {
            max_market_exposure: Decimal::new(1000, 0),
            max_total_exposure: Decimal::new(5000, 0),
            min_order_size: Decimal::new(5, 0),
            use_market_orders: true,
        }
    }
}

/// Active order being tracked.
#[derive(Debug, Clone)]
struct ActiveOrder {
    handle: OrderHandle,
    reserved_exposure: Decimal,
}

/// Simple OrderManager that forwards directly to executor.
///
/// Features:
/// - One pending order per market
/// - Exposure tracking and limits
/// - No chase logic (use ChasingOrderManager for that)
pub struct SimpleOrderManager<E: Executor> {
    /// Underlying executor
    executor: Arc<RwLock<E>>,
    /// Configuration
    config: SimpleOrderManagerConfig,
    /// Active orders by market ID
    active_orders: DashMap<String, ActiveOrder>,
    /// Total reserved exposure
    total_reserved: std::sync::atomic::AtomicU64,
}

impl<E: Executor> SimpleOrderManager<E> {
    /// Create a new SimpleOrderManager.
    pub fn new(executor: E, config: SimpleOrderManagerConfig) -> Self {
        Self {
            executor: Arc::new(RwLock::new(executor)),
            config,
            active_orders: DashMap::new(),
            total_reserved: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Create with default config.
    pub fn with_executor(executor: E) -> Self {
        Self::new(executor, SimpleOrderManagerConfig::default())
    }

    /// Get the executor (for direct access if needed).
    pub fn executor(&self) -> &Arc<RwLock<E>> {
        &self.executor
    }

    /// Convert OrderIntent to OrderRequest for executor.
    fn intent_to_request(&self, intent: &OrderIntent, request_id: String) -> OrderRequest {
        if self.config.use_market_orders {
            OrderRequest::market(
                request_id,
                intent.market_id.clone(),
                intent.token_id.clone(),
                intent.outcome,
                intent.side,
                intent.size,
            )
        } else {
            OrderRequest::limit(
                request_id,
                intent.market_id.clone(),
                intent.token_id.clone(),
                intent.outcome,
                intent.side,
                intent.size,
                intent.max_price,
            )
        }
    }

    /// Reserve exposure for an order.
    fn reserve_exposure(&self, amount: Decimal) {
        // Convert to u64 cents for atomic operations
        // Use trunc() to get integer value, avoiding decimal parsing issues
        let cents = (amount * Decimal::new(100, 0)).trunc().to_u64().unwrap_or(0);
        self.total_reserved.fetch_add(cents, std::sync::atomic::Ordering::SeqCst);
    }

    /// Release reserved exposure.
    fn release_exposure(&self, amount: Decimal) {
        let cents = (amount * Decimal::new(100, 0)).trunc().to_u64().unwrap_or(0);
        self.total_reserved.fetch_sub(cents, std::sync::atomic::Ordering::SeqCst);
    }

    /// Get current reserved exposure as Decimal.
    fn get_reserved_exposure(&self) -> Decimal {
        let cents = self.total_reserved.load(std::sync::atomic::Ordering::SeqCst);
        Decimal::new(cents as i64, 2)
    }

    /// Check if we can accept this order (exposure limits).
    fn check_exposure_limits(&self, intent: &OrderIntent) -> Result<(), RejectionReason> {
        let cost = intent.max_cost();

        // Check market exposure (simplified - just check cost for now)
        if cost > self.config.max_market_exposure {
            return Err(RejectionReason::MarketExposureLimit);
        }

        // Check total exposure
        let current_reserved = self.get_reserved_exposure();
        if current_reserved + cost > self.config.max_total_exposure {
            return Err(RejectionReason::TotalExposureLimit);
        }

        Ok(())
    }

    /// Convert executor OrderResult to our OrderStatus.
    fn result_to_status(result: &OrderResult) -> OrderStatus {
        match result {
            OrderResult::Filled(fill) => OrderStatus::Filled {
                filled_size: fill.size,
                avg_price: fill.price,
            },
            OrderResult::PartialFill(partial) => OrderStatus::PartialFill {
                filled_size: partial.filled_size,
                avg_price: partial.avg_price,
                remaining: partial.requested_size - partial.filled_size,
            },
            OrderResult::Rejected(rejection) => OrderStatus::Rejected {
                reason: rejection.reason.clone(),
            },
            OrderResult::Pending(_) => OrderStatus::Pending,
            OrderResult::Cancelled(_) => OrderStatus::Cancelled,
        }
    }
}

#[async_trait]
impl<E: Executor + Send + Sync + 'static> OrderManager for SimpleOrderManager<E> {
    async fn submit(&self, intent: OrderIntent) -> Result<OrderHandle, Rejected> {
        let market_id = intent.market_id.clone();

        // 1. Check if market already has pending order
        if let Some(existing) = self.active_orders.get(&market_id) {
            debug!(
                market_id = %market_id,
                existing_handle = %existing.handle.id,
                "Rejecting order - market already has pending order"
            );
            return Err(Rejected::already_pending(intent, existing.handle.clone()));
        }

        // 2. Check minimum size
        if intent.size < self.config.min_order_size {
            return Err(Rejected {
                reason: RejectionReason::BelowMinSize,
                intent,
                existing_handle: None,
            });
        }

        // 3. Check exposure limits
        if let Err(reason) = self.check_exposure_limits(&intent) {
            return Err(Rejected {
                reason,
                intent,
                existing_handle: None,
            });
        }

        // 4. Create handle and reserve exposure
        let handle = OrderHandle::new(intent.clone());
        let reserved = intent.max_cost();
        self.reserve_exposure(reserved);

        // 5. Track as active order
        let active = ActiveOrder {
            handle: handle.clone(),
            reserved_exposure: reserved,
        };
        self.active_orders.insert(market_id.clone(), active);

        // 6. Submit to executor
        let request = self.intent_to_request(&intent, handle.id.0.clone());
        let result = {
            let mut executor = self.executor.write().await;
            executor.place_order(request).await
        };

        match result {
            Ok(order_result) => {
                // Check if order completed immediately
                let status = Self::result_to_status(&order_result);
                if status.is_complete() {
                    // Clear active order and release exposure
                    self.active_orders.remove(&market_id);
                    self.release_exposure(reserved);

                    if !status.is_filled() {
                        // Order was rejected by executor
                        if let OrderStatus::Rejected { reason } = status {
                            return Err(Rejected::executor_rejected(intent, reason));
                        }
                    }
                }

                info!(
                    handle_id = %handle.id,
                    market_id = %market_id,
                    size = %intent.size,
                    "Order submitted successfully"
                );
                Ok(handle)
            }
            Err(err) => {
                // Failed to submit - clean up
                self.active_orders.remove(&market_id);
                self.release_exposure(reserved);

                warn!(
                    market_id = %market_id,
                    error = %err,
                    "Failed to submit order to executor"
                );
                Err(Rejected::executor_rejected(intent, err.to_string()))
            }
        }
    }

    fn has_pending_order(&self, market_id: &str) -> bool {
        self.active_orders.contains_key(market_id)
    }

    async fn status(&self, handle: &OrderHandle) -> OrderStatus {
        // Check if we're still tracking this order
        if let Some(active) = self.active_orders.get(&handle.market_id) {
            if active.handle.id == handle.id {
                // Still active - check with executor if we have an order ID
                if let Some(ref order_id) = handle.executor_order_id {
                    let executor = self.executor.read().await;
                    if let Some(result) = executor.order_status(order_id).await {
                        return Self::result_to_status(&result);
                    }
                }
                return OrderStatus::Working;
            }
        }
        // Not tracking - must be complete
        OrderStatus::Unknown
    }

    async fn cancel(&self, handle: &OrderHandle) -> Result<(), Rejected> {
        // Check if order exists
        if !self.active_orders.contains_key(&handle.market_id) {
            return Err(Rejected::other(
                handle.intent.clone(),
                "Order not found or already complete".to_string(),
            ));
        }

        // Try to cancel with executor
        if let Some(ref order_id) = handle.executor_order_id {
            let mut executor = self.executor.write().await;
            if let Err(err) = executor.cancel_order(order_id).await {
                warn!(
                    handle_id = %handle.id,
                    error = %err,
                    "Failed to cancel order"
                );
                return Err(Rejected::other(handle.intent.clone(), err.to_string()));
            }
        }

        // Clean up (order may still fill, but we've requested cancel)
        self.on_order_complete(handle);
        Ok(())
    }

    fn on_order_complete(&self, handle: &OrderHandle) {
        if let Some((_, active)) = self.active_orders.remove(&handle.market_id) {
            self.release_exposure(active.reserved_exposure);
            debug!(
                handle_id = %handle.id,
                market_id = %handle.market_id,
                "Order complete, cleared pending state"
            );
        }
    }

    fn reserved_exposure(&self) -> Decimal {
        self.get_reserved_exposure()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{
        Executor, ExecutorError, OrderCancellation, OrderRequest, OrderResult, PendingOrder,
    };
    use poly_common::types::{Outcome, Side};
    use rust_decimal_macros::dec;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Test executor that keeps orders pending (for testing order management).
    struct PendingExecutor {
        order_counter: AtomicU64,
        pending_orders: tokio::sync::RwLock<HashMap<String, OrderRequest>>,
        balance: Decimal,
    }

    impl PendingExecutor {
        fn new() -> Self {
            Self {
                order_counter: AtomicU64::new(0),
                pending_orders: tokio::sync::RwLock::new(HashMap::new()),
                balance: dec!(10000),
            }
        }
    }

    #[async_trait::async_trait]
    impl Executor for PendingExecutor {
        async fn place_order(&mut self, order: OrderRequest) -> Result<OrderResult, ExecutorError> {
            let order_id = format!("order-{}", self.order_counter.fetch_add(1, Ordering::SeqCst));
            self.pending_orders
                .write()
                .await
                .insert(order_id.clone(), order.clone());
            Ok(OrderResult::Pending(PendingOrder {
                request_id: order.request_id,
                order_id,
                timestamp: chrono::Utc::now(),
            }))
        }

        async fn cancel_order(&mut self, order_id: &str) -> Result<OrderCancellation, ExecutorError> {
            self.pending_orders.write().await.remove(order_id);
            Ok(OrderCancellation {
                request_id: String::new(),
                order_id: order_id.to_string(),
                filled_size: Decimal::ZERO,
                unfilled_size: Decimal::ZERO,
                timestamp: chrono::Utc::now(),
            })
        }

        async fn order_status(&self, order_id: &str) -> Option<OrderResult> {
            if self.pending_orders.read().await.contains_key(order_id) {
                Some(OrderResult::Pending(PendingOrder {
                    request_id: String::new(),
                    order_id: order_id.to_string(),
                    timestamp: chrono::Utc::now(),
                }))
            } else {
                None
            }
        }

        fn pending_orders(&self) -> Vec<PendingOrder> {
            Vec::new()
        }

        fn available_balance(&self) -> Decimal {
            self.balance
        }

        fn market_exposure(&self, _event_id: &str) -> Decimal {
            Decimal::ZERO
        }

        fn total_exposure(&self) -> Decimal {
            Decimal::ZERO
        }

        fn remaining_capacity(&self) -> Decimal {
            Decimal::MAX
        }

        fn get_position(&self, _event_id: &str) -> Option<crate::executor::PositionSnapshot> {
            None
        }

        async fn shutdown(&mut self) {}
    }

    fn create_pending_executor() -> PendingExecutor {
        PendingExecutor::new()
    }

    fn create_test_intent() -> OrderIntent {
        OrderIntent::new(
            "market-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.55),
        )
    }

    #[tokio::test]
    async fn test_submit_order() {
        let executor = create_pending_executor();
        let manager = SimpleOrderManager::with_executor(executor);

        let intent = create_test_intent();
        let result = manager.submit(intent).await;

        assert!(result.is_ok());
        let handle = result.unwrap();
        assert_eq!(handle.market_id, "market-1");
    }

    #[tokio::test]
    async fn test_reject_duplicate_order() {
        let executor = create_pending_executor();
        let manager = SimpleOrderManager::with_executor(executor);

        let intent1 = create_test_intent();
        let intent2 = create_test_intent();

        // First order should succeed
        let result1 = manager.submit(intent1).await;
        assert!(result1.is_ok());

        // Second order to same market should be rejected
        let result2 = manager.submit(intent2).await;
        assert!(result2.is_err());

        let rejected = result2.unwrap_err();
        assert!(matches!(rejected.reason, RejectionReason::AlreadyPending));
    }

    #[tokio::test]
    async fn test_has_pending_order() {
        let executor = create_pending_executor();
        let manager = SimpleOrderManager::with_executor(executor);

        assert!(!manager.has_pending_order("market-1"));

        let intent = create_test_intent();
        let _ = manager.submit(intent).await;

        assert!(manager.has_pending_order("market-1"));
        assert!(!manager.has_pending_order("market-2"));
    }

    #[tokio::test]
    async fn test_exposure_tracking() {
        let executor = create_pending_executor();
        let manager = SimpleOrderManager::with_executor(executor);

        assert_eq!(manager.reserved_exposure(), dec!(0));

        // With pending executor, exposure should be reserved until order completes
        let intent = create_test_intent();
        let handle = manager.submit(intent).await.unwrap();

        // Order is pending, exposure should be reserved (100 * 0.55 = 55)
        assert_eq!(manager.reserved_exposure(), dec!(55));

        // Complete the order - exposure should be released
        manager.on_order_complete(&handle);
        assert_eq!(manager.reserved_exposure(), dec!(0));
    }

    #[tokio::test]
    async fn test_reject_below_min_size() {
        let executor = create_pending_executor();
        let config = SimpleOrderManagerConfig {
            min_order_size: dec!(50),
            ..Default::default()
        };
        let manager = SimpleOrderManager::new(executor, config);

        let intent = OrderIntent::new(
            "market-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(10), // Below minimum
            dec!(0.55),
        );

        let result = manager.submit(intent).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err().reason, RejectionReason::BelowMinSize));
    }

    #[tokio::test]
    async fn test_on_order_complete_clears_state() {
        let executor = create_pending_executor();
        let manager = SimpleOrderManager::with_executor(executor);

        let intent = create_test_intent();
        let handle = manager.submit(intent).await.unwrap();

        assert!(manager.has_pending_order("market-1"));

        manager.on_order_complete(&handle);

        assert!(!manager.has_pending_order("market-1"));
    }
}
