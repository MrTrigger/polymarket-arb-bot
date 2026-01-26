//! Chasing OrderManager - Limit orders with price chasing.
//!
//! This implementation adds price chasing capability on top of order management:
//! - Tracks pending orders per market (one at a time)
//! - For limit orders, uses PriceChaser to bump prices until filled
//! - Supports market orders as immediate execution fallback
//! - Integrates with PendingOrderTracker for consistent pending state

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::executor::{
    Executor, ExecutorError, OrderRequest, OrderResult, OrderType,
};
use crate::executor::chase::{ChaseConfig, ChaseResult, PriceChaser};

use super::{
    OrderHandle, OrderIntent, OrderManager, OrderStatus, PendingOrderTracker, PendingOrderType,
    Rejected, RejectionReason,
};

/// Configuration for ChasingOrderManager.
#[derive(Debug, Clone)]
pub struct ChasingOrderManagerConfig {
    /// Maximum exposure per market (USDC).
    pub max_market_exposure: Decimal,
    /// Maximum total exposure across all markets (USDC).
    pub max_total_exposure: Decimal,
    /// Minimum order size (USDC).
    pub min_order_size: Decimal,
    /// Use market orders (IOC) instead of limit with chase.
    pub use_market_orders: bool,
    /// Chase configuration (when use_market_orders is false).
    pub chase_config: ChaseConfig,
    /// Maximum POST_ONLY retries before giving up.
    pub max_post_only_retries: u32,
}

impl Default for ChasingOrderManagerConfig {
    fn default() -> Self {
        Self {
            max_market_exposure: Decimal::new(1000, 0),
            max_total_exposure: Decimal::new(5000, 0),
            min_order_size: Decimal::new(5, 0),
            use_market_orders: false,
            chase_config: ChaseConfig::default(),
            max_post_only_retries: 3,
        }
    }
}

/// Result of an order submission through ChasingOrderManager.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// The order handle.
    pub handle: OrderHandle,
    /// Final status after execution attempt.
    pub status: OrderStatus,
    /// If chasing was used, the chase result details.
    pub chase_result: Option<ChaseResult>,
}

/// Chasing OrderManager with price chase support.
///
/// Features:
/// - One pending order per market (prevents duplicates)
/// - Limit orders with automatic price chasing
/// - Market order fallback for urgent execution
/// - Integrated exposure tracking
pub struct ChasingOrderManager<E: Executor> {
    /// Underlying executor.
    executor: Arc<RwLock<E>>,
    /// Configuration.
    config: ChasingOrderManagerConfig,
    /// Pending order tracker (shared with strategy if needed).
    pending_tracker: PendingOrderTracker,
    /// Price chaser for limit order execution.
    chaser: PriceChaser,
    /// Total reserved exposure (atomic for performance).
    total_reserved: std::sync::atomic::AtomicU64,
}

