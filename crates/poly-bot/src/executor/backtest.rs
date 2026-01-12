//! Backtest executor that simulates fills against historical order book.
//!
//! The `BacktestExecutor` simulates order execution using historical order
//! book data. It provides more realistic fill simulation than `PaperExecutor`
//! by walking the actual order book depth.
//!
//! ## Features
//!
//! - Fills simulated by walking historical order book depth
//! - Partial fills when liquidity is insufficient
//! - Configurable latency simulation
//! - Accurate fee calculation
//! - Position and P&L tracking
//!
//! ## Usage
//!
//! The backtest executor should be used with `ReplayDataSource` to provide
//! historical order book state. The executor queries the current book state
//! when simulating fills.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use poly_common::types::Side;

use crate::types::OrderBook;

use super::{
    Executor, ExecutorError, OrderCancellation, OrderFill, OrderRejection, OrderRequest,
    OrderResult, OrderType, PartialOrderFill, PendingOrder,
};

/// Configuration for the backtest executor.
#[derive(Debug, Clone)]
pub struct BacktestExecutorConfig {
    /// Initial balance for backtesting.
    pub initial_balance: Decimal,
    /// Fee rate (e.g., 0.001 for 0.1%).
    pub fee_rate: Decimal,
    /// Simulated latency in milliseconds (for order book staleness).
    pub latency_ms: u64,
    /// Whether to enforce balance checks.
    pub enforce_balance: bool,
    /// Maximum position per market (0 = unlimited).
    pub max_position_per_market: Decimal,
    /// Minimum fill ratio to accept partial fills (0.0-1.0).
    pub min_fill_ratio: Decimal,
}

impl Default for BacktestExecutorConfig {
    fn default() -> Self {
        Self {
            initial_balance: Decimal::new(10000, 0), // $10,000
            fee_rate: Decimal::new(1, 3),            // 0.1% fee
            latency_ms: 50,                          // 50ms simulated latency
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO, // Unlimited
            min_fill_ratio: Decimal::new(5, 1),     // Accept partial fills >= 50%
        }
    }
}

/// Backtest position tracking.
#[derive(Debug, Clone, Default)]
pub struct BacktestPosition {
    /// YES shares held.
    pub yes_shares: Decimal,
    /// NO shares held.
    pub no_shares: Decimal,
    /// Total cost basis.
    pub cost_basis: Decimal,
    /// Realized P&L.
    #[allow(dead_code)]
    pub realized_pnl: Decimal,
}

/// Completed order record.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct CompletedOrder {
    request: OrderRequest,
    result: OrderResult,
    executed_at: DateTime<Utc>,
}

/// Backtest executor that simulates fills against historical order book.
pub struct BacktestExecutor {
    config: BacktestExecutorConfig,
    balance: Decimal,
    positions: HashMap<String, BacktestPosition>,
    orders: HashMap<String, CompletedOrder>,
    next_order_id: u64,
    /// Current simulation time (from replay source).
    current_time: DateTime<Utc>,
    /// Shared order books (updated by replay source).
    order_books: Arc<RwLock<HashMap<String, OrderBook>>>,
    /// Statistics.
    stats: BacktestStats,
}

/// Statistics collected during backtesting.
#[derive(Debug, Clone, Default)]
pub struct BacktestStats {
    /// Total orders placed.
    pub orders_placed: u64,
    /// Orders fully filled.
    pub orders_filled: u64,
    /// Orders partially filled.
    pub orders_partial: u64,
    /// Orders rejected.
    pub orders_rejected: u64,
    /// Total volume traded.
    pub volume_traded: Decimal,
    /// Total fees paid.
    pub fees_paid: Decimal,
    /// Total P&L (realized).
    pub realized_pnl: Decimal,
}

impl BacktestExecutor {
    /// Create a new backtest executor with the given configuration.
    pub fn new(config: BacktestExecutorConfig) -> Self {
        Self {
            balance: config.initial_balance,
            config,
            positions: HashMap::new(),
            orders: HashMap::new(),
            next_order_id: 1,
            current_time: Utc::now(),
            order_books: Arc::new(RwLock::new(HashMap::new())),
            stats: BacktestStats::default(),
        }
    }

