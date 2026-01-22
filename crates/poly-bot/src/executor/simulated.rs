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
    /// Fee rate (e.g., 0.001 for 0.1%) - only used if use_realistic_fees is false.
    pub fee_rate: Decimal,
    /// Use realistic Polymarket fee/rebate calculations.
    pub use_realistic_fees: bool,
    /// Whether executing as taker (market orders) or maker (limit orders).
    pub is_taker_mode: bool,
    /// Estimated rebate rate for makers (as fraction of fee_equivalent, e.g., 0.5 for 50%).
    /// In reality this depends on total market volume; this is an approximation.
    pub maker_rebate_rate: Decimal,
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
            use_realistic_fees: false,
            is_taker_mode: false,
            maker_rebate_rate: Decimal::new(5, 1),   // 50% of fee_equivalent as rebate
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
            use_realistic_fees: true,               // Use realistic fees for backtest
            is_taker_mode: false,                   // Default to maker (limit orders)
            maker_rebate_rate: Decimal::new(5, 1),  // 50% of fee_equivalent as rebate
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

    /// Calculate realistic Polymarket fee for a taker order.
    /// Formula: fee_equivalent = shares * price * 0.25 * (price * (1 - price))^2
    pub fn calculate_taker_fee(shares: Decimal, price: Decimal) -> Decimal {
        let p_complement = Decimal::ONE - price;
        let variance_term = price * p_complement;
        let variance_squared = variance_term * variance_term;
        shares * price * Decimal::new(25, 2) * variance_squared
    }

    /// Calculate maker rebate for a limit order.
    /// In reality: rebate = (your_fee_equivalent / total_fee_equivalent) * rebate_pool
    /// We approximate with: rebate = fee_equivalent * rebate_rate
    pub fn calculate_maker_rebate(shares: Decimal, price: Decimal, rebate_rate: Decimal) -> Decimal {
        let fee_equivalent = Self::calculate_taker_fee(shares, price);
        fee_equivalent * rebate_rate
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
    /// Total fees paid (taker fees).
    pub fees_paid: Decimal,
    /// Total rebates earned (maker rebates).
    pub rebates_earned: Decimal,
    /// Number of settled markets with positive PnL.
    pub markets_won: u64,
    /// Number of settled markets with negative or zero PnL.
    pub markets_lost: u64,
    /// Peak balance observed (for drawdown calculation).
    pub peak_balance: Decimal,
    /// Maximum drawdown observed (peak to trough).
    pub max_drawdown: Decimal,
    /// Balance history for Sharpe ratio calculation (timestamp, balance).
    pub balance_history: Vec<(DateTime<Utc>, Decimal)>,
    /// Gross profit from winning markets (sum of positive PnLs).
    pub gross_profit: Decimal,
    /// Gross loss from losing markets (sum of negative PnLs, stored as positive).
    pub gross_loss: Decimal,
    /// Timestamp when current drawdown started (None if at peak).
    pub drawdown_start: Option<DateTime<Utc>>,
    /// Maximum drawdown duration in seconds.
    pub max_drawdown_duration_secs: i64,
}

impl SimulatedStats {
    /// Calculate win rate as a percentage.
    pub fn win_rate(&self) -> Option<Decimal> {
        let total = self.markets_won + self.markets_lost;
        if total == 0 {
            None
        } else {
            Some(Decimal::new(self.markets_won as i64, 0) / Decimal::new(total as i64, 0))
        }
    }

    /// Calculate Sharpe ratio from balance history.
    /// Uses daily returns, assuming 252 trading days per year.
    pub fn sharpe_ratio(&self) -> Option<f64> {
        if self.balance_history.len() < 2 {
            return None;
        }

        // Calculate returns between consecutive balance snapshots
        let returns: Vec<f64> = self
            .balance_history
            .windows(2)
            .filter_map(|w| {
                let prev = w[0].1;
                let curr = w[1].1;
                if prev > Decimal::ZERO {
                    use rust_decimal::prelude::ToPrimitive;
                    Some(((curr - prev) / prev).to_f64()?)
                } else {
                    None
                }
            })
            .collect();

        if returns.is_empty() {
            return None;
        }

        let n = returns.len() as f64;
        let mean: f64 = returns.iter().sum::<f64>() / n;
        let variance: f64 = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
        let std_dev: f64 = variance.sqrt();

        if std_dev <= 0.0 {
            return None;
        }

        // Annualize: for 15-minute windows, assume ~96 periods per day, ~252 days/year
        // Sharpe = (mean * sqrt(periods_per_year)) / std_dev
        let periods_per_year: f64 = 96.0 * 252.0;
        Some(mean * periods_per_year.sqrt() / std_dev)
    }

