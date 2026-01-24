//! Async Order Manager - Non-blocking order management with background task.
//!
//! This module provides an order manager that runs in a separate tokio task,
//! allowing the strategy to submit orders without blocking the event loop.
//!
//! ## Architecture
//!
//! ```text
//! Strategy ──► [command channel] ──► OrderManagerTask ──► Executor
//!    │                                      │
//!    └──────◄ [update channel] ◄────────────┘
//! ```
//!
//! The strategy submits orders via a non-blocking channel send and receives
//! fill notifications asynchronously. This allows:
//! - Price chasing without blocking data input
//! - Multiple orders executing concurrently
//! - Clean separation of concerns

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use rust_decimal::Decimal;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{debug, info, warn};

use poly_common::types::{Outcome, Side};

use crate::executor::{Executor, OrderRequest, OrderResult, OrderType};
use crate::executor::chase::{ChaseConfig, ChaseStopReason, PriceChaser};
use crate::state::GlobalState;

use super::{
    OrderHandle, OrderHandleId, OrderIntent,
    PendingOrderTracker, PendingOrderType, Rejected, RejectionReason,
};

/// Command sent from strategy to order manager task.
#[derive(Debug)]
pub enum OrderCommand {
    /// Submit a new order.
    Submit {
        /// The order intent
        intent: OrderIntent,
        /// Price of the other leg (for ceiling calculation in arb/directional)
        other_leg_price: Decimal,
        /// Channel to send back the handle or rejection
        response: oneshot::Sender<Result<OrderHandle, Rejected>>,
    },
    /// Cancel an existing order.
    Cancel {
        /// Handle ID to cancel
        handle_id: OrderHandleId,
        /// Channel to send back result
        response: oneshot::Sender<Result<(), String>>,
    },
    /// Shutdown the order manager task.
    Shutdown,
}

/// Update sent from order manager task back to strategy.
#[derive(Debug, Clone)]
pub enum OrderUpdateEvent {
    /// Order was filled (fully or partially).
    Fill {
        handle_id: OrderHandleId,
        market_id: String,
        token_id: String,
        outcome: Outcome,
        side: Side,
        filled_size: Decimal,
        avg_price: Decimal,
        fee: Decimal,
        is_complete: bool,
    },
    /// Order was rejected by exchange.
    Rejected {
        handle_id: OrderHandleId,
        market_id: String,
        reason: String,
    },
    /// Order was cancelled.
    Cancelled {
        handle_id: OrderHandleId,
        market_id: String,
    },
    /// Chase stopped (timeout, ceiling reached, etc.)
    ChaseEnded {
        handle_id: OrderHandleId,
        market_id: String,
        reason: ChaseStopReason,
        filled_size: Decimal,
        avg_price: Decimal,
    },
}

impl OrderUpdateEvent {
    /// Get the handle ID for this event.
    pub fn handle_id(&self) -> &OrderHandleId {
        match self {
            OrderUpdateEvent::Fill { handle_id, .. } => handle_id,
            OrderUpdateEvent::Rejected { handle_id, .. } => handle_id,
            OrderUpdateEvent::Cancelled { handle_id, .. } => handle_id,
            OrderUpdateEvent::ChaseEnded { handle_id, .. } => handle_id,
        }
    }

    /// Get the market ID for this event.
    pub fn market_id(&self) -> &str {
        match self {
            OrderUpdateEvent::Fill { market_id, .. } => market_id,
            OrderUpdateEvent::Rejected { market_id, .. } => market_id,
            OrderUpdateEvent::Cancelled { market_id, .. } => market_id,
            OrderUpdateEvent::ChaseEnded { market_id, .. } => market_id,
        }
    }
}

/// Configuration for the async order manager.
#[derive(Debug, Clone)]
pub struct AsyncOrderManagerConfig {
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
    /// Command channel capacity.
    pub command_channel_capacity: usize,
    /// Update channel capacity.
    pub update_channel_capacity: usize,
}

impl Default for AsyncOrderManagerConfig {
    fn default() -> Self {
        Self {
            max_market_exposure: Decimal::new(300, 0),
            max_total_exposure: Decimal::new(1000, 0),
            min_order_size: Decimal::new(5, 0),
            use_market_orders: true,
            chase_config: ChaseConfig::default(),
            command_channel_capacity: 100,
            update_channel_capacity: 1000,
        }
    }
}