impl<E: Executor> ChasingOrderManager<E> {
    /// Create a new ChasingOrderManager.
    pub fn new(executor: E, config: ChasingOrderManagerConfig) -> Self {
        let chaser = PriceChaser::new(config.chase_config.clone());
        Self {
            executor: Arc::new(RwLock::new(executor)),
            config,
            pending_tracker: PendingOrderTracker::new(),
            chaser,
            total_reserved: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Create with default config.
    pub fn with_executor(executor: E) -> Self {
        Self::new(executor, ChasingOrderManagerConfig::default())
    }

    /// Get reference to the pending tracker (for external checks if needed).
    pub fn pending_tracker(&self) -> &PendingOrderTracker {
        &self.pending_tracker
    }

    /// Get the executor (for operations not going through OrderManager).
    pub fn executor(&self) -> &Arc<RwLock<E>> {
        &self.executor
    }

    /// Reserve exposure for an order.
    fn reserve_exposure(&self, amount: Decimal) {
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

    /// Check exposure limits.
    fn check_exposure_limits(&self, intent: &OrderIntent) -> Result<(), RejectionReason> {
        let cost = intent.max_cost();

        if cost > self.config.max_market_exposure {
            return Err(RejectionReason::MarketExposureLimit);
        }

        let current_reserved = self.get_reserved_exposure();
        if current_reserved + cost > self.config.max_total_exposure {
            return Err(RejectionReason::TotalExposureLimit);
        }

        Ok(())
    }

    /// Convert OrderIntent to OrderRequest.
    fn intent_to_request(&self, intent: &OrderIntent, request_id: String) -> OrderRequest {
        let order_type = if self.config.use_market_orders {
            OrderType::Ioc // Immediate-or-cancel for market-like behavior
        } else {
            OrderType::Gtc // Good-til-cancelled for limit with chase
        };

        OrderRequest {
            request_id,
            event_id: intent.market_id.clone(),
            token_id: intent.token_id.clone(),
            outcome: intent.outcome,
            side: intent.side,
            size: intent.size,
            price: Some(intent.max_price),
            order_type,
            timeout_ms: None,
            timestamp: Utc::now(),
        }
    }

    /// Map pending order type from intent.
    fn order_type_from_intent(intent: &OrderIntent) -> PendingOrderType {
        // Default to Directional - in future could use metadata to distinguish
        if let Some(ref meta) = intent.metadata {
            if meta.engine.as_deref() == Some("arbitrage") {
                return PendingOrderType::Arbitrage;
            }
            if meta.engine.as_deref() == Some("maker") {
                return PendingOrderType::Maker;
            }
        }
        PendingOrderType::Directional
    }

    /// Execute an order with chasing (internal implementation).
    async fn execute_with_chase(
        &self,
        intent: &OrderIntent,
        handle: &OrderHandle,
        other_leg_price: Decimal,
    ) -> Result<ExecutionResult, ExecutorError> {
        let request = self.intent_to_request(intent, handle.id.0.clone());

        // For market orders, just place once
        if self.config.use_market_orders {
            let mut executor = self.executor.write().await;
            let result = executor.place_order(request).await?;
            return Ok(self.result_to_execution(handle.clone(), &result, None));
        }

        // For limit orders, use chaser
        let mut executor = self.executor.write().await;
        let chase_result = self.chaser.chase_order(&mut *executor, request, other_leg_price).await?;

        // Convert chase result to execution result
        let status = if chase_result.success {
            OrderStatus::Filled {
                filled_size: chase_result.filled_size,
                avg_price: chase_result.avg_price,
            }
        } else if chase_result.filled_size > Decimal::ZERO {
            OrderStatus::PartialFill {
                filled_size: chase_result.filled_size,
                avg_price: chase_result.avg_price,
                remaining: intent.size - chase_result.filled_size,
            }
        } else {
            let reason = chase_result.stop_reason
                .map(|r| format!("{:?}", r))
                .unwrap_or_else(|| "Chase failed".to_string());
            OrderStatus::Rejected { reason }
        };

        Ok(ExecutionResult {
            handle: handle.clone(),
            status,
            chase_result: Some(chase_result),
        })
    }

    /// Convert OrderResult to ExecutionResult.
    fn result_to_execution(
        &self,
        handle: OrderHandle,
        result: &OrderResult,
        chase_result: Option<ChaseResult>,
    ) -> ExecutionResult {
        let status = match result {
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
        };

        ExecutionResult {
            handle,
            status,
            chase_result,
        }
    }

    /// Submit an order with optional other-leg price for ceiling calculation.
    ///
    /// For directional trades, other_leg_price is the opposite outcome's price.
    /// For single-leg trades, pass Decimal::ZERO (no ceiling enforcement).
    pub async fn submit_with_ceiling(
        &self,
        intent: OrderIntent,
        other_leg_price: Decimal,
    ) -> Result<ExecutionResult, Rejected> {
        let market_id = intent.market_id.clone();
        let order_type = Self::order_type_from_intent(&intent);

        // 1. Check for pending order
        if self.pending_tracker.is_pending(&market_id, order_type) {
            debug!(
                market_id = %market_id,
                "Rejecting order - market already has pending order"
            );
            return Err(Rejected {
                reason: RejectionReason::AlreadyPending,
                intent,
                existing_handle: None,
            });
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

        // 4. Create handle and reserve
        let handle = OrderHandle::new(intent.clone());
        let reserved = intent.max_cost();
        self.reserve_exposure(reserved);
        self.pending_tracker.register(&market_id, order_type);

        // 5. Execute with chase
        let result = self.execute_with_chase(&intent, &handle, other_leg_price).await;

        // 6. Handle result
        match result {
            Ok(exec_result) => {
                // Clear pending if complete
                if exec_result.status.is_complete() {
                    self.pending_tracker.clear(&market_id, order_type);
                    self.release_exposure(reserved);
                }

                info!(
                    handle_id = %handle.id,
                    market_id = %market_id,
                    status = ?exec_result.status,
                    "Order executed"
                );
                Ok(exec_result)
            }
            Err(err) => {
                // Clean up on failure
                self.pending_tracker.clear(&market_id, order_type);
                self.release_exposure(reserved);

                warn!(
                    market_id = %market_id,
                    error = %err,
                    "Order execution failed"
                );
                Err(Rejected::executor_rejected(intent, err.to_string()))
            }
        }
    }
}

#[async_trait]
impl<E: Executor + Send + Sync + 'static> OrderManager for ChasingOrderManager<E> {
    async fn submit(&self, intent: OrderIntent) -> Result<OrderHandle, Rejected> {
        // Default: no ceiling (use max_price from intent)
        let result = self.submit_with_ceiling(intent, Decimal::ZERO).await?;
        Ok(result.handle)
    }

    fn has_pending_order(&self, market_id: &str) -> bool {
        self.pending_tracker.has_any_pending(market_id)
    }

    async fn status(&self, handle: &OrderHandle) -> OrderStatus {
        // Check if still pending
        if self.pending_tracker.has_any_pending(&handle.market_id) {
            if let Some(ref order_id) = handle.executor_order_id {
                let executor = self.executor.read().await;
                if let Some(result) = executor.order_status(order_id).await {
                    return match result {
                        OrderResult::Filled(f) => OrderStatus::Filled {
                            filled_size: f.size,
                            avg_price: f.price,
                        },
                        OrderResult::PartialFill(p) => OrderStatus::PartialFill {
                            filled_size: p.filled_size,
                            avg_price: p.avg_price,
                            remaining: p.requested_size - p.filled_size,
                        },
                        OrderResult::Rejected(r) => OrderStatus::Rejected { reason: r.reason },
                        OrderResult::Pending(_) => OrderStatus::Pending,
                        OrderResult::Cancelled(_) => OrderStatus::Cancelled,
                    };
                }
            }
            return OrderStatus::Working;
        }
        OrderStatus::Unknown
    }

    async fn cancel(&self, handle: &OrderHandle) -> Result<(), Rejected> {
        let order_type = Self::order_type_from_intent(&handle.intent);

        if !self.pending_tracker.is_pending(&handle.market_id, order_type) {
            return Err(Rejected::other(
                handle.intent.clone(),
                "Order not found or already complete".to_string(),
            ));
        }

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

        self.on_order_complete(handle);
        Ok(())
    }

    fn on_order_complete(&self, handle: &OrderHandle) {
        let order_type = Self::order_type_from_intent(&handle.intent);
        if self.pending_tracker.clear(&handle.market_id, order_type) {
            let reserved = handle.intent.max_cost();
            self.release_exposure(reserved);
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
        Executor, ExecutorError, OrderCancellation, OrderFill, OrderRequest, OrderResult, PendingOrder,
    };
    use poly_common::types::{Outcome, Side};
    use rust_decimal_macros::dec;

    /// Test executor that fills orders immediately.
    struct ImmediateFillExecutor {
        fill_price: Decimal,
    }

    impl ImmediateFillExecutor {
        fn new(fill_price: Decimal) -> Self {
            Self { fill_price }
        }
    }

    #[async_trait::async_trait]
    impl Executor for ImmediateFillExecutor {
        async fn place_order(&mut self, order: OrderRequest) -> Result<OrderResult, ExecutorError> {
            Ok(OrderResult::Filled(OrderFill {
                request_id: order.request_id,
                order_id: "test-order-1".to_string(),
                size: order.size,
                price: self.fill_price,
                fee: dec!(0.01),
                timestamp: Utc::now(),
            }))
        }

        async fn cancel_order(&mut self, _order_id: &str) -> Result<OrderCancellation, ExecutorError> {
            Ok(OrderCancellation {
                request_id: String::new(),
                order_id: "test-order-1".to_string(),
                filled_size: Decimal::ZERO,
                unfilled_size: Decimal::ZERO,
                timestamp: Utc::now(),
            })
        }

        async fn order_status(&self, _order_id: &str) -> Option<OrderResult> {
            None
        }

        fn pending_orders(&self) -> Vec<PendingOrder> {
            Vec::new()
        }

        fn available_balance(&self) -> Decimal {
            dec!(10000)
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
    async fn test_submit_order_fills() {
        let executor = ImmediateFillExecutor::new(dec!(0.50));
        let config = ChasingOrderManagerConfig {
            use_market_orders: true, // Use market mode for immediate fills
            ..Default::default()
        };
        let manager = ChasingOrderManager::new(executor, config);

        let intent = create_test_intent();
        let result = manager.submit_with_ceiling(intent, dec!(0.45)).await;

        assert!(result.is_ok());
        let exec_result = result.unwrap();
        assert!(matches!(exec_result.status, OrderStatus::Filled { .. }));
    }

    #[tokio::test]
    async fn test_pending_tracking() {
        let executor = ImmediateFillExecutor::new(dec!(0.50));
        let config = ChasingOrderManagerConfig {
            use_market_orders: true,
            ..Default::default()
        };
        let manager = ChasingOrderManager::new(executor, config);

        assert!(!manager.has_pending_order("market-1"));

        // After immediate fill, should not be pending
        let intent = create_test_intent();
        let _ = manager.submit_with_ceiling(intent, dec!(0.45)).await;

        assert!(!manager.has_pending_order("market-1"));
    }

    #[tokio::test]
    async fn test_exposure_tracking() {
        let executor = ImmediateFillExecutor::new(dec!(0.50));
        let config = ChasingOrderManagerConfig {
            use_market_orders: true,
            ..Default::default()
        };
        let manager = ChasingOrderManager::new(executor, config);

        assert_eq!(manager.reserved_exposure(), dec!(0));

        // After fill, exposure should be released
        let intent = create_test_intent();
        let _ = manager.submit_with_ceiling(intent, dec!(0.45)).await;

        assert_eq!(manager.reserved_exposure(), dec!(0));
    }

    #[tokio::test]
    async fn test_reject_below_min_size() {
        let executor = ImmediateFillExecutor::new(dec!(0.50));
        let config = ChasingOrderManagerConfig {
            min_order_size: dec!(50),
            use_market_orders: true,
            ..Default::default()
        };
        let manager = ChasingOrderManager::new(executor, config);

        let intent = OrderIntent::new(
            "market-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(10), // Below minimum
            dec!(0.55),
        );

        let result = manager.submit_with_ceiling(intent, dec!(0.45)).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err().reason, RejectionReason::BelowMinSize));
    }
}