    /// Get max drawdown as a percentage.
    pub fn max_drawdown_pct(&self) -> Option<Decimal> {
        if self.peak_balance > Decimal::ZERO && self.max_drawdown > Decimal::ZERO {
            Some((self.max_drawdown / self.peak_balance) * Decimal::ONE_HUNDRED)
        } else {
            None
        }
    }

    /// Calculate profit factor (gross profit / gross loss).
    /// A value > 1 indicates profitability.
    pub fn profit_factor(&self) -> Option<f64> {
        if self.gross_loss <= Decimal::ZERO {
            // No losses - infinite profit factor, return None
            if self.gross_profit > Decimal::ZERO {
                return Some(f64::INFINITY);
            }
            return None;
        }

        use rust_decimal::prelude::ToPrimitive;
        let pf = self.gross_profit / self.gross_loss;
        pf.to_f64()
    }

    /// Calculate Sortino ratio (like Sharpe but only penalizes downside volatility).
    /// Uses daily returns, assuming 252 trading days per year.
    pub fn sortino_ratio(&self) -> Option<f64> {
        if self.balance_history.len() < 2 {
            return None;
        }

        // Calculate returns between consecutive balance snapshots
        let returns: Vec<f64> = self
            .balance_history
            .windows(2)
            .filter_map(|w| {
                let prev = w[0].1;
                let curr = w[1].1;
                if prev > Decimal::ZERO {
                    use rust_decimal::prelude::ToPrimitive;
                    Some(((curr - prev) / prev).to_f64()?)
                } else {
                    None
                }
            })
            .collect();

        if returns.is_empty() {
            return None;
        }

        let n = returns.len() as f64;
        let mean: f64 = returns.iter().sum::<f64>() / n;

        // Calculate downside deviation (only negative returns)
        let downside_returns: Vec<f64> = returns.iter().filter(|&&r| r < 0.0).copied().collect();

        if downside_returns.is_empty() {
            // No downside - excellent, but can't compute ratio
            return None;
        }

        let downside_variance: f64 =
            downside_returns.iter().map(|r| r.powi(2)).sum::<f64>() / n;
        let downside_dev: f64 = downside_variance.sqrt();

        if downside_dev <= 0.0 {
            return None;
        }

        // Annualize: for 15-minute windows, assume ~96 periods per day, ~252 days/year
        let periods_per_year: f64 = 96.0 * 252.0;
        Some(mean * periods_per_year.sqrt() / downside_dev)
    }

    /// Calculate Calmar ratio (annualized return / max drawdown).
    /// Higher is better - measures return per unit of drawdown risk.
    pub fn calmar_ratio(&self) -> Option<f64> {
        if self.balance_history.len() < 2 {
            return None;
        }

        let max_dd_pct = self.max_drawdown_pct()?;
        if max_dd_pct <= Decimal::ZERO {
            return None;
        }

        // Calculate total return
        let initial = self.balance_history.first()?.1;
        let final_bal = self.balance_history.last()?.1;

        if initial <= Decimal::ZERO {
            return None;
        }

        use rust_decimal::prelude::ToPrimitive;
        let total_return = ((final_bal - initial) / initial).to_f64()?;

        // Annualize the return (assuming the backtest period)
        // For now, use a simple approximation based on number of samples
        // With 15-min windows: samples / 96 = days, * 252 = annual factor
        let samples = self.balance_history.len() as f64;
        let days = samples / 96.0;
        let annual_factor = if days > 0.0 { 252.0 / days } else { 1.0 };
        let annualized_return = total_return * annual_factor;

        let max_dd_f64 = (max_dd_pct / Decimal::ONE_HUNDRED).to_f64()?;
        if max_dd_f64 <= 0.0 {
            return None;
        }

        Some(annualized_return / max_dd_f64)
    }