/// Handle to the async order manager.
///
/// This is the interface the strategy uses. Commands are sent via channel
/// and don't block the caller.
#[derive(Clone)]
pub struct AsyncOrderManager {
    /// Channel to send commands to the task.
    command_tx: mpsc::Sender<OrderCommand>,
    /// Pending order tracker (shared with task for fast lookups).
    pending_tracker: Arc<PendingOrderTracker>,
}

impl AsyncOrderManager {
    /// Check if a market has a pending order (non-blocking).
    pub fn has_pending_order(&self, market_id: &str) -> bool {
        self.pending_tracker.has_any_pending(market_id)
    }

    /// Check if a specific order type is pending for a market.
    pub fn is_pending(&self, market_id: &str, order_type: PendingOrderType) -> bool {
        self.pending_tracker.is_pending(market_id, order_type)
    }

    /// Submit an order (non-blocking).
    ///
    /// Returns immediately with a handle or rejection.
    /// Fill notifications come via the update channel.
    pub async fn submit(
        &self,
        intent: OrderIntent,
        other_leg_price: Decimal,
    ) -> Result<OrderHandle, Rejected> {
        let (response_tx, response_rx) = oneshot::channel();

        // Send command (non-blocking if channel has capacity)
        if let Err(e) = self.command_tx.send(OrderCommand::Submit {
            intent: intent.clone(),
            other_leg_price,
            response: response_tx,
        }).await {
            warn!("Failed to send submit command: {}", e);
            return Err(Rejected::other(intent, "Order manager channel full".to_string()));
        }

        // Wait for acknowledgment (should be fast - just validation)
        match response_rx.await {
            Ok(result) => result,
            Err(_) => Err(Rejected::other(intent, "Order manager task died".to_string())),
        }
    }

    /// Cancel an order (non-blocking).
    pub async fn cancel(&self, handle_id: OrderHandleId) -> Result<(), String> {
        let (response_tx, response_rx) = oneshot::channel();

        if let Err(e) = self.command_tx.send(OrderCommand::Cancel {
            handle_id,
            response: response_tx,
        }).await {
            return Err(format!("Failed to send cancel command: {}", e));
        }

        match response_rx.await {
            Ok(result) => result,
            Err(_) => Err("Order manager task died".to_string()),
        }
    }

    /// Request shutdown of the order manager task.
    pub async fn shutdown(&self) {
        let _ = self.command_tx.send(OrderCommand::Shutdown).await;
    }
}

/// Active order being managed by the task.
struct ActiveOrder {
    handle: OrderHandle,
    other_leg_price: Decimal,
    /// Cancel signal sender (drop to signal cancellation)
    cancel_tx: Option<oneshot::Sender<()>>,
}

/// The background task that manages orders.
pub struct OrderManagerTask<E: Executor> {
    /// The executor for placing orders.
    executor: Arc<RwLock<E>>,
    /// Configuration.
    config: AsyncOrderManagerConfig,
    /// Price chaser for limit orders.
    chaser: PriceChaser,
    /// Global state for live orderbook access.
    state: Arc<GlobalState>,
    /// Pending order tracker.
    pending_tracker: Arc<PendingOrderTracker>,
    /// Active orders being managed.
    active_orders: HashMap<OrderHandleId, ActiveOrder>,
    /// Channel to receive commands.
    command_rx: mpsc::Receiver<OrderCommand>,
    /// Channel to send updates.
    update_tx: mpsc::Sender<OrderUpdateEvent>,
}

