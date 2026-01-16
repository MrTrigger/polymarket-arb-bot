//! Unified simulated executor for paper trading and backtesting.
//!
//! The `SimulatedExecutor` provides order execution simulation that works for both:
//! - Paper trading with live data (real-time timestamps, optional latency)
//! - Backtesting with historical data (simulated time, order book fills)
//!
//! ## Configuration Modes
//!
//! The executor behavior is controlled by three configuration options:
//!
//! - **TimeSource**: `WallClock` uses real time, `Simulated` uses externally set time
//! - **FillMode**: `Simple` fills at limit price, `OrderBook` walks the book for fills
//! - **LatencyMode**: `RealDelay` adds actual latency, `Instant` has no delay
//!
//! ## Typical Configurations
//!
//! | Mode     | TimeSource | FillMode  | LatencyMode |
//! |----------|------------|-----------|-------------|
//! | Paper    | WallClock  | Simple    | RealDelay   |
//! | Backtest | Simulated  | OrderBook | Instant     |

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use poly_common::types::Side;

use crate::types::{OrderBook, PriceLevel};

use super::{
    Executor, ExecutorError, OrderCancellation, OrderFill, OrderRejection, OrderRequest,
    OrderResult, OrderType, PartialOrderFill, PendingOrder,
};

/// Time source for the executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSource {
    /// Use real wall-clock time (Utc::now()).
    WallClock,
    /// Use externally set simulated time.
    Simulated,
}

/// Fill simulation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillMode {
    /// Simple fill at limit price (or market price with slippage).
    Simple,
    /// Walk order book for realistic fills with partial fill support.
    OrderBook,
}

/// Latency simulation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyMode {
    /// Add real delay using tokio::sleep.
    RealDelay,
    /// No delay (instant execution).
    Instant,
}

/// Configuration for the simulated executor.
#[derive(Debug, Clone)]
pub struct SimulatedExecutorConfig {
    /// Initial balance.
    pub initial_balance: Decimal,
    /// Fee rate (e.g., 0.001 for 0.1%).
    pub fee_rate: Decimal,
    /// Time source mode.
    pub time_source: TimeSource,
    /// Fill simulation mode.
    pub fill_mode: FillMode,
    /// Latency simulation mode.
    pub latency_mode: LatencyMode,
    /// Simulated latency in milliseconds (only used with RealDelay).
    pub latency_ms: u64,
    /// Whether to enforce balance checks.
    pub enforce_balance: bool,
    /// Maximum position per market (0 = unlimited).
    pub max_position_per_market: Decimal,
    /// Market order slippage factor (e.g., 0.001 for 0.1%, Simple mode only).
    pub market_order_slippage: Decimal,
    /// Minimum fill ratio to accept partial fills (0.0-1.0, OrderBook mode only).
    pub min_fill_ratio: Decimal,
}

impl SimulatedExecutorConfig {
    /// Create configuration for paper trading.
    pub fn paper() -> Self {
        Self {
            initial_balance: Decimal::new(10000, 0), // $10,000
            fee_rate: Decimal::new(1, 3),            // 0.1%
            time_source: TimeSource::WallClock,
            fill_mode: FillMode::Simple,
            latency_mode: LatencyMode::RealDelay,
            latency_ms: 50,
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO,
            market_order_slippage: Decimal::new(1, 3), // 0.1%
            min_fill_ratio: Decimal::new(5, 1),        // 50%
        }
    }

    /// Create configuration for backtesting.
    pub fn backtest() -> Self {
        Self {
            initial_balance: Decimal::new(10000, 0), // $10,000
            fee_rate: Decimal::new(1, 3),            // 0.1%
            time_source: TimeSource::Simulated,
            fill_mode: FillMode::OrderBook,
            latency_mode: LatencyMode::Instant,
            latency_ms: 0,
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO,
            market_order_slippage: Decimal::ZERO,
            min_fill_ratio: Decimal::new(5, 1), // 50%
        }
    }
}

impl Default for SimulatedExecutorConfig {
    fn default() -> Self {
        Self::paper()
    }
}

/// Position tracking for simulated execution.
#[derive(Debug, Clone, Default)]
pub struct SimulatedPosition {
    /// YES shares held.
    pub yes_shares: Decimal,
    /// NO shares held.
    pub no_shares: Decimal,
    /// Total cost basis.
    pub cost_basis: Decimal,
}

/// Completed order record.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct CompletedOrder {
    request: OrderRequest,
    result: OrderResult,
    executed_at: DateTime<Utc>,
}

/// Statistics collected during simulation.
#[derive(Debug, Clone, Default)]
pub struct SimulatedStats {
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
}

