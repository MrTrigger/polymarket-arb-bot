//! Order execution abstraction for live, paper, and backtest trading.
//!
//! This module provides the `Executor` trait that abstracts order submission.
//! The same strategy code can work with:
//! - Live execution via Polymarket API
//! - Paper trading with simulated fills
//! - Backtesting against historical order book data
//!
//! ## Order Flow
//!
//! 1. Strategy detects opportunity and calls `place_order()`
//! 2. Executor validates order and submits (or simulates)
//! 3. Executor returns `OrderResult` with fill status
//! 4. Strategy updates inventory based on result
//!
//! ## Implementations
//!
//! - `LiveExecutor`: Real order submission (stubbed, requires wallet)
//! - `SimulatedExecutor`: Unified executor for paper trading and backtesting
//!   - Paper mode: Real-time timestamps, optional latency, simple fills
//!   - Backtest mode: Simulated time, order book fills, no latency
//!
//! ## Shadow Bidding
//!
//! The `shadow` module provides pre-hashed order support for fast secondary
//! order submission (<2ms) when a primary order fills.

pub mod allowance;
pub mod chase;
pub mod fill;
pub mod interval;
pub mod live;
pub mod live_sdk;
pub mod noop;
pub mod shadow;
pub mod simulated;

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use poly_common::types::{Outcome, Side};

/// Errors that can occur during order execution.
#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("Order rejected: {0}")]
    Rejected(String),

    #[error("Order timeout: {0}")]
    Timeout(String),

    #[error("Connection failed: {0}")]
    Connection(String),

    #[error("Insufficient funds: available={available}, required={required}")]
    InsufficientFunds { available: Decimal, required: Decimal },

    #[error("Position limit exceeded: current={current}, max={max}")]
    PositionLimit { current: Decimal, max: Decimal },

    #[error("Market closed")]
    MarketClosed,

    #[error("Invalid order: {0}")]
    InvalidOrder(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

/// Order type for execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderType {
    /// Limit order at specified price.
    Limit,
    /// Market order (fill at best available).
    Market,
    /// Immediate-or-cancel (partial fills ok, cancel rest).
    Ioc,
    /// Good-till-cancelled limit order.
    Gtc,
}

impl fmt::Display for OrderType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OrderType::Limit => write!(f, "LIMIT"),
            OrderType::Market => write!(f, "MARKET"),
            OrderType::Ioc => write!(f, "IOC"),
            OrderType::Gtc => write!(f, "GTC"),
        }
    }
}

/// Request to place an order.
#[derive(Debug, Clone)]
pub struct OrderRequest {
    /// Unique request ID for tracking.
    pub request_id: String,
    /// Event ID for the market.
    pub event_id: String,
    /// Token ID to trade.
    pub token_id: String,
    /// YES or NO outcome.
    pub outcome: Outcome,
    /// Buy or Sell.
    pub side: Side,
    /// Size to trade.
    pub size: Decimal,
    /// Limit price (required for Limit/Gtc orders).
    pub price: Option<Decimal>,
    /// Order type.
    pub order_type: OrderType,
    /// Maximum time to wait for fill (milliseconds).
    pub timeout_ms: Option<u64>,
    /// Request timestamp.
    pub timestamp: DateTime<Utc>,
}

impl OrderRequest {
    /// Create a new limit order request.
    pub fn limit(
        request_id: String,
        event_id: String,
        token_id: String,
        outcome: Outcome,
        side: Side,
        size: Decimal,
        price: Decimal,
    ) -> Self {
        Self {
            request_id,
            event_id,
            token_id,
            outcome,
            side,
            size,
            price: Some(price),
            order_type: OrderType::Limit,
            timeout_ms: None,
            timestamp: Utc::now(),
        }
    }

    /// Create a new IOC order request.
    pub fn ioc(
        request_id: String,
        event_id: String,
        token_id: String,
        outcome: Outcome,
        side: Side,
        size: Decimal,
        price: Decimal,
    ) -> Self {
        Self {
            request_id,
            event_id,
            token_id,
            outcome,
            side,
            size,
            price: Some(price),
            order_type: OrderType::Ioc,
            timeout_ms: None,
            timestamp: Utc::now(),
        }
    }

