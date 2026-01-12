//! Live executor stub for real order submission.
//!
//! This module provides a stub implementation of the live executor.
//! The full implementation requires:
//! - Polymarket API authentication (wallet credentials)
//! - WebSocket connection for fill notifications
//! - Shadow bid manager integration
//! - Price chasing logic
//!
//! ## Current State
//!
//! This is a stub that returns errors for all operations.
//! It will be fully implemented in Phase 5 (p5-3).
//!
//! ## Future Implementation
//!
//! The live executor will:
//! - Submit real orders via polymarket-client-sdk
//! - Track pending orders and fills via WebSocket
//! - Fire shadow bids on primary fill (< 2ms)
//! - Chase prices when fills are slow
//! - Update global state on fills

use async_trait::async_trait;
use rust_decimal::Decimal;
use tracing::warn;

use super::{
    Executor, ExecutorError, OrderCancellation, OrderRequest, OrderResult, PendingOrder,
};

/// Configuration for the live executor.
#[derive(Debug, Clone)]
pub struct LiveExecutorConfig {
    /// Polymarket API endpoint.
    pub api_endpoint: String,
    /// Maximum order timeout in milliseconds.
    pub order_timeout_ms: u64,
    /// Whether to enable shadow bids.
    pub enable_shadow: bool,
    /// Maximum price chase iterations.
    pub max_chase_iterations: u32,
    /// Price chase step size.
    pub chase_step_size: Decimal,
}

impl Default for LiveExecutorConfig {
    fn default() -> Self {
        Self {
            api_endpoint: "https://clob.polymarket.com".to_string(),
            order_timeout_ms: 30_000,
            enable_shadow: true,
            max_chase_iterations: 10,
            chase_step_size: Decimal::new(1, 2), // 0.01
        }
    }
}

/// Live executor stub.
///
/// This is a placeholder implementation that will be completed in Phase 5.
/// Currently returns errors for all operations.
pub struct LiveExecutor {
    #[allow(dead_code)]
    config: LiveExecutorConfig,
}

impl LiveExecutor {
    /// Create a new live executor stub.
    ///
    /// # Note
    ///
    /// This is a stub. Full implementation requires wallet credentials
    /// and will be completed in Phase 5.
    pub fn new(config: LiveExecutorConfig) -> Self {
        warn!("LiveExecutor is a stub - real order submission not implemented");
        Self { config }
    }

    /// Create a live executor with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(LiveExecutorConfig::default())
    }
}

#[async_trait]
impl Executor for LiveExecutor {
    async fn place_order(&mut self, order: OrderRequest) -> Result<OrderResult, ExecutorError> {
        warn!(
            request_id = %order.request_id,
            "LiveExecutor.place_order() called but not implemented"
        );
        Err(ExecutorError::Internal(
            "LiveExecutor not implemented - use PaperExecutor or BacktestExecutor".to_string(),
        ))
    }

    async fn cancel_order(&mut self, order_id: &str) -> Result<OrderCancellation, ExecutorError> {
        warn!(
            order_id = %order_id,
            "LiveExecutor.cancel_order() called but not implemented"
        );
        Err(ExecutorError::Internal(
            "LiveExecutor not implemented".to_string(),
        ))
    }

    async fn order_status(&self, order_id: &str) -> Option<OrderResult> {
        warn!(
            order_id = %order_id,
            "LiveExecutor.order_status() called but not implemented"
        );
        None
    }

    fn pending_orders(&self) -> Vec<PendingOrder> {
        Vec::new()
    }

    fn available_balance(&self) -> Decimal {
        warn!("LiveExecutor.available_balance() called but not implemented");
        Decimal::ZERO
    }

    async fn shutdown(&mut self) {
        warn!("LiveExecutor shutting down (stub)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poly_common::types::{Outcome, Side};
    use rust_decimal_macros::dec;

    #[tokio::test]
    async fn test_live_executor_stub_place_order() {
        let mut executor = LiveExecutor::with_defaults();

        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.45),
        );

        let result = executor.place_order(order).await;
        assert!(matches!(result, Err(ExecutorError::Internal(_))));
    }

    #[tokio::test]
    async fn test_live_executor_stub_cancel_order() {
        let mut executor = LiveExecutor::with_defaults();

        let result = executor.cancel_order("order-1").await;
        assert!(matches!(result, Err(ExecutorError::Internal(_))));
    }

    #[tokio::test]
    async fn test_live_executor_stub_order_status() {
        let executor = LiveExecutor::with_defaults();

        let result = executor.order_status("order-1").await;
        assert!(result.is_none());
    }

    #[test]
    fn test_live_executor_stub_pending_orders() {
        let executor = LiveExecutor::with_defaults();
        assert!(executor.pending_orders().is_empty());
    }

    #[test]
    fn test_live_executor_stub_balance() {
        let executor = LiveExecutor::with_defaults();
        assert_eq!(executor.available_balance(), Decimal::ZERO);
    }

    #[test]
    fn test_live_executor_config_default() {
        let config = LiveExecutorConfig::default();
        assert_eq!(config.api_endpoint, "https://clob.polymarket.com");
        assert_eq!(config.order_timeout_ms, 30_000);
        assert!(config.enable_shadow);
    }
}