/// Unified simulated executor for paper trading and backtesting.
pub struct SimulatedExecutor {
    config: SimulatedExecutorConfig,
    balance: Decimal,
    positions: HashMap<String, SimulatedPosition>,
    orders: HashMap<String, CompletedOrder>,
    pending: HashMap<String, PendingOrder>,
    next_order_id: u64,
    /// Current simulation time (used when time_source is Simulated).
    simulated_time: DateTime<Utc>,
    /// Order books for OrderBook fill mode.
    order_books: Arc<RwLock<HashMap<String, OrderBook>>>,
    /// Market prices for Simple fill mode.
    market_prices: Arc<RwLock<HashMap<String, Decimal>>>,
    /// Execution statistics.
    stats: SimulatedStats,
}

impl SimulatedExecutor {
    /// Create a new simulated executor with the given configuration.
    pub fn new(config: SimulatedExecutorConfig) -> Self {
        Self {
            balance: config.initial_balance,
            config,
            positions: HashMap::new(),
            orders: HashMap::new(),
            pending: HashMap::new(),
            next_order_id: 1,
            simulated_time: Utc::now(),
            order_books: Arc::new(RwLock::new(HashMap::new())),
            market_prices: Arc::new(RwLock::new(HashMap::new())),
            stats: SimulatedStats::default(),
        }
    }

    /// Create a paper trading executor with default configuration.
    pub fn paper() -> Self {
        Self::new(SimulatedExecutorConfig::paper())
    }

    /// Create a backtest executor with default configuration.
    pub fn backtest() -> Self {
        Self::new(SimulatedExecutorConfig::backtest())
    }

    /// Get current time based on configuration.
    fn current_time(&self) -> DateTime<Utc> {
        match self.config.time_source {
            TimeSource::WallClock => Utc::now(),
            TimeSource::Simulated => self.simulated_time,
        }
    }

    /// Set the simulated time (only effective when time_source is Simulated).
    pub fn set_time(&mut self, time: DateTime<Utc>) {
        self.simulated_time = time;
    }

    /// Get the shared order books reference for external updates.
    pub fn order_books(&self) -> Arc<RwLock<HashMap<String, OrderBook>>> {
        self.order_books.clone()
    }

    /// Get the shared market prices reference for external updates.
    pub fn market_prices(&self) -> Arc<RwLock<HashMap<String, Decimal>>> {
        self.market_prices.clone()
    }

    /// Update a market price (for Simple fill mode).
    pub async fn update_price(&self, token_id: &str, price: Decimal) {
        let mut prices = self.market_prices.write().await;
        prices.insert(token_id.to_string(), price);
    }

    /// Update an order book (for OrderBook fill mode).
    pub async fn update_book(&self, token_id: &str, book: OrderBook) {
        let mut books = self.order_books.write().await;
        books.insert(token_id.to_string(), book);
    }

    /// Get current balance.
    pub fn balance(&self) -> Decimal {
        self.balance
    }

    /// Get position for an event.
    pub fn position(&self, event_id: &str) -> Option<&SimulatedPosition> {
        self.positions.get(event_id)
    }

    /// Get total position value (approximate).
    pub fn total_position_value(&self) -> Decimal {
        self.positions
            .values()
            .map(|p| (p.yes_shares + p.no_shares) * Decimal::new(5, 1))
            .sum()
    }

    /// Get execution statistics.
    pub fn stats(&self) -> &SimulatedStats {
        &self.stats
    }

    /// Generate a unique order ID.
    fn generate_order_id(&mut self) -> String {
        let prefix = match self.config.time_source {
            TimeSource::WallClock => "paper",
            TimeSource::Simulated => "backtest",
        };
        let id = format!("{}-{}", prefix, self.next_order_id);
        self.next_order_id += 1;
        id
    }