    /// Create a new market order request.
    pub fn market(
        request_id: String,
        event_id: String,
        token_id: String,
        outcome: Outcome,
        side: Side,
        size: Decimal,
    ) -> Self {
        Self {
            request_id,
            event_id,
            token_id,
            outcome,
            side,
            size,
            price: None,
            order_type: OrderType::Market,
            timeout_ms: None,
            timestamp: Utc::now(),
        }
    }

    /// Set timeout for the order.
    pub fn with_timeout(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    /// Calculate the maximum cost for this order.
    ///
    /// For buy orders, this is price * size.
    /// For sell orders, returns zero (selling existing position).
    pub fn max_cost(&self) -> Decimal {
        match self.side {
            Side::Buy => {
                let price = self.price.unwrap_or(Decimal::ONE);
                price * self.size
            }
            Side::Sell => Decimal::ZERO,
        }
    }
}

/// Result of an order submission.
#[derive(Debug, Clone)]
pub enum OrderResult {
    /// Order fully filled.
    Filled(OrderFill),
    /// Order partially filled (remaining cancelled).
    PartialFill(PartialOrderFill),
    /// Order rejected (not submitted).
    Rejected(OrderRejection),
    /// Order pending (async execution).
    Pending(PendingOrder),
    /// Order cancelled.
    Cancelled(OrderCancellation),
}

impl OrderResult {
    /// Returns true if the order was at least partially filled.
    pub fn is_filled(&self) -> bool {
        matches!(self, OrderResult::Filled(_) | OrderResult::PartialFill(_))
    }

    /// Returns true if the order was rejected.
    pub fn is_rejected(&self) -> bool {
        matches!(self, OrderResult::Rejected(_))
    }

    /// Returns true if the order is pending.
    pub fn is_pending(&self) -> bool {
        matches!(self, OrderResult::Pending(_))
    }

    /// Get the filled size, if any.
    pub fn filled_size(&self) -> Decimal {
        match self {
            OrderResult::Filled(f) => f.size,
            OrderResult::PartialFill(f) => f.filled_size,
            _ => Decimal::ZERO,
        }
    }

    /// Get the filled cost (size * price), if any.
    pub fn filled_cost(&self) -> Decimal {
        match self {
            OrderResult::Filled(f) => f.size * f.price,
            OrderResult::PartialFill(f) => f.filled_size * f.avg_price,
            _ => Decimal::ZERO,
        }
    }

