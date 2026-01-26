//! No-operation executor for shadow mode.
//!
//! Implements the Executor trait but never executes orders.
//! Used for shadow mode where we want to detect opportunities and log
//! decisions without actually trading.
//!
//! ## Use Cases
//!
//! - Shadow mode: Validate feeds and strategy without execution
//! - Testing: Verify opportunity detection without side effects
//! - Monitoring: Track what the bot *would* trade without risking capital

use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use tracing::{debug, info};

use poly_common::types::Side;

use crate::executor::{
    Executor, ExecutorError, OrderCancellation, OrderRejection, OrderRequest, OrderResult,
    PendingOrder,
};

/// Configuration for no-op executor.
#[derive(Debug, Clone)]
pub struct NoOpExecutorConfig {
    /// Virtual balance to report (for strategy compatibility).
    pub virtual_balance: Decimal,
    /// Whether to log rejected orders.
    pub log_orders: bool,
    /// Rejection reason to return for all orders.
    pub rejection_reason: String,
}

impl Default for NoOpExecutorConfig {
    fn default() -> Self {
        Self {
            virtual_balance: Decimal::new(10000, 0), // $10,000 virtual
            log_orders: true,
            rejection_reason: "Shadow mode: execution disabled".to_string(),
        }
    }
}

/// No-operation executor that logs but never executes.
///
/// All order placements are immediately rejected with a configurable reason.
/// Used for shadow mode to validate feeds and strategy without trading.
pub struct NoOpExecutor {
    config: NoOpExecutorConfig,
    /// Count of orders that would have been placed.
    orders_received: u64,
    /// Total volume that would have been traded.
    volume_received: Decimal,
}

impl NoOpExecutor {
    /// Create a new no-op executor.
    pub fn new(config: NoOpExecutorConfig) -> Self {
        Self {
            config,
            orders_received: 0,
            volume_received: Decimal::ZERO,
        }
    }

    /// Get the number of orders received (but not executed).
    pub fn orders_received(&self) -> u64 {
        self.orders_received
    }

    /// Get the total volume received (but not executed).
    pub fn volume_received(&self) -> Decimal {
        self.volume_received
    }
}

#[async_trait]
impl Executor for NoOpExecutor {
    async fn place_order(&mut self, order: OrderRequest) -> Result<OrderResult, ExecutorError> {
        // Track the order
        self.orders_received += 1;
        let order_value = match order.side {
            Side::Buy => order.price.unwrap_or(Decimal::ONE) * order.size,
            Side::Sell => order.size, // Track shares for sells
        };
        self.volume_received += order_value;

        // Log if enabled
        if self.config.log_orders {
            info!(
                request_id = %order.request_id,
                event_id = %order.event_id,
                token_id = %order.token_id,
                outcome = ?order.outcome,
                side = ?order.side,
                size = %order.size,
                price = ?order.price,
                order_type = ?order.order_type,
                "Shadow mode: Order would be placed"
            );
        }

        // Always reject - this is shadow mode
        Ok(OrderResult::Rejected(OrderRejection {
            request_id: order.request_id,
            reason: self.config.rejection_reason.clone(),
            timestamp: Utc::now(),
        }))
    }

    async fn cancel_order(&mut self, order_id: &str) -> Result<OrderCancellation, ExecutorError> {
        debug!(order_id = %order_id, "Shadow mode: Cancel request (no-op)");

        // Return a cancellation for a non-existent order
        Ok(OrderCancellation {
            request_id: String::new(),
            order_id: order_id.to_string(),
            filled_size: Decimal::ZERO,
            unfilled_size: Decimal::ZERO,
            timestamp: Utc::now(),
        })
    }

    async fn order_status(&self, order_id: &str) -> Option<OrderResult> {
        debug!(order_id = %order_id, "Shadow mode: Order status request (none)");
        // No orders are ever pending in shadow mode
        None
    }

    fn pending_orders(&self) -> Vec<PendingOrder> {
        // No orders are ever pending in shadow mode
        Vec::new()
    }

    fn available_balance(&self) -> Decimal {
        // Return virtual balance for strategy compatibility
        self.config.virtual_balance
    }

    fn market_exposure(&self, _event_id: &str) -> Decimal {
        // Noop executor doesn't track positions
        Decimal::ZERO
    }

    fn total_exposure(&self) -> Decimal {
        // Noop executor doesn't track positions
        Decimal::ZERO
    }

    fn remaining_capacity(&self) -> Decimal {
        // Unlimited capacity for noop executor
        Decimal::MAX
    }

    fn get_position(&self, _event_id: &str) -> Option<crate::executor::PositionSnapshot> {
        // Noop executor doesn't track positions
        None
    }