    /// Get the maximum drawdown duration in seconds.
    pub fn max_drawdown_duration(&self) -> Option<i64> {
        if self.max_drawdown_duration_secs > 0 {
            Some(self.max_drawdown_duration_secs)
        } else {
            None
        }
    }
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
    #[allow(clippy::field_reassign_with_default)]
    pub fn new(config: SimulatedExecutorConfig) -> Self {
        let initial_balance = config.initial_balance;
        let mut stats = SimulatedStats::default();
        stats.peak_balance = initial_balance;
        stats.balance_history.push((Utc::now(), initial_balance));

        Self {
            balance: initial_balance,
            config,
            positions: HashMap::new(),
            orders: HashMap::new(),
            pending: HashMap::new(),
            next_order_id: 1,
            simulated_time: Utc::now(),
            order_books: Arc::new(RwLock::new(HashMap::new())),
            market_prices: Arc::new(RwLock::new(HashMap::new())),
            stats,
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

        // Calculate fill cost and fee/rebate
        let fill_cost = fill_price * order.size;
        let (fee, rebate) = if self.config.use_realistic_fees {
            let is_taker = self.config.is_taker_mode
                || matches!(order.order_type, OrderType::Market | OrderType::Ioc);
            if is_taker {
                // Taker fee: fee_equivalent = shares * price * 0.25 * (price * (1 - price))^2
                let fee = SimulatedExecutorConfig::calculate_taker_fee(order.size, fill_price);
                (fee, Decimal::ZERO)
            } else {
                // Maker rebate
                let rebate = SimulatedExecutorConfig::calculate_maker_rebate(
                    order.size, fill_price, self.config.maker_rebate_rate
                );
                (Decimal::ZERO, rebate)
            }
        } else {
            (fill_cost * self.config.fee_rate, Decimal::ZERO)
        };

        // Generate order ID
        let order_id = self.generate_order_id();

        // Update balance and position (rebate reduces cost, fee increases it)
        self.apply_fill(order, order.size, fill_cost, fee - rebate);

        // Update stats
        self.stats.orders_filled += 1;
        self.stats.volume_traded += fill_cost;
        self.stats.fees_paid += fee;
        self.stats.rebates_earned += rebate;

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

        // Calculate fee/rebate using avg fill price
        let fill_price = avg_price.unwrap_or(Decimal::new(5, 1));
        let (fee, rebate) = if self.config.use_realistic_fees {
            let is_taker = self.config.is_taker_mode
                || matches!(order.order_type, OrderType::Market | OrderType::Ioc);
            if is_taker {
                let fee = SimulatedExecutorConfig::calculate_taker_fee(filled_size, fill_price);
                (fee, Decimal::ZERO)
            } else {
                let rebate = SimulatedExecutorConfig::calculate_maker_rebate(
                    filled_size, fill_price, self.config.maker_rebate_rate
                );
                (Decimal::ZERO, rebate)
            }
        } else {
            (total_cost * self.config.fee_rate, Decimal::ZERO)
        };

        // Update balance and position (rebate reduces cost, fee increases it)
        self.apply_fill(order, filled_size, total_cost, fee - rebate);

        // Update stats
        self.stats.volume_traded += total_cost;
        self.stats.fees_paid += fee;
        self.stats.rebates_earned += rebate;

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

        // Track win/loss statistics and profit/loss amounts
        if realized_pnl > Decimal::ZERO {
            self.stats.markets_won += 1;
            self.stats.gross_profit += realized_pnl;
        } else {
            self.stats.markets_lost += 1;
            self.stats.gross_loss += realized_pnl.abs();
        }

        // Track balance history for Sharpe calculation
        let current_time = self.current_time();
        self.stats.balance_history.push((current_time, self.balance));

        // Update peak balance, max drawdown, and drawdown duration
        if self.balance >= self.stats.peak_balance {
            // At or above peak - record drawdown duration if we were in a drawdown
            if let Some(dd_start) = self.stats.drawdown_start.take() {
                let duration_secs = (current_time - dd_start).num_seconds();
                if duration_secs > self.stats.max_drawdown_duration_secs {
                    self.stats.max_drawdown_duration_secs = duration_secs;
                }
            }
            self.stats.peak_balance = self.balance;
        } else {
            // In drawdown - start tracking if not already
            if self.stats.drawdown_start.is_none() {
                self.stats.drawdown_start = Some(current_time);
            }
            let drawdown = self.stats.peak_balance - self.balance;
            if drawdown > self.stats.max_drawdown {
                self.stats.max_drawdown = drawdown;
            }
        }

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

    fn simulation_stats(&self) -> Option<SimulatedStats> {
        Some(self.stats.clone())
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
        config.use_realistic_fees = false; // Use simple fee_rate for stats tracking tests

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