    /// Get the order ID, if assigned.
    pub fn order_id(&self) -> Option<&str> {
        match self {
            OrderResult::Filled(f) => Some(&f.order_id),
            OrderResult::PartialFill(f) => Some(&f.order_id),
            OrderResult::Pending(p) => Some(&p.order_id),
            OrderResult::Cancelled(c) => Some(&c.order_id),
            OrderResult::Rejected(_) => None,
        }
    }
}

/// A fully filled order.
#[derive(Debug, Clone)]
pub struct OrderFill {
    /// Request ID from the original order.
    pub request_id: String,
    /// Assigned order ID.
    pub order_id: String,
    /// Filled size.
    pub size: Decimal,
    /// Fill price.
    pub price: Decimal,
    /// Fee paid.
    pub fee: Decimal,
    /// Fill timestamp.
    pub timestamp: DateTime<Utc>,
}

/// A partially filled order.
#[derive(Debug, Clone)]
pub struct PartialOrderFill {
    /// Request ID from the original order.
    pub request_id: String,
    /// Assigned order ID.
    pub order_id: String,
    /// Requested size.
    pub requested_size: Decimal,
    /// Actually filled size.
    pub filled_size: Decimal,
    /// Average fill price.
    pub avg_price: Decimal,
    /// Total fee paid.
    pub fee: Decimal,
    /// Fill timestamp.
    pub timestamp: DateTime<Utc>,
}

/// A rejected order.
#[derive(Debug, Clone)]
pub struct OrderRejection {
    /// Request ID from the original order.
    pub request_id: String,
    /// Rejection reason.
    pub reason: String,
    /// Rejection timestamp.
    pub timestamp: DateTime<Utc>,
}

/// A pending order waiting for execution.
#[derive(Debug, Clone)]
pub struct PendingOrder {
    /// Request ID from the original order.
    pub request_id: String,
    /// Assigned order ID.
    pub order_id: String,
    /// Order timestamp.
    pub timestamp: DateTime<Utc>,
}

/// A cancelled order.
#[derive(Debug, Clone)]
pub struct OrderCancellation {
    /// Request ID from the original order.
    pub request_id: String,
    /// Order ID that was cancelled.
    pub order_id: String,
    /// Size that was filled before cancellation.
    pub filled_size: Decimal,
    /// Remaining unfilled size.
    pub unfilled_size: Decimal,
    /// Cancellation timestamp.
    pub timestamp: DateTime<Utc>,
}

/// Order execution trait.
///
/// Implementations provide different execution modes:
/// - `LiveExecutor`: Real order submission via Polymarket API
/// - `SimulatedExecutor`: Unified paper trading and backtesting
#[async_trait]
pub trait Executor: Send + Sync {
    /// Place an order and wait for initial result.
    ///
    /// For live execution, this may return `Pending` immediately.
    /// For paper/backtest, this typically returns filled or rejected.
    ///
    /// # Errors
    ///
    /// Returns an error if the order cannot be submitted.
    async fn place_order(&mut self, order: OrderRequest) -> Result<OrderResult, ExecutorError>;

    /// Cancel an existing order by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the order cannot be cancelled.
    async fn cancel_order(&mut self, order_id: &str) -> Result<OrderCancellation, ExecutorError>;

    /// Get the status of a pending order.
    ///
    /// Returns `None` if the order is not found.
    async fn order_status(&self, order_id: &str) -> Option<OrderResult>;

    /// Get all pending orders.
    fn pending_orders(&self) -> Vec<PendingOrder>;

    /// Get available balance for trading.
    ///
    /// For paper trading, returns simulated balance.
    fn available_balance(&self) -> Decimal;

    /// Settle a market position when it expires.
    ///
    /// For binary options, this determines the payout based on the winning outcome:
    /// - YES wins: YES shares pay $1, NO shares pay $0
    /// - NO wins: NO shares pay $1, YES shares pay $0
    ///
    /// Returns the realized PnL from the settlement (positive = profit, negative = loss).
    ///
    /// # Arguments
    /// * `event_id` - The market event ID
    /// * `yes_wins` - True if YES outcome won, false if NO won
    ///
    /// Default implementation returns 0 (no-op for live execution).
    async fn settle_market(&mut self, _event_id: &str, _yes_wins: bool) -> Decimal {
        Decimal::ZERO
    }

    /// Shutdown the executor gracefully.
    async fn shutdown(&mut self);

    /// Get simulation statistics (for backtest/paper trading).
    ///
    /// Returns None for live executors, Some for simulated executors.
    fn simulation_stats(&self) -> Option<crate::executor::simulated::SimulatedStats> {
        None
    }

    /// Update order book for backtesting.
    ///
    /// For backtest execution, this updates the simulated order book state.
    /// For live/paper execution, this is a no-op.
    ///
    /// # Arguments
    /// * `token_id` - The token ID for the order book
    /// * `bids` - Bid price levels
    /// * `asks` - Ask price levels
    async fn update_order_book(
        &self,
        _token_id: &str,
        _bids: Vec<crate::types::PriceLevel>,
        _asks: Vec<crate::types::PriceLevel>,
    ) {
        // Default: no-op for live/paper execution
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_order_request_limit() {
        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.45),
        );