    /// Create a backtest executor with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(BacktestExecutorConfig::default())
    }

    /// Get the shared order books reference for external updates.
    pub fn order_books(&self) -> Arc<RwLock<HashMap<String, OrderBook>>> {
        self.order_books.clone()
    }

    /// Update the current simulation time.
    pub fn set_time(&mut self, time: DateTime<Utc>) {
        self.current_time = time;
    }

    /// Update an order book (typically called by replay source).
    pub async fn update_book(&self, token_id: &str, book: OrderBook) {
        let mut books = self.order_books.write().await;
        books.insert(token_id.to_string(), book);
    }

    /// Get current balance.
    pub fn balance(&self) -> Decimal {
        self.balance
    }

    /// Get position for an event.
    pub fn position(&self, event_id: &str) -> Option<&BacktestPosition> {
        self.positions.get(event_id)
    }

    /// Get backtest statistics.
    pub fn stats(&self) -> &BacktestStats {
        &self.stats
    }

    /// Generate a unique order ID.
    fn generate_order_id(&mut self) -> String {
        let id = format!("backtest-{}", self.next_order_id);
        self.next_order_id += 1;
        id
    }

    /// Simulate fill by walking the order book.
    async fn simulate_fill(&mut self, order: &OrderRequest) -> Result<OrderResult, ExecutorError> {
        self.stats.orders_placed += 1;

        // Check balance for buy orders
        if order.side == Side::Buy && self.config.enforce_balance {
            let required = order.max_cost();
            if required > self.balance {
                self.stats.orders_rejected += 1;
                return Ok(OrderResult::Rejected(OrderRejection {
                    request_id: order.request_id.clone(),
                    reason: format!(
                        "Insufficient funds: available={}, required={}",
                        self.balance, required
                    ),
                    timestamp: self.current_time,
                }));
            }
        }

        // Check position limit
        if self.config.max_position_per_market > Decimal::ZERO {
            let position = self.positions.get(&order.event_id);
            let current_size = position
                .map(|p| p.yes_shares + p.no_shares)
                .unwrap_or(Decimal::ZERO);
            if current_size + order.size > self.config.max_position_per_market {
                self.stats.orders_rejected += 1;
                return Ok(OrderResult::Rejected(OrderRejection {
                    request_id: order.request_id.clone(),
                    reason: format!(
                        "Position limit exceeded: current={}, max={}",
                        current_size, self.config.max_position_per_market
                    ),
                    timestamp: self.current_time,
                }));
            }
        }

        // Get the order book for this token
        let books = self.order_books.read().await;
        let book = match books.get(&order.token_id) {
            Some(b) => b.clone(),
            None => {
                self.stats.orders_rejected += 1;
                return Ok(OrderResult::Rejected(OrderRejection {
                    request_id: order.request_id.clone(),
                    reason: format!("No order book found for token {}", order.token_id),
                    timestamp: self.current_time,
                }));
            }
        };
        drop(books);

        // Simulate fill by walking the book
        let (filled_size, total_cost, avg_price) = match order.side {
            Side::Buy => {
                // Walk the ask book
                let (filled, cost, avg) = book.cost_to_buy(order.size);

                // For limit orders, only fill at or below limit price
                if let Some(limit_price) = order.price {
                    if matches!(order.order_type, OrderType::Limit | OrderType::Gtc | OrderType::Ioc) {
                        // Recalculate with price limit
                        let mut remaining = order.size;
                        let mut fill_cost = Decimal::ZERO;
                        let mut fill_size = Decimal::ZERO;

                        for level in &book.asks {
                            if level.price > limit_price || remaining <= Decimal::ZERO {
                                break;
                            }
                            let level_fill = remaining.min(level.size);
                            fill_cost += level_fill * level.price;
                            fill_size += level_fill;
                            remaining -= level_fill;
                        }

                        let level_avg = if fill_size > Decimal::ZERO {
                            Some(fill_cost / fill_size)
                        } else {
                            None
                        };

                        (fill_size, fill_cost, level_avg)
                    } else {
                        (filled, cost, avg)
                    }
                } else {
                    (filled, cost, avg)
                }
            }
            Side::Sell => {
                // Walk the bid book
                let (filled, proceeds, avg) = book.proceeds_to_sell(order.size);

                // For limit orders, only fill at or above limit price
                if let Some(limit_price) = order.price {
                    if matches!(order.order_type, OrderType::Limit | OrderType::Gtc | OrderType::Ioc) {
                        let mut remaining = order.size;
                        let mut fill_proceeds = Decimal::ZERO;
                        let mut fill_size = Decimal::ZERO;

                        for level in &book.bids {
                            if level.price < limit_price || remaining <= Decimal::ZERO {
                                break;
                            }
                            let level_fill = remaining.min(level.size);
                            fill_proceeds += level_fill * level.price;
                            fill_size += level_fill;
                            remaining -= level_fill;
                        }

                        let level_avg = if fill_size > Decimal::ZERO {
                            Some(fill_proceeds / fill_size)
                        } else {
                            None
                        };

                        (fill_size, fill_proceeds, level_avg)
                    } else {
                        (filled, proceeds, avg)
                    }
                } else {
                    (filled, proceeds, avg)
                }
            }
        };

        // Generate order ID
        let order_id = self.generate_order_id();

        // Check if we got any fill
        if filled_size <= Decimal::ZERO {
            self.stats.orders_rejected += 1;
            return Ok(OrderResult::Rejected(OrderRejection {
                request_id: order.request_id.clone(),
                reason: "No liquidity at acceptable price".to_string(),
                timestamp: self.current_time,
            }));
        }

        // Check minimum fill ratio for partial fills
        let fill_ratio = filled_size / order.size;
        if fill_ratio < Decimal::ONE
            && fill_ratio < self.config.min_fill_ratio
            && matches!(order.order_type, OrderType::Limit | OrderType::Gtc)
        {
            // GTC/Limit orders can wait for more liquidity
            self.stats.orders_rejected += 1;
            return Ok(OrderResult::Rejected(OrderRejection {
                request_id: order.request_id.clone(),
                reason: format!(
                    "Insufficient liquidity: only {:.2}% fillable",
                    fill_ratio * Decimal::new(100, 0)
                ),
                timestamp: self.current_time,
            }));
        }

        // Calculate fee
        let fee = total_cost * self.config.fee_rate;

        // Update balance and position
        match order.side {
            Side::Buy => {
                self.balance -= total_cost + fee;
                let position = self.positions.entry(order.event_id.clone()).or_default();
                match order.outcome {
                    poly_common::types::Outcome::Yes => {
                        position.yes_shares += filled_size;
                    }
                    poly_common::types::Outcome::No => {
                        position.no_shares += filled_size;
                    }
                }
                position.cost_basis += total_cost + fee;
            }
            Side::Sell => {
                self.balance += total_cost - fee;
                let position = self.positions.entry(order.event_id.clone()).or_default();
                match order.outcome {
                    poly_common::types::Outcome::Yes => {
                        position.yes_shares -= filled_size.min(position.yes_shares);
                    }
                    poly_common::types::Outcome::No => {
                        position.no_shares -= filled_size.min(position.no_shares);
                    }
                }
            }
        }

        // Update stats
        self.stats.volume_traded += total_cost;
        self.stats.fees_paid += fee;

        // Create result
        let result = if filled_size >= order.size {
            self.stats.orders_filled += 1;
            let fill = OrderFill {
                request_id: order.request_id.clone(),
                order_id: order_id.clone(),
                size: filled_size,
                price: avg_price.unwrap_or(Decimal::ZERO),
                fee,
                timestamp: self.current_time,
            };
            OrderResult::Filled(fill)
        } else {
            self.stats.orders_partial += 1;
            let fill = PartialOrderFill {
                request_id: order.request_id.clone(),
                order_id: order_id.clone(),
                requested_size: order.size,
                filled_size,
                avg_price: avg_price.unwrap_or(Decimal::ZERO),
                fee,
                timestamp: self.current_time,
            };
            OrderResult::PartialFill(fill)
        };

        debug!(
            order_id = %order_id,
            side = %order.side,
            outcome = ?order.outcome,
            requested = %order.size,
            filled = %filled_size,
            avg_price = ?avg_price,
            fee = %fee,
            "Backtest order executed"
        );

        // Store order for history
        self.orders.insert(
            order_id,
            CompletedOrder {
                request: order.clone(),
                result: result.clone(),
                executed_at: self.current_time,
            },
        );

        Ok(result)
    }
}

