//! Paper trading executor with simulated fills.
//!
//! The `PaperExecutor` simulates order execution without real money.
//! It's useful for:
//! - Testing strategy logic with live market data
//! - Validating risk management before going live
//! - Debugging order flow and timing
//!
//! ## Simulation Features
//!
//! - Configurable fill latency (simulates network/exchange delay)
//! - Fill probability based on order vs market price
//! - Simulated fees
//! - Balance and position tracking
//!
//! ## Limitations
//!
//! - Does not simulate market impact
//! - Does not simulate partial fills (all-or-nothing)
//! - Assumes infinite liquidity at best ask/bid

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use poly_common::types::Side;

use super::{
    Executor, ExecutorError, OrderCancellation, OrderFill, OrderRejection, OrderRequest,
    OrderResult, OrderType, PendingOrder,
};

/// Configuration for the paper executor.
#[derive(Debug, Clone)]
pub struct PaperExecutorConfig {
    /// Initial balance for paper trading.
    pub initial_balance: Decimal,
    /// Simulated fill latency in milliseconds.
    pub fill_latency_ms: u64,
    /// Fee rate (e.g., 0.001 for 0.1%).
    pub fee_rate: Decimal,
    /// Whether to reject orders when balance is insufficient.
    pub enforce_balance: bool,
    /// Maximum position per market (0 = unlimited).
    pub max_position_per_market: Decimal,
    /// Slippage factor for market orders (e.g., 0.001 for 0.1%).
    pub market_order_slippage: Decimal,
}

impl Default for PaperExecutorConfig {
    fn default() -> Self {
        Self {
            initial_balance: Decimal::new(10000, 0), // $10,000
            fill_latency_ms: 50,                     // 50ms simulated latency
            fee_rate: Decimal::new(1, 3),            // 0.1% fee
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO, // Unlimited
            market_order_slippage: Decimal::new(1, 3), // 0.1% slippage
        }
    }
}

/// Simulated position for paper trading.
#[derive(Debug, Clone, Default)]
pub struct PaperPosition {
    /// YES shares held.
    pub yes_shares: Decimal,
    /// NO shares held.
    pub no_shares: Decimal,
    /// Total cost basis.
    pub cost_basis: Decimal,
}

/// Simulated order for tracking.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct SimulatedOrder {
    request: OrderRequest,
    result: OrderResult,
    created_at: DateTime<Utc>,
}

/// Paper trading executor.
///
/// Simulates order execution without real money. Tracks balance,
/// positions, and order history for strategy validation.
pub struct PaperExecutor {
    config: PaperExecutorConfig,
    balance: Decimal,
    positions: HashMap<String, PaperPosition>,
    orders: HashMap<String, SimulatedOrder>,
    pending: HashMap<String, PendingOrder>,
    next_order_id: u64,
    /// Shared state for getting current prices (optional).
    market_prices: Arc<RwLock<HashMap<String, Decimal>>>,
}