        assert_eq!(order.request_id, "req-1");
        assert_eq!(order.outcome, Outcome::Yes);
        assert_eq!(order.side, Side::Buy);
        assert_eq!(order.size, dec!(100));
        assert_eq!(order.price, Some(dec!(0.45)));
        assert_eq!(order.order_type, OrderType::Limit);
        assert_eq!(order.max_cost(), dec!(45));
    }

    #[test]
    fn test_order_request_market() {
        let order = OrderRequest::market(
            "req-2".to_string(),
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::No,
            Side::Sell,
            dec!(50),
        );

        assert_eq!(order.order_type, OrderType::Market);
        assert!(order.price.is_none());
        assert_eq!(order.max_cost(), Decimal::ZERO); // Selling, no cost
    }

    #[test]
    fn test_order_request_ioc() {
        let order = OrderRequest::ioc(
            "req-3".to_string(),
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.50),
        );

        assert_eq!(order.order_type, OrderType::Ioc);
        assert_eq!(order.price, Some(dec!(0.50)));
    }

    #[test]
    fn test_order_request_with_timeout() {
        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-1".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.45),
        )
        .with_timeout(5000);

        assert_eq!(order.timeout_ms, Some(5000));
    }

    #[test]
    fn test_order_result_filled() {
        let fill = OrderFill {
            request_id: "req-1".to_string(),
            order_id: "order-1".to_string(),
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.01),
            timestamp: Utc::now(),
        };

        let result = OrderResult::Filled(fill);
        assert!(result.is_filled());
        assert!(!result.is_rejected());
        assert!(!result.is_pending());
        assert_eq!(result.filled_size(), dec!(100));
        assert_eq!(result.filled_cost(), dec!(45));
        assert_eq!(result.order_id(), Some("order-1"));
    }

    #[test]
    fn test_order_result_partial_fill() {
        let fill = PartialOrderFill {
            request_id: "req-1".to_string(),
            order_id: "order-1".to_string(),
            requested_size: dec!(100),
            filled_size: dec!(50),
            avg_price: dec!(0.45),
            fee: dec!(0.005),
            timestamp: Utc::now(),
        };

        let result = OrderResult::PartialFill(fill);
        assert!(result.is_filled());
        assert_eq!(result.filled_size(), dec!(50));
        assert_eq!(result.filled_cost(), dec!(22.5));
    }

    #[test]
    fn test_order_result_rejected() {
        let rejection = OrderRejection {
            request_id: "req-1".to_string(),
            reason: "Insufficient funds".to_string(),
            timestamp: Utc::now(),
        };

        let result = OrderResult::Rejected(rejection);
        assert!(!result.is_filled());
        assert!(result.is_rejected());
        assert_eq!(result.filled_size(), Decimal::ZERO);
        assert!(result.order_id().is_none());
    }

    #[test]
    fn test_order_result_pending() {
        let pending = PendingOrder {
            request_id: "req-1".to_string(),
            order_id: "order-1".to_string(),
            timestamp: Utc::now(),
        };

        let result = OrderResult::Pending(pending);
        assert!(result.is_pending());
        assert_eq!(result.filled_size(), Decimal::ZERO);
        assert_eq!(result.order_id(), Some("order-1"));
    }

    #[test]
    fn test_order_type_display() {
        assert_eq!(format!("{}", OrderType::Limit), "LIMIT");
        assert_eq!(format!("{}", OrderType::Market), "MARKET");
        assert_eq!(format!("{}", OrderType::Ioc), "IOC");
        assert_eq!(format!("{}", OrderType::Gtc), "GTC");
    }

    #[test]
    fn test_executor_error_display() {
        let err = ExecutorError::InsufficientFunds {
            available: dec!(10),
            required: dec!(20),
        };
        assert!(format!("{}", err).contains("10"));
        assert!(format!("{}", err).contains("20"));

        let err = ExecutorError::PositionLimit {
            current: dec!(100),
            max: dec!(50),
        };
        assert!(format!("{}", err).contains("100"));
        assert!(format!("{}", err).contains("50"));
    }
}