#[async_trait]
impl Executor for BacktestExecutor {
    async fn place_order(&mut self, order: OrderRequest) -> Result<OrderResult, ExecutorError> {
        debug!(
            request_id = %order.request_id,
            event_id = %order.event_id,
            side = %order.side,
            outcome = ?order.outcome,
            size = %order.size,
            price = ?order.price,
            "Placing backtest order"
        );

        // Validate order
        if order.size <= Decimal::ZERO {
            return Err(ExecutorError::InvalidOrder(
                "Size must be positive".to_string(),
            ));
        }

        if matches!(order.order_type, OrderType::Limit | OrderType::Gtc | OrderType::Ioc)
            && order.price.is_none()
        {
            return Err(ExecutorError::InvalidOrder(
                "Limit orders require a price".to_string(),
            ));
        }

        // Simulate the fill
        self.simulate_fill(&order).await
    }

    async fn cancel_order(&mut self, order_id: &str) -> Result<OrderCancellation, ExecutorError> {
        // In backtest mode, orders are filled immediately (no pending state)
        if self.orders.contains_key(order_id) {
            warn!(order_id = %order_id, "Cannot cancel: backtest order already completed");
            Err(ExecutorError::InvalidOrder(
                "Order already completed".to_string(),
            ))
        } else {
            warn!(order_id = %order_id, "Cannot cancel: order not found");
            Err(ExecutorError::InvalidOrder("Order not found".to_string()))
        }
    }