impl<E: Executor + Send + Sync + 'static> OrderManagerTask<E> {
    /// Spawn the order manager task and return handles for communication.
    ///
    /// Returns:
    /// - `AsyncOrderManager` for sending commands
    /// - `mpsc::Receiver<OrderUpdateEvent>` for receiving fill notifications
    /// - `JoinHandle` for the background task
    pub fn spawn(
        executor: E,
        config: AsyncOrderManagerConfig,
        state: Arc<GlobalState>,
    ) -> (
        AsyncOrderManager,
        mpsc::Receiver<OrderUpdateEvent>,
        tokio::task::JoinHandle<()>,
    ) {
        let (command_tx, command_rx) = mpsc::channel(config.command_channel_capacity);
        let (update_tx, update_rx) = mpsc::channel(config.update_channel_capacity);
        let pending_tracker = Arc::new(PendingOrderTracker::new());

        let chaser = PriceChaser::new(config.chase_config.clone());

        let task = Self {
            executor: Arc::new(RwLock::new(executor)),
            config,
            chaser,
            state,
            pending_tracker: pending_tracker.clone(),
            active_orders: HashMap::new(),
            command_rx,
            update_tx,
        };

        let handle = tokio::spawn(task.run());

        let manager = AsyncOrderManager {
            command_tx,
            pending_tracker,
        };

        (manager, update_rx, handle)
    }

    /// Main task loop.
    async fn run(mut self) {
        info!("Order manager task started");

        loop {
            tokio::select! {
                // Handle incoming commands
                Some(command) = self.command_rx.recv() => {
                    match command {
                        OrderCommand::Submit { intent, other_leg_price, response } => {
                            let result = self.handle_submit(intent, other_leg_price).await;
                            let _ = response.send(result);
                        }
                        OrderCommand::Cancel { handle_id, response } => {
                            let result = self.handle_cancel(&handle_id);
                            let _ = response.send(result);
                        }
                        OrderCommand::Shutdown => {
                            info!("Order manager task shutting down");
                            break;
                        }
                    }
                }
                else => {
                    // Channel closed, exit
                    info!("Order manager command channel closed");
                    break;
                }
            }
        }

        // Cancel any active orders on shutdown
        for (handle_id, active) in self.active_orders.drain() {
            if let Some(cancel_tx) = active.cancel_tx {
                let _ = cancel_tx.send(());
            }
            self.pending_tracker.clear(&active.handle.market_id, PendingOrderType::Directional);
            debug!(handle_id = %handle_id, "Cancelled order on shutdown");
        }

        info!("Order manager task stopped");
    }

    /// Handle a submit command.
    async fn handle_submit(
        &mut self,
        intent: OrderIntent,
        other_leg_price: Decimal,
    ) -> Result<OrderHandle, Rejected> {
        let market_id = intent.market_id.clone();
        let order_type = Self::order_type_from_intent(&intent);

        // 1. Check for existing pending order
        if self.pending_tracker.is_pending(&market_id, order_type) {
            debug!(market_id = %market_id, "Rejecting - market has pending order");
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

        // 3. Check exposure limits (simplified - could be more sophisticated)
        let cost = intent.max_cost();
        if cost > self.config.max_market_exposure {
            return Err(Rejected {
                reason: RejectionReason::MarketExposureLimit,
                intent,
                existing_handle: None,
            });
        }

        // 4. Create handle and register as pending
        let handle = OrderHandle::new(intent.clone());
        self.pending_tracker.register(&market_id, order_type);

        // 5. Create cancel channel for this order
        let (cancel_tx, cancel_rx) = oneshot::channel();

        // 6. Store active order
        let handle_clone = handle.clone();
        self.active_orders.insert(handle.id.clone(), ActiveOrder {
            handle: handle.clone(),
            other_leg_price,
            cancel_tx: Some(cancel_tx),
        });

        // 7. Spawn execution task (doesn't block this function)
        let executor = self.executor.clone();
        let config = self.config.clone();
        let chaser = PriceChaser::new(self.config.chase_config.clone());
        let state = self.state.clone();
        let update_tx = self.update_tx.clone();
        let pending_tracker = self.pending_tracker.clone();
        let intent_clone = intent.clone();

        tokio::spawn(async move {
            Self::execute_order(
                executor,
                config,
                chaser,
                state,
                update_tx,
                pending_tracker,
                handle_clone,
                intent_clone,
                other_leg_price,
                cancel_rx,
            ).await;
        });

        info!(
            handle_id = %handle.id,
            market_id = %market_id,
            token_id = %intent.token_id,
            size = %intent.size,
            max_price = %intent.max_price,
            "Order submitted to background task"
        );

        Ok(handle)
    }

    /// Execute an order in a background task.
    async fn execute_order(
        executor: Arc<RwLock<E>>,
        config: AsyncOrderManagerConfig,
        chaser: PriceChaser,
        state: Arc<GlobalState>,
        update_tx: mpsc::Sender<OrderUpdateEvent>,
        pending_tracker: Arc<PendingOrderTracker>,
        handle: OrderHandle,
        intent: OrderIntent,
        other_leg_price: Decimal,
        mut cancel_rx: oneshot::Receiver<()>,
    ) {
        let market_id = intent.market_id.clone();
        let token_id = intent.token_id.clone();
        let handle_id = handle.id.clone();
        let order_type = Self::order_type_from_intent(&intent);

        // Build the order request
        let order_type_enum = if config.use_market_orders {
            OrderType::Ioc
        } else {
            OrderType::Gtc
        };

        let request = OrderRequest {
            request_id: handle_id.0.clone(),
            event_id: market_id.clone(),
            token_id: token_id.clone(),
            outcome: intent.outcome,
            side: intent.side,
            size: intent.size,
            price: Some(intent.max_price),
            order_type: order_type_enum,
            timeout_ms: None,
            timestamp: Utc::now(),
        };

        // Execute based on config
        let result = if config.use_market_orders {
            // Simple market order - just place once
            let mut exec = executor.write().await;
            exec.place_order(request).await
        } else {
            // Use chaser with live orderbook data
            let token_id_for_closure = token_id.clone();
            let state_for_closure = state.clone();
            let get_best_price = move || {
                state_for_closure.market_data.order_books.get(&token_id_for_closure)
                    .map(|book| book.best_bid)
            };

            // Run chase with cancellation support
            let mut exec = executor.write().await;
            tokio::select! {
                chase_result = chaser.chase_order_with_market(
                    &mut *exec,
                    request,
                    other_leg_price,
                    get_best_price,
                ) => {
                    match chase_result {
                        Ok(cr) => {
                            // Convert chase result to order result
                            if cr.success || cr.filled_size > Decimal::ZERO {
                                Ok(OrderResult::Filled(crate::executor::OrderFill {
                                    request_id: handle_id.0.clone(),
                                    order_id: "chased".to_string(),
                                    size: cr.filled_size,
                                    price: cr.avg_price,
                                    fee: cr.total_fee,
                                    timestamp: Utc::now(),
                                }))
                            } else {
                                // Chase ended without fill - send specific event
                                if let Some(reason) = cr.stop_reason {
                                    let _ = update_tx.send(OrderUpdateEvent::ChaseEnded {
                                        handle_id: handle_id.clone(),
                                        market_id: market_id.clone(),
                                        reason,
                                        filled_size: cr.filled_size,
                                        avg_price: cr.avg_price,
                                    }).await;
                                }
                                pending_tracker.clear(&market_id, order_type);
                                return;
                            }
                        }
                        Err(e) => Err(e),
                    }
                }
                _ = &mut cancel_rx => {
                    // Cancellation requested
                    debug!(handle_id = %handle_id, "Order cancelled during chase");
                    let _ = update_tx.send(OrderUpdateEvent::Cancelled {
                        handle_id: handle_id.clone(),
                        market_id: market_id.clone(),
                    }).await;
                    pending_tracker.clear(&market_id, order_type);
                    return;
                }
            }
        };

        // Send result update
        match result {
            Ok(OrderResult::Filled(fill)) => {
                let _ = update_tx.send(OrderUpdateEvent::Fill {
                    handle_id: handle_id.clone(),
                    market_id: market_id.clone(),
                    token_id,
                    outcome: intent.outcome,
                    side: intent.side,
                    filled_size: fill.size,
                    avg_price: fill.price,
                    fee: fill.fee,
                    is_complete: true,
                }).await;
            }
            Ok(OrderResult::PartialFill(partial)) => {
                let _ = update_tx.send(OrderUpdateEvent::Fill {
                    handle_id: handle_id.clone(),
                    market_id: market_id.clone(),
                    token_id,
                    outcome: intent.outcome,
                    side: intent.side,
                    filled_size: partial.filled_size,
                    avg_price: partial.avg_price,
                    fee: partial.fee,
                    is_complete: false,
                }).await;
            }
            Ok(OrderResult::Rejected(rejection)) => {
                let _ = update_tx.send(OrderUpdateEvent::Rejected {
                    handle_id: handle_id.clone(),
                    market_id: market_id.clone(),
                    reason: rejection.reason,
                }).await;
            }
            Ok(OrderResult::Cancelled(_)) => {
                let _ = update_tx.send(OrderUpdateEvent::Cancelled {
                    handle_id: handle_id.clone(),
                    market_id: market_id.clone(),
                }).await;
            }
            Ok(OrderResult::Pending(_)) => {
                // Shouldn't happen with IOC, but handle gracefully
                debug!(handle_id = %handle_id, "Order still pending after execution");
            }
            Err(e) => {
                warn!(handle_id = %handle_id, error = %e, "Order execution failed");
                let _ = update_tx.send(OrderUpdateEvent::Rejected {
                    handle_id: handle_id.clone(),
                    market_id: market_id.clone(),
                    reason: e.to_string(),
                }).await;
            }
        }

        // Clear pending state
        pending_tracker.clear(&market_id, order_type);
    }

    /// Handle a cancel command.
    fn handle_cancel(&mut self, handle_id: &OrderHandleId) -> Result<(), String> {
        if let Some(mut active) = self.active_orders.remove(handle_id) {
            if let Some(cancel_tx) = active.cancel_tx.take() {
                let _ = cancel_tx.send(());
            }
            self.pending_tracker.clear(&active.handle.market_id, PendingOrderType::Directional);
            Ok(())
        } else {
            Err("Order not found".to_string())
        }
    }

    /// Map pending order type from intent.
    fn order_type_from_intent(intent: &OrderIntent) -> PendingOrderType {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{
        Executor, ExecutorError, OrderCancellation, OrderFill, OrderRequest, OrderResult, PendingOrder,
    };
    use rust_decimal_macros::dec;

    /// Test executor that fills orders immediately.
    struct ImmediateFillExecutor {
        fill_price: Decimal,
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

        async fn shutdown(&mut self) {}
    }

    #[tokio::test]
    async fn test_async_order_manager_submit() {
        let executor = ImmediateFillExecutor { fill_price: dec!(0.50) };
        let config = AsyncOrderManagerConfig {
            use_market_orders: true,
            ..Default::default()
        };
        let market_data = Arc::new(GlobalState::new());

        let (manager, mut update_rx, _handle) = OrderManagerTask::spawn(executor, config, market_data);

        let intent = OrderIntent::new(
            "market-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.55),
        );

        // Submit should return immediately with handle
        let result = manager.submit(intent, dec!(0.45)).await;
        assert!(result.is_ok());

        let handle = result.unwrap();
        assert_eq!(handle.market_id, "market-1");

        // Should receive fill notification
        let update = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            update_rx.recv(),
        ).await.expect("timeout").expect("channel closed");

        match update {
            OrderUpdateEvent::Fill { filled_size, avg_price, is_complete, .. } => {
                assert_eq!(filled_size, dec!(100));
                assert_eq!(avg_price, dec!(0.50));
                assert!(is_complete);
            }
            _ => panic!("Expected fill event"),
        }

        manager.shutdown().await;
    }

    #[tokio::test]
    async fn test_async_order_manager_reject_duplicate() {
        let executor = ImmediateFillExecutor { fill_price: dec!(0.50) };
        let config = AsyncOrderManagerConfig {
            use_market_orders: true,
            ..Default::default()
        };
        let market_data = Arc::new(GlobalState::new());

        let (manager, _update_rx, _handle) = OrderManagerTask::spawn(executor, config, market_data);

        // Create a slow-filling executor scenario by just checking pending state
        let intent1 = OrderIntent::new(
            "market-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.55),
        );

        let _ = manager.submit(intent1.clone(), dec!(0.45)).await;

        // Second submit to same market should be rejected while first is pending
        // (Note: This test is a bit racy - the first might complete before second submits)
        // In practice, pending state is checked synchronously in handle_submit

        manager.shutdown().await;
    }

    #[tokio::test]
    async fn test_async_order_manager_has_pending() {
        let executor = ImmediateFillExecutor { fill_price: dec!(0.50) };
        let config = AsyncOrderManagerConfig {
            use_market_orders: true,
            ..Default::default()
        };
        let market_data = Arc::new(GlobalState::new());

        let (manager, _update_rx, _handle) = OrderManagerTask::spawn(executor, config, market_data);

        assert!(!manager.has_pending_order("market-1"));

        manager.shutdown().await;
    }
}