    /// Simulate order fill based on configuration.
    async fn simulate_fill(&mut self, order: &OrderRequest) -> Result<OrderResult, ExecutorError> {
        self.stats.orders_placed += 1;
        let current_time = self.current_time();

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
                    timestamp: current_time,
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
                    timestamp: current_time,
                }));
            }
        }

        // Apply latency
        if matches!(self.config.latency_mode, LatencyMode::RealDelay) && self.config.latency_ms > 0
        {
            tokio::time::sleep(tokio::time::Duration::from_millis(self.config.latency_ms)).await;
        }

        // Simulate fill based on mode
        match self.config.fill_mode {
            FillMode::Simple => self.simulate_simple_fill(order, current_time).await,
            FillMode::OrderBook => self.simulate_orderbook_fill(order, current_time).await,
        }
    }

    /// Simple fill simulation (fills at limit price or market price with slippage).
    async fn simulate_simple_fill(
        &mut self,
        order: &OrderRequest,
        current_time: DateTime<Utc>,
    ) -> Result<OrderResult, ExecutorError> {
        // Determine fill price
        let fill_price = match order.order_type {
            OrderType::Market => {
                // Use market price with slippage
                let base_price = {
                    let prices = self.market_prices.read().await;
                    prices
                        .get(&order.token_id)
                        .copied()
                        .unwrap_or_else(|| order.price.unwrap_or(Decimal::new(5, 1)))
                };

                match order.side {
                    Side::Buy => base_price * (Decimal::ONE + self.config.market_order_slippage),
                    Side::Sell => base_price * (Decimal::ONE - self.config.market_order_slippage),
                }
            }
            OrderType::Limit | OrderType::Gtc | OrderType::Ioc => {
                // Fill at limit price
                order.price.unwrap_or(Decimal::new(5, 1))
            }
        };

        // Calculate fill cost and fee
        let fill_cost = fill_price * order.size;
        let fee = fill_cost * self.config.fee_rate;

        // Generate order ID
        let order_id = self.generate_order_id();

        // Update balance and position
        self.apply_fill(order, order.size, fill_cost, fee);

        // Update stats
        self.stats.orders_filled += 1;
        self.stats.volume_traded += fill_cost;
        self.stats.fees_paid += fee;

        let fill = OrderFill {
            request_id: order.request_id.clone(),
            order_id: order_id.clone(),
            size: order.size,
            price: fill_price,
            fee,
            timestamp: current_time,
        };

        info!(
            order_id = %order_id,
            side = %order.side,
            outcome = ?order.outcome,
            size = %order.size,
            price = %fill_price,
            fee = %fee,
            "Simulated order filled (simple)"
        );

        let result = OrderResult::Filled(fill);

        // Store order for history
        self.orders.insert(
            order_id,
            CompletedOrder {
                request: order.clone(),
                result: result.clone(),
                executed_at: current_time,
            },
        );

        Ok(result)
    }

    /// Order book fill simulation (walks the book for realistic fills).
    async fn simulate_orderbook_fill(
        &mut self,
        order: &OrderRequest,
        current_time: DateTime<Utc>,
    ) -> Result<OrderResult, ExecutorError> {
        // Get the order book for this token
        let books = self.order_books.read().await;
        let book = match books.get(&order.token_id) {
            Some(b) => b.clone(),
            None => {
                self.stats.orders_rejected += 1;
                return Ok(OrderResult::Rejected(OrderRejection {
                    request_id: order.request_id.clone(),
                    reason: format!("No order book found for token {}", order.token_id),
                    timestamp: current_time,
                }));
            }
        };
        drop(books);

        // Simulate fill by walking the book
        let (filled_size, total_cost, avg_price) = match order.side {
            Side::Buy => book.cost_to_buy(order.size),
            Side::Sell => book.proceeds_to_sell(order.size),
        };

        // Generate order ID
        let order_id = self.generate_order_id();

        // Check if we got any fill
        if filled_size <= Decimal::ZERO {
            self.stats.orders_rejected += 1;
            return Ok(OrderResult::Rejected(OrderRejection {
                request_id: order.request_id.clone(),
                reason: "No liquidity at acceptable price".to_string(),
                timestamp: current_time,
            }));
        }

        // Check minimum fill ratio for partial fills
        let fill_ratio = filled_size / order.size;
        if fill_ratio < Decimal::ONE
            && fill_ratio < self.config.min_fill_ratio
            && matches!(order.order_type, OrderType::Limit | OrderType::Gtc)
        {
            self.stats.orders_rejected += 1;
            return Ok(OrderResult::Rejected(OrderRejection {
                request_id: order.request_id.clone(),
                reason: format!(
                    "Insufficient liquidity: only {:.2}% fillable",
                    fill_ratio * Decimal::new(100, 0)
                ),
                timestamp: current_time,
            }));
        }

        // Calculate fee
        let fee = total_cost * self.config.fee_rate;

        // Update balance and position
        self.apply_fill(order, filled_size, total_cost, fee);

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
                timestamp: current_time,
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
                timestamp: current_time,
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
            "Simulated order executed (order book)"
        );

        // Store order for history
        self.orders.insert(
            order_id,
            CompletedOrder {
                request: order.clone(),
                result: result.clone(),
                executed_at: current_time,
            },
        );

        Ok(result)
    }

    /// Apply fill to balance and position.
    fn apply_fill(&mut self, order: &OrderRequest, filled_size: Decimal, cost: Decimal, fee: Decimal) {
        match order.side {
            Side::Buy => {
                self.balance -= cost + fee;
                let position = self.positions.entry(order.event_id.clone()).or_default();
                match order.outcome {
                    poly_common::types::Outcome::Yes => {
                        position.yes_shares += filled_size;
                    }
                    poly_common::types::Outcome::No => {
                        position.no_shares += filled_size;
                    }
                }
                position.cost_basis += cost + fee;
            }
            Side::Sell => {
                self.balance += cost - fee;
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
    }

    /// Settle a market position when it expires.
    fn settle_position(&mut self, event_id: &str, yes_wins: bool) -> Decimal {
        let position = match self.positions.remove(event_id) {
            Some(p) => p,
            None => return Decimal::ZERO,
        };

        // Calculate payout: winning side pays $1.00 per share
        let payout = if yes_wins {
            position.yes_shares
        } else {
            position.no_shares
        };

        // Calculate realized PnL
        let realized_pnl = payout - position.cost_basis;

        // Update balance with payout
        self.balance += payout;

        info!(
            event_id = %event_id,
            yes_wins = %yes_wins,
            yes_shares = %position.yes_shares,
            no_shares = %position.no_shares,
            cost_basis = %position.cost_basis,
            payout = %payout,
            realized_pnl = %realized_pnl,
            new_balance = %self.balance,
            "Market settled"
        );

        realized_pnl
    }
}

#[async_trait]
impl Executor for SimulatedExecutor {
    async fn place_order(&mut self, order: OrderRequest) -> Result<OrderResult, ExecutorError> {
        debug!(
            request_id = %order.request_id,
            event_id = %order.event_id,
            side = %order.side,
            outcome = ?order.outcome,
            size = %order.size,
            price = ?order.price,
            "Placing simulated order"
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

        self.simulate_fill(&order).await
    }

    async fn cancel_order(&mut self, order_id: &str) -> Result<OrderCancellation, ExecutorError> {
        // Check if order exists and is pending
        if let Some(pending) = self.pending.remove(order_id) {
            let cancellation = OrderCancellation {
                request_id: pending.request_id,
                order_id: order_id.to_string(),
                filled_size: Decimal::ZERO,
                unfilled_size: Decimal::ZERO,
                timestamp: self.current_time(),
            };

            info!(order_id = %order_id, "Order cancelled");
            Ok(cancellation)
        } else if self.orders.contains_key(order_id) {
            warn!(order_id = %order_id, "Cannot cancel: order already completed");
            Err(ExecutorError::InvalidOrder(
                "Order already completed".to_string(),
            ))
        } else {
            warn!(order_id = %order_id, "Cannot cancel: order not found");
            Err(ExecutorError::InvalidOrder("Order not found".to_string()))
        }
    }

    async fn order_status(&self, order_id: &str) -> Option<OrderResult> {
        // Check pending orders
        if let Some(pending) = self.pending.get(order_id) {
            return Some(OrderResult::Pending(pending.clone()));
        }

        // Check completed orders
        self.orders.get(order_id).map(|o| o.result.clone())
    }

    fn pending_orders(&self) -> Vec<PendingOrder> {
        self.pending.values().cloned().collect()
    }

    fn available_balance(&self) -> Decimal {
        self.balance
    }

    async fn settle_market(&mut self, event_id: &str, yes_wins: bool) -> Decimal {
        self.settle_position(event_id, yes_wins)
    }

    async fn update_order_book(
        &self,
        token_id: &str,
        bids: Vec<PriceLevel>,
        asks: Vec<PriceLevel>,
    ) {
        let mut book = OrderBook::new(token_id.to_string());
        book.bids = bids;
        book.asks = asks;
        self.update_book(token_id, book).await;
    }

    async fn shutdown(&mut self) {
        let mode = match self.config.time_source {
            TimeSource::WallClock => "paper",
            TimeSource::Simulated => "backtest",
        };
        info!(
            mode = %mode,
            balance = %self.balance,
            orders_placed = self.stats.orders_placed,
            orders_filled = self.stats.orders_filled,
            orders_partial = self.stats.orders_partial,
            orders_rejected = self.stats.orders_rejected,
            volume_traded = %self.stats.volume_traded,
            fees_paid = %self.stats.fees_paid,
            "Simulated executor shutting down"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poly_common::types::Outcome;
    use rust_decimal_macros::dec;

    // Helper to create executor with test order book
    async fn create_executor_with_book() -> SimulatedExecutor {
        let mut config = SimulatedExecutorConfig::backtest();
        config.initial_balance = dec!(1000);
        config.latency_ms = 0;

        let executor = SimulatedExecutor::new(config);

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
    async fn test_paper_mode_simple_fill() {
        let mut config = SimulatedExecutorConfig::paper();
        config.initial_balance = dec!(1000);
        config.latency_ms = 0; // No latency for tests
        config.fee_rate = dec!(0.001);

        let mut executor = SimulatedExecutor::new(config);
        assert_eq!(executor.balance(), dec!(1000));

        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.45),
        );

        let result = executor.place_order(order).await.unwrap();
        assert!(result.is_filled());

        // Check balance: 1000 - (100 * 0.45) - fee = 1000 - 45 - 0.045 = 954.955
        let expected_balance = dec!(1000) - dec!(45) - dec!(0.045);
        assert_eq!(executor.balance(), expected_balance);

        // Check position
        let position = executor.position("event-1").unwrap();
        assert_eq!(position.yes_shares, dec!(100));
    }

    #[tokio::test]
    async fn test_backtest_mode_orderbook_fill() {
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
        assert!(result.is_filled());

        if let OrderResult::Filled(fill) = result {
            assert_eq!(fill.size, dec!(100));
            assert_eq!(fill.price, dec!(0.45)); // Filled at best ask
        }

        assert_eq!(executor.stats().orders_filled, 1);
    }

    #[tokio::test]
    async fn test_insufficient_funds() {
        let mut config = SimulatedExecutorConfig::paper();
        config.initial_balance = dec!(10);
        config.latency_ms = 0;

        let mut executor = SimulatedExecutor::new(config);

        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.45), // Cost = 45, but only have 10
        );

        let result = executor.place_order(order).await.unwrap();
        assert!(result.is_rejected());
    }

    #[tokio::test]
    async fn test_sell_order() {
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
            dec!(0.40),
        );

        let result = executor.place_order(sell_order).await.unwrap();
        assert!(result.is_filled());
        assert!(executor.balance() > balance_after_buy);

        let position = executor.position("event-1").unwrap();
        assert_eq!(position.yes_shares, dec!(50));
    }

    #[tokio::test]
    async fn test_settlement_yes_wins() {
        let mut config = SimulatedExecutorConfig::paper();
        config.initial_balance = dec!(1000);
        config.latency_ms = 0;
        config.fee_rate = Decimal::ZERO;

        let mut executor = SimulatedExecutor::new(config);

        // Buy 100 YES at $0.40
        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.40),
        );
        executor.place_order(order).await.unwrap();

        // Balance = $1000 - $40 = $960
        assert_eq!(executor.balance(), dec!(960));

        // Settle with YES winning: payout = $100, PnL = $100 - $40 = $60
        let pnl = executor.settle_market("event-1", true).await;
        assert_eq!(pnl, dec!(60));
        assert_eq!(executor.balance(), dec!(1060));
    }

    #[tokio::test]
    async fn test_settlement_no_wins() {
        let mut config = SimulatedExecutorConfig::paper();
        config.initial_balance = dec!(1000);
        config.latency_ms = 0;
        config.fee_rate = Decimal::ZERO;

        let mut executor = SimulatedExecutor::new(config);

        // Buy 100 YES at $0.80
        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.80),
        );
        executor.place_order(order).await.unwrap();

        // Settle with NO winning: payout = $0, PnL = $0 - $80 = -$80
        let pnl = executor.settle_market("event-1", false).await;
        assert_eq!(pnl, dec!(-80));
        assert_eq!(executor.balance(), dec!(920));
    }

    #[tokio::test]
    async fn test_no_order_book() {
        let config = SimulatedExecutorConfig::backtest();
        let mut executor = SimulatedExecutor::new(config);

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
    async fn test_simulated_time() {
        let mut executor = SimulatedExecutor::backtest();

        let custom_time = chrono::DateTime::parse_from_rfc3339("2025-01-15T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        executor.set_time(custom_time);
        assert_eq!(executor.current_time(), custom_time);
    }

    #[tokio::test]
    async fn test_stats_tracking() {
        let mut executor = create_executor_with_book().await;

        for i in 0..3 {
            let order = OrderRequest::limit(
                format!("req-{}", i),
                "event-1".to_string(),
                "token-yes".to_string(),
                Outcome::Yes,
                Side::Buy,
                dec!(50),
                dec!(0.50),
            );
            executor.place_order(order).await.unwrap();
        }

        let stats = executor.stats();
        assert_eq!(stats.orders_placed, 3);
        assert_eq!(stats.orders_filled, 3);
        assert!(stats.volume_traded > Decimal::ZERO);
        assert!(stats.fees_paid > Decimal::ZERO);
    }
}