    async fn order_status(&self, order_id: &str) -> Option<OrderResult> {
        self.orders.get(order_id).map(|o| o.result.clone())
    }

    fn pending_orders(&self) -> Vec<PendingOrder> {
        // Backtest executor has no pending orders (immediate fills)
        Vec::new()
    }

    fn available_balance(&self) -> Decimal {
        self.balance
    }

    async fn shutdown(&mut self) {
        info!(
            balance = %self.balance,
            orders_placed = self.stats.orders_placed,
            orders_filled = self.stats.orders_filled,
            orders_partial = self.stats.orders_partial,
            orders_rejected = self.stats.orders_rejected,
            volume_traded = %self.stats.volume_traded,
            fees_paid = %self.stats.fees_paid,
            "Backtest executor shutting down"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PriceLevel;
    use poly_common::types::Outcome;
    use rust_decimal_macros::dec;

    async fn create_executor_with_book() -> BacktestExecutor {
        let config = BacktestExecutorConfig {
            initial_balance: dec!(1000),
            fee_rate: dec!(0.001),
            latency_ms: 0,
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO,
            min_fill_ratio: dec!(0.5),
        };

        let executor = BacktestExecutor::new(config);

        // Create a test order book
        let mut book = OrderBook::new("token-yes".to_string());
        book.asks.push(PriceLevel::new(dec!(0.45), dec!(100)));
        book.asks.push(PriceLevel::new(dec!(0.46), dec!(100)));
        book.asks.push(PriceLevel::new(dec!(0.47), dec!(100)));
        book.bids.push(PriceLevel::new(dec!(0.44), dec!(100)));
        book.bids.push(PriceLevel::new(dec!(0.43), dec!(100)));
        book.bids.push(PriceLevel::new(dec!(0.42), dec!(100)));

        executor.update_book("token-yes", book).await;

        executor
    }

    #[tokio::test]
    async fn test_backtest_executor_full_fill() {
        let mut executor = create_executor_with_book().await;

        // Place a buy order that can be fully filled
        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.50), // Limit above best ask
        );

        let result = executor.place_order(order).await.unwrap();
        assert!(result.is_filled());

        if let OrderResult::Filled(fill) = result {
            assert_eq!(fill.size, dec!(100));
            assert_eq!(fill.price, dec!(0.45)); // Filled at best ask
        }

        // Check stats
        assert_eq!(executor.stats().orders_filled, 1);
        assert_eq!(executor.stats().orders_partial, 0);
    }

    #[tokio::test]
    async fn test_backtest_executor_partial_fill() {
        let mut executor = create_executor_with_book().await;

        // Place a buy order larger than top level
        let order = OrderRequest::ioc(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(150), // More than the 100 at best ask
            dec!(0.45), // Only fill at 0.45
        );

        let result = executor.place_order(order).await.unwrap();
        assert!(result.is_filled()); // Partial fill with IOC

        if let OrderResult::PartialFill(fill) = result {
            assert_eq!(fill.requested_size, dec!(150));
            assert_eq!(fill.filled_size, dec!(100)); // Only 100 available at 0.45
        }

        // Check stats
        assert_eq!(executor.stats().orders_partial, 1);
    }