impl PaperExecutor {
    /// Create a new paper executor with the given configuration.
    pub fn new(config: PaperExecutorConfig) -> Self {
        Self {
            balance: config.initial_balance,
            config,
            positions: HashMap::new(),
            orders: HashMap::new(),
            pending: HashMap::new(),
            next_order_id: 1,
            market_prices: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a paper executor with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(PaperExecutorConfig::default())
    }

    /// Get the shared market prices reference for external updates.
    pub fn market_prices(&self) -> Arc<RwLock<HashMap<String, Decimal>>> {
        self.market_prices.clone()
    }

    /// Update a market price (for fill simulation).
    pub async fn update_price(&self, token_id: &str, price: Decimal) {
        let mut prices = self.market_prices.write().await;
        prices.insert(token_id.to_string(), price);
    }

    /// Get current balance.
    pub fn balance(&self) -> Decimal {
        self.balance
    }

    /// Get position for an event.
    pub fn position(&self, event_id: &str) -> Option<&PaperPosition> {
        self.positions.get(event_id)
    }

    /// Get total position value (approximate).
    pub fn total_position_value(&self) -> Decimal {
        // Approximate using 0.5 per share (fair value for binary)
        self.positions
            .values()
            .map(|p| (p.yes_shares + p.no_shares) * Decimal::new(5, 1))
            .sum()
    }

    /// Generate a unique order ID.
    fn generate_order_id(&mut self) -> String {
        let id = format!("paper-{}", self.next_order_id);
        self.next_order_id += 1;
        id
    }

    /// Simulate order fill based on current market conditions.
    async fn simulate_fill(&mut self, order: &OrderRequest) -> Result<OrderResult, ExecutorError> {
        // Check balance for buy orders
        if order.side == Side::Buy && self.config.enforce_balance {
            let required = order.max_cost();
            if required > self.balance {
                return Ok(OrderResult::Rejected(OrderRejection {
                    request_id: order.request_id.clone(),
                    reason: format!(
                        "Insufficient funds: available={}, required={}",
                        self.balance, required
                    ),
                    timestamp: Utc::now(),
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
                return Ok(OrderResult::Rejected(OrderRejection {
                    request_id: order.request_id.clone(),
                    reason: format!(
                        "Position limit exceeded: current={}, max={}",
                        current_size, self.config.max_position_per_market
                    ),
                    timestamp: Utc::now(),
                }));
            }
        }

        // Simulate latency
        if self.config.fill_latency_ms > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(self.config.fill_latency_ms))
                .await;
        }

        // Determine fill price
        let fill_price = match order.order_type {
            OrderType::Market => {
                // Use provided price with slippage, or market price
                let base_price = {
                    let prices = self.market_prices.read().await;
                    prices
                        .get(&order.token_id)
                        .copied()
                        .unwrap_or_else(|| order.price.unwrap_or(Decimal::new(5, 1)))
                };

                // Apply slippage
                match order.side {
                    Side::Buy => base_price * (Decimal::ONE + self.config.market_order_slippage),
                    Side::Sell => base_price * (Decimal::ONE - self.config.market_order_slippage),
                }
            }
            OrderType::Limit | OrderType::Gtc | OrderType::Ioc => {
                // Assume fill at limit price for simplicity
                order.price.unwrap_or(Decimal::new(5, 1))
            }
        };

        // Calculate fill cost and fee
        let fill_cost = fill_price * order.size;
        let fee = fill_cost * self.config.fee_rate;

        // Generate order ID
        let order_id = self.generate_order_id();

        // Update balance and position
        match order.side {
            Side::Buy => {
                self.balance -= fill_cost + fee;
                let position = self.positions.entry(order.event_id.clone()).or_default();
                match order.outcome {
                    poly_common::types::Outcome::Yes => {
                        position.yes_shares += order.size;
                    }
                    poly_common::types::Outcome::No => {
                        position.no_shares += order.size;
                    }
                }
                position.cost_basis += fill_cost + fee;
            }
            Side::Sell => {
                self.balance += fill_cost - fee;
                let position = self.positions.entry(order.event_id.clone()).or_default();
                match order.outcome {
                    poly_common::types::Outcome::Yes => {
                        position.yes_shares -= order.size.min(position.yes_shares);
                    }
                    poly_common::types::Outcome::No => {
                        position.no_shares -= order.size.min(position.no_shares);
                    }
                }
            }
        }

        let fill = OrderFill {
            request_id: order.request_id.clone(),
            order_id: order_id.clone(),
            size: order.size,
            price: fill_price,
            fee,
            timestamp: Utc::now(),
        };

        info!(
            order_id = %order_id,
            side = %order.side,
            outcome = ?order.outcome,
            size = %order.size,
            price = %fill_price,
            fee = %fee,
            "Paper order filled"
        );

        let result = OrderResult::Filled(fill);

        // Store order for history
        self.orders.insert(
            order_id,
            SimulatedOrder {
                request: order.clone(),
                result: result.clone(),
                created_at: Utc::now(),
            },
        );

        Ok(result)
    }
}

impl PaperExecutor {
    /// Settle a market and calculate realized PnL.
    ///
    /// For binary options:
    /// - YES wins: YES shares pay $1.00 each, NO shares pay $0
    /// - NO wins: NO shares pay $1.00 each, YES shares pay $0
    ///
    /// Returns realized PnL (payout - cost_basis).
    pub fn settle_market_position(&mut self, event_id: &str, yes_wins: bool) -> Decimal {
        let position = match self.positions.remove(event_id) {
            Some(p) => p,
            None => return Decimal::ZERO, // No position to settle
        };

        // Calculate payout
        let payout = if yes_wins {
            position.yes_shares // YES shares pay $1.00 each
        } else {
            position.no_shares // NO shares pay $1.00 each
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

    /// Get total realized PnL from all settled positions.
    pub fn total_realized_pnl(&self) -> Decimal {
        self.balance - self.config.initial_balance + self.total_position_value()
    }
}

#[async_trait]
impl Executor for PaperExecutor {
    async fn place_order(&mut self, order: OrderRequest) -> Result<OrderResult, ExecutorError> {
        debug!(
            request_id = %order.request_id,
            event_id = %order.event_id,
            side = %order.side,
            outcome = ?order.outcome,
            size = %order.size,
            price = ?order.price,
            "Placing paper order"
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
        // Check if order exists and is pending
        if let Some(pending) = self.pending.remove(order_id) {
            let cancellation = OrderCancellation {
                request_id: pending.request_id,
                order_id: order_id.to_string(),
                filled_size: Decimal::ZERO,
                unfilled_size: Decimal::ZERO, // Would need original order to know
                timestamp: Utc::now(),
            };

            info!(order_id = %order_id, "Paper order cancelled");
            Ok(cancellation)
        } else {
            // Check if it's a completed order
            if self.orders.contains_key(order_id) {
                warn!(order_id = %order_id, "Cannot cancel: order already completed");
                Err(ExecutorError::InvalidOrder(
                    "Order already completed".to_string(),
                ))
            } else {
                warn!(order_id = %order_id, "Cannot cancel: order not found");
                Err(ExecutorError::InvalidOrder("Order not found".to_string()))
            }
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
        self.settle_market_position(event_id, yes_wins)
    }

    async fn shutdown(&mut self) {
        info!(
            balance = %self.balance,
            orders_executed = self.orders.len(),
            "Paper executor shutting down"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poly_common::types::Outcome;
    use rust_decimal_macros::dec;

    #[tokio::test]
    async fn test_paper_executor_basic_fill() {
        let config = PaperExecutorConfig {
            initial_balance: dec!(1000),
            fill_latency_ms: 0, // No latency for tests
            fee_rate: dec!(0.001),
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO,
            market_order_slippage: Decimal::ZERO,
        };

        let mut executor = PaperExecutor::new(config);
        assert_eq!(executor.balance(), dec!(1000));

        // Place a buy order
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
        assert_eq!(position.no_shares, Decimal::ZERO);
    }

    #[tokio::test]
    async fn test_paper_executor_insufficient_funds() {
        let config = PaperExecutorConfig {
            initial_balance: dec!(10), // Low balance
            fill_latency_ms: 0,
            fee_rate: dec!(0.001),
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO,
            market_order_slippage: Decimal::ZERO,
        };

        let mut executor = PaperExecutor::new(config);

        // Try to buy more than balance allows
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
    async fn test_paper_executor_sell_order() {
        let config = PaperExecutorConfig {
            initial_balance: dec!(1000),
            fill_latency_ms: 0,
            fee_rate: dec!(0.001),
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO,
            market_order_slippage: Decimal::ZERO,
        };

        let mut executor = PaperExecutor::new(config);

        // First buy some shares
        let buy_order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.45),
        );
        executor.place_order(buy_order).await.unwrap();

        let balance_after_buy = executor.balance();

        // Now sell some shares
        let sell_order = OrderRequest::limit(
            "req-2".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Sell,
            dec!(50),
            dec!(0.50), // Sell higher
        );

        let result = executor.place_order(sell_order).await.unwrap();
        assert!(result.is_filled());

        // Balance should increase: 50 * 0.50 - fee = 25 - 0.025 = 24.975
        let expected_gain = dec!(25) - dec!(0.025);
        assert_eq!(executor.balance(), balance_after_buy + expected_gain);

        // Position should decrease
        let position = executor.position("event-1").unwrap();
        assert_eq!(position.yes_shares, dec!(50));
    }

    #[tokio::test]
    async fn test_paper_executor_position_limit() {
        let config = PaperExecutorConfig {
            initial_balance: dec!(10000),
            fill_latency_ms: 0,
            fee_rate: dec!(0.001),
            enforce_balance: true,
            max_position_per_market: dec!(100), // Max 100 shares per market
            market_order_slippage: Decimal::ZERO,
        };

        let mut executor = PaperExecutor::new(config);

        // Buy up to the limit
        let order1 = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.45),
        );
        executor.place_order(order1).await.unwrap();

        // Try to exceed the limit
        let order2 = OrderRequest::limit(
            "req-2".to_string(),
            "event-1".to_string(),
            "token-no".to_string(),
            Outcome::No,
            Side::Buy,
            dec!(50), // Would bring total to 150
            dec!(0.50),
        );

        let result = executor.place_order(order2).await.unwrap();
        assert!(result.is_rejected());
    }

    #[tokio::test]
    async fn test_paper_executor_market_order_slippage() {
        let config = PaperExecutorConfig {
            initial_balance: dec!(1000),
            fill_latency_ms: 0,
            fee_rate: Decimal::ZERO,
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO,
            market_order_slippage: dec!(0.01), // 1% slippage
        };

        let mut executor = PaperExecutor::new(config);

        // Set market price
        executor.update_price("token-yes", dec!(0.50)).await;

        // Place market buy order
        let order = OrderRequest::market(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
        );

        let result = executor.place_order(order).await.unwrap();
        assert!(result.is_filled());

        // Fill price should be 0.50 * 1.01 = 0.505
        if let OrderResult::Filled(fill) = result {
            assert_eq!(fill.price, dec!(0.505));
        }
    }

    #[tokio::test]
    async fn test_paper_executor_invalid_order() {
        let mut executor = PaperExecutor::with_defaults();

        // Zero size
        let order = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            Decimal::ZERO,
            dec!(0.45),
        );

        let result = executor.place_order(order).await;
        assert!(matches!(result, Err(ExecutorError::InvalidOrder(_))));
    }

    #[tokio::test]
    async fn test_paper_executor_cancel_order() {
        let mut executor = PaperExecutor::with_defaults();

        // Try to cancel non-existent order
        let result = executor.cancel_order("non-existent").await;
        assert!(matches!(result, Err(ExecutorError::InvalidOrder(_))));
    }

    #[tokio::test]
    async fn test_paper_executor_order_history() {
        let config = PaperExecutorConfig {
            initial_balance: dec!(1000),
            fill_latency_ms: 0,
            fee_rate: Decimal::ZERO,
            ..Default::default()
        };

        let mut executor = PaperExecutor::new(config);

        // Place an order
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
        let order_id = result.order_id().unwrap().to_string();

        // Check order status
        let status = executor.order_status(&order_id).await;
        assert!(status.is_some());
        assert!(status.unwrap().is_filled());
    }

    #[test]
    fn test_paper_executor_config_default() {
        let config = PaperExecutorConfig::default();
        assert_eq!(config.initial_balance, dec!(10000));
        assert_eq!(config.fill_latency_ms, 50);
        assert_eq!(config.fee_rate, dec!(0.001));
        assert!(config.enforce_balance);
    }

    #[tokio::test]
    async fn test_paper_executor_settlement_yes_wins() {
        let config = PaperExecutorConfig {
            initial_balance: dec!(1000),
            fill_latency_ms: 0,
            fee_rate: Decimal::ZERO, // No fees for simpler math
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO,
            market_order_slippage: Decimal::ZERO,
        };

        let mut executor = PaperExecutor::new(config);

        // Buy 100 YES shares at $0.40 = $40 cost
        let buy_yes = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.40),
        );
        executor.place_order(buy_yes).await.unwrap();

        // Buy 50 NO shares at $0.60 = $30 cost
        let buy_no = OrderRequest::limit(
            "req-2".to_string(),
            "event-1".to_string(),
            "token-no".to_string(),
            Outcome::No,
            Side::Buy,
            dec!(50),
            dec!(0.60),
        );
        executor.place_order(buy_no).await.unwrap();

        // Total cost = $40 + $30 = $70
        // Balance = $1000 - $70 = $930
        assert_eq!(executor.balance(), dec!(930));

        // Settle with YES winning
        // YES shares (100) pay $1 each = $100 payout
        // NO shares (50) pay $0 = $0 payout
        // Total payout = $100
        // Realized PnL = $100 - $70 = $30 profit
        let realized_pnl = executor.settle_market_position("event-1", true);
        assert_eq!(realized_pnl, dec!(30));

        // Balance = $930 + $100 payout = $1030
        assert_eq!(executor.balance(), dec!(1030));

        // Position should be cleared
        assert!(executor.position("event-1").is_none());
    }

    #[tokio::test]
    async fn test_paper_executor_settlement_no_wins() {
        let config = PaperExecutorConfig {
            initial_balance: dec!(1000),
            fill_latency_ms: 0,
            fee_rate: Decimal::ZERO,
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO,
            market_order_slippage: Decimal::ZERO,
        };

        let mut executor = PaperExecutor::new(config);

        // Buy 100 YES shares at $0.80 = $80 cost
        let buy_yes = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.80),
        );
        executor.place_order(buy_yes).await.unwrap();

        // Settle with NO winning
        // YES shares (100) pay $0 = $0 payout
        // Realized PnL = $0 - $80 = -$80 loss
        let realized_pnl = executor.settle_market_position("event-1", false);
        assert_eq!(realized_pnl, dec!(-80));

        // Balance = $920 + $0 payout = $920
        assert_eq!(executor.balance(), dec!(920));
    }
}