    async fn shutdown(&mut self) {
        info!(
            orders_received = self.orders_received,
            volume_received = %self.volume_received,
            "Shadow mode executor shutting down"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poly_common::types::Outcome;
    use rust_decimal_macros::dec;

    #[test]
    fn test_noop_executor_config_default() {
        let config = NoOpExecutorConfig::default();
        assert_eq!(config.virtual_balance, dec!(10000));
        assert!(config.log_orders);
        assert!(config.rejection_reason.contains("Shadow mode"));
    }

    #[test]
    fn test_noop_executor_new() {
        let config = NoOpExecutorConfig::default();
        let executor = NoOpExecutor::new(config);
        assert_eq!(executor.orders_received(), 0);
        assert_eq!(executor.volume_received(), Decimal::ZERO);
    }

    #[tokio::test]
    async fn test_noop_executor_place_order_rejected() {
        let config = NoOpExecutorConfig::default();
        let mut executor = NoOpExecutor::new(config);

        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.45),
        );

        let result = executor.place_order(order).await.unwrap();

        assert!(matches!(result, OrderResult::Rejected(_)));
        if let OrderResult::Rejected(rejection) = result {
            assert_eq!(rejection.request_id, "req-1");
            assert!(rejection.reason.contains("Shadow mode"));
        }
    }

    #[tokio::test]
    async fn test_noop_executor_tracks_orders() {
        let config = NoOpExecutorConfig::default();
        let mut executor = NoOpExecutor::new(config);

        // Place multiple orders
        for i in 0..5 {
            let order = OrderRequest::limit(
                format!("req-{}", i),
                "event-1".to_string(),
                "token-1".to_string(),
                Outcome::Yes,
                Side::Buy,
                dec!(100),
                dec!(0.45),
            );
            let _ = executor.place_order(order).await;
        }

        assert_eq!(executor.orders_received(), 5);
        assert_eq!(executor.volume_received(), dec!(225)); // 5 * 100 * 0.45
    }

    #[tokio::test]
    async fn test_noop_executor_cancel_order() {
        let config = NoOpExecutorConfig::default();
        let mut executor = NoOpExecutor::new(config);

        let result = executor.cancel_order("order-1").await.unwrap();

        assert_eq!(result.order_id, "order-1");
        assert_eq!(result.filled_size, Decimal::ZERO);
        assert_eq!(result.unfilled_size, Decimal::ZERO);
    }

    #[tokio::test]
    async fn test_noop_executor_order_status() {
        let config = NoOpExecutorConfig::default();
        let executor = NoOpExecutor::new(config);

        let result = executor.order_status("order-1").await;
        assert!(result.is_none());
    }

    #[test]
    fn test_noop_executor_pending_orders() {
        let config = NoOpExecutorConfig::default();
        let executor = NoOpExecutor::new(config);

        let pending = executor.pending_orders();
        assert!(pending.is_empty());
    }

    #[test]
    fn test_noop_executor_available_balance() {
        let config = NoOpExecutorConfig {
            virtual_balance: dec!(5000),
            ..Default::default()
        };
        let executor = NoOpExecutor::new(config);

        assert_eq!(executor.available_balance(), dec!(5000));
    }

    #[tokio::test]
    async fn test_noop_executor_shutdown() {
        let config = NoOpExecutorConfig::default();
        let mut executor = NoOpExecutor::new(config);

        // Place an order first
        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.50),
        );
        let _ = executor.place_order(order).await;

        // Shutdown should complete without error
        executor.shutdown().await;

        // Stats should still be accessible after shutdown
        assert_eq!(executor.orders_received(), 1);
    }

    #[tokio::test]
    async fn test_noop_executor_custom_rejection_reason() {
        let config = NoOpExecutorConfig {
            rejection_reason: "Test rejection".to_string(),
            ..Default::default()
        };
        let mut executor = NoOpExecutor::new(config);

        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.45),
        );

        let result = executor.place_order(order).await.unwrap();

        if let OrderResult::Rejected(rejection) = result {
            assert_eq!(rejection.reason, "Test rejection");
        } else {
            panic!("Expected rejection");
        }
    }

    #[tokio::test]
    async fn test_noop_executor_sell_order_tracking() {
        let config = NoOpExecutorConfig::default();
        let mut executor = NoOpExecutor::new(config);

        // Sell order tracks size, not price * size
        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::Yes,
            Side::Sell,
            dec!(100),
            dec!(0.45),
        );
        let _ = executor.place_order(order).await;

        // For sells, we track the share count
        assert_eq!(executor.volume_received(), dec!(100));
    }
}