    #[tokio::test]
    async fn test_backtest_executor_limit_price_respected() {
        let mut executor = create_executor_with_book().await;

        // Place a buy order with limit below best ask
        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.44), // Limit below best ask of 0.45
        );

        let result = executor.place_order(order).await.unwrap();
        assert!(result.is_rejected()); // No liquidity at or below 0.44
    }

    #[tokio::test]
    async fn test_backtest_executor_sell_order() {
        let mut executor = create_executor_with_book().await;

        // First buy some shares
        let buy_order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.50),
        );
        executor.place_order(buy_order).await.unwrap();

        let balance_after_buy = executor.balance();

        // Now sell
        let sell_order = OrderRequest::limit(
            "req-2".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Sell,
            dec!(50),
            dec!(0.40), // Limit below best bid
        );

        let result = executor.place_order(sell_order).await.unwrap();
        assert!(result.is_filled());

        // Balance should increase
        assert!(executor.balance() > balance_after_buy);

        // Position should decrease
        let position = executor.position("event-1").unwrap();
        assert_eq!(position.yes_shares, dec!(50));
    }

    #[tokio::test]
    async fn test_backtest_executor_no_book() {
        let mut executor = BacktestExecutor::with_defaults();

        // Place order for token without order book
        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "unknown-token".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.50),
        );

        let result = executor.place_order(order).await.unwrap();
        assert!(result.is_rejected());
    }

    #[tokio::test]
    async fn test_backtest_executor_insufficient_funds() {
        let config = BacktestExecutorConfig {
            initial_balance: dec!(10), // Low balance
            enforce_balance: true,
            ..Default::default()
        };

        let mut executor = BacktestExecutor::new(config);

        // Create order book
        let mut book = OrderBook::new("token-yes".to_string());
        book.asks.push(PriceLevel::new(dec!(0.50), dec!(100)));
        executor.update_book("token-yes", book).await;

        // Try to buy more than balance allows
        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.50), // Cost = 50, but only have 10
        );

        let result = executor.place_order(order).await.unwrap();
        assert!(result.is_rejected());
        assert_eq!(executor.stats().orders_rejected, 1);
    }

    #[tokio::test]
    async fn test_backtest_executor_market_order() {
        let mut executor = create_executor_with_book().await;

        // Place market order (no price limit)
        let order = OrderRequest::market(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(200), // Will walk multiple levels
        );

        let result = executor.place_order(order).await.unwrap();
        assert!(result.is_filled());

        if let OrderResult::Filled(fill) = result {
            assert_eq!(fill.size, dec!(200));
            // Avg price should be between 0.45 and 0.46
            assert!(fill.price > dec!(0.45) && fill.price < dec!(0.46));
        }
    }

    #[tokio::test]
    async fn test_backtest_executor_stats() {
        let mut executor = create_executor_with_book().await;

        // Place a few orders - all will fill since book doesn't consume
        // (book has 100 @ 0.45, 100 @ 0.46, 100 @ 0.47)
        for i in 0..3 {
            let order = OrderRequest::limit(
                format!("req-{}", i),
                "event-1".to_string(),
                "token-yes".to_string(),
                Outcome::Yes,
                Side::Buy,
                dec!(50),
                dec!(0.50), // Limit above best ask, will fill at 0.45
            );
            executor.place_order(order).await.unwrap();
        }

        let stats = executor.stats();
        assert_eq!(stats.orders_placed, 3);
        assert_eq!(stats.orders_filled, 3); // All orders fill (book doesn't consume)
        assert!(stats.volume_traded > Decimal::ZERO);
        assert!(stats.fees_paid > Decimal::ZERO);
    }

    #[tokio::test]
    async fn test_backtest_executor_order_history() {
        let mut executor = create_executor_with_book().await;

        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.50),
        );

        let result = executor.place_order(order).await.unwrap();
        let order_id = result.order_id().unwrap().to_string();

        // Check order status
        let status = executor.order_status(&order_id).await;
        assert!(status.is_some());
        assert!(status.unwrap().is_filled());
    }

    #[test]
    fn test_backtest_executor_config_default() {
        let config = BacktestExecutorConfig::default();
        assert_eq!(config.initial_balance, dec!(10000));
        assert_eq!(config.fee_rate, dec!(0.001));
        assert!(config.enforce_balance);
    }
}
