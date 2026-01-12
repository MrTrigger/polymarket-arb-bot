//! Price chasing logic for aggressive order filling.
//!
//! When a limit order doesn't fill immediately, the price chaser bumps the
//! price incrementally to "chase" fills up to a calculated ceiling price.
//!
//! ## Ceiling Calculation
//!
//! For arbitrage trades, the ceiling is the maximum price we can pay while
//! still maintaining our minimum margin:
//!
//! ```text
//! ceiling = 1.0 - other_leg_price - min_margin
//! ```
//!
//! For example, if we're buying YES at 0.45 and NO is at 0.50:
//! - With 0.5% min margin: ceiling = 1.0 - 0.50 - 0.005 = 0.495
//! - We can chase up to 0.495 and still capture arb profit
//!
//! ## Chase Loop
//!
//! 1. Place initial limit order at starting price
//! 2. Wait for check_interval
//! 3. If filled, return success
//! 4. If not filled, bump price by step_size
//! 5. If new price > ceiling, stop chasing
//! 6. Cancel old order, place new order at bumped price
//! 7. Repeat until filled, ceiling hit, or timeout
//!
//! ## Partial Fills
//!
//! When an order is partially filled:
//! - Record the partial fill
//! - Continue chasing for remaining size
//! - Accumulate fills until complete or timeout

use std::time::{Duration, Instant};

use chrono::Utc;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use poly_common::types::Side;

use super::{Executor, ExecutorError, OrderRequest, OrderResult, OrderType};

/// Configuration for price chasing behavior.
#[derive(Debug, Clone)]
pub struct ChaseConfig {
    /// Enable price chasing.
    pub enabled: bool,

    /// Price increment per chase iteration.
    pub step_size: Decimal,

    /// Interval between fill checks (milliseconds).
    pub check_interval_ms: u64,

    /// Maximum time to chase (milliseconds).
    pub max_chase_time_ms: u64,

    /// Minimum remaining size to continue chasing.
    pub min_chase_size: Decimal,

    /// Minimum margin to maintain (affects ceiling calculation).
    pub min_margin: Decimal,
}

impl Default for ChaseConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            step_size: Decimal::new(1, 3),        // 0.001
            check_interval_ms: 100,               // 100ms
            max_chase_time_ms: 5000,              // 5 seconds
            min_chase_size: Decimal::new(1, 0),   // 1 share minimum
            min_margin: Decimal::new(5, 3),       // 0.5% minimum margin
        }
    }
}

impl ChaseConfig {
    /// Create a config from execution config values.
    pub fn from_execution_config(
        chase_enabled: bool,
        chase_step_size: Decimal,
        chase_check_interval_ms: u64,
        max_chase_time_ms: u64,
        min_margin: Decimal,
    ) -> Self {
        Self {
            enabled: chase_enabled,
            step_size: chase_step_size,
            check_interval_ms: chase_check_interval_ms,
            max_chase_time_ms,
            min_chase_size: Decimal::new(1, 0),
            min_margin,
        }
    }
}

/// Result of a price chase operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChaseResult {
    /// Whether the chase was successful (fully filled).
    pub success: bool,

    /// Total size filled across all iterations.
    pub filled_size: Decimal,

    /// Weighted average fill price.
    pub avg_price: Decimal,

    /// Total cost (filled_size * avg_price).
    pub total_cost: Decimal,

    /// Total fees paid.
    pub total_fee: Decimal,

    /// Number of chase iterations.
    pub iterations: u32,

    /// Final price reached during chase.
    pub final_price: Decimal,

    /// Reason for stopping (if not fully filled).
    pub stop_reason: Option<ChaseStopReason>,

    /// Time spent chasing (milliseconds).
    pub elapsed_ms: u64,

    /// Individual fills accumulated during chase.
    pub fills: Vec<ChaseFill>,
}

impl ChaseResult {
    /// Create an empty result for when chasing is disabled.
    pub fn empty() -> Self {
        Self {
            success: false,
            filled_size: Decimal::ZERO,
            avg_price: Decimal::ZERO,
            total_cost: Decimal::ZERO,
            total_fee: Decimal::ZERO,
            iterations: 0,
            final_price: Decimal::ZERO,
            stop_reason: Some(ChaseStopReason::Disabled),
            elapsed_ms: 0,
            fills: Vec::new(),
        }
    }

    /// Create a result from a single immediate fill.
    pub fn from_immediate_fill(
        size: Decimal,
        price: Decimal,
        fee: Decimal,
    ) -> Self {
        Self {
            success: true,
            filled_size: size,
            avg_price: price,
            total_cost: size * price,
            total_fee: fee,
            iterations: 1,
            final_price: price,
            stop_reason: None,
            elapsed_ms: 0,
            fills: vec![ChaseFill {
                size,
                price,
                fee,
                iteration: 1,
            }],
        }
    }

    /// Check if the result is a complete fill.
    pub fn is_complete(&self) -> bool {
        self.success && self.stop_reason.is_none()
    }

    /// Calculate profit if this was an arb trade at given other leg price.
    pub fn arb_profit(&self, other_leg_price: Decimal) -> Decimal {
        // Revenue from arb = shares * (1.0 - combined_cost)
        // Combined cost = our avg price + other leg price
        let combined = self.avg_price + other_leg_price;
        self.filled_size * (Decimal::ONE - combined) - self.total_fee
    }
}

/// Reason why chasing stopped before full fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChaseStopReason {
    /// Chasing is disabled in config.
    Disabled,

    /// Reached maximum chase time.
    Timeout,

    /// Reached price ceiling (would violate margin).
    CeilingReached,

    /// Order was rejected by executor.
    OrderRejected,

    /// Remaining size below minimum.
    SizeTooSmall,

    /// Executor error during chase.
    ExecutorError,

    /// Market closed or window expired.
    MarketClosed,

    /// Manual cancellation.
    Cancelled,
}

/// Individual fill during chase operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChaseFill {
    /// Size filled in this iteration.
    pub size: Decimal,

    /// Price filled at.
    pub price: Decimal,

    /// Fee for this fill.
    pub fee: Decimal,

    /// Which iteration this fill occurred.
    pub iteration: u32,
}

/// Price chaser for aggressive order filling.
///
/// Wraps an executor and provides price chasing functionality.
/// When an order doesn't fill immediately, bumps the price until
/// filled, ceiling reached, or timeout.
pub struct PriceChaser {
    config: ChaseConfig,
}

impl PriceChaser {
    /// Create a new price chaser with given configuration.
    pub fn new(config: ChaseConfig) -> Self {
        Self { config }
    }

    /// Create a price chaser with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(ChaseConfig::default())
    }

    /// Check if chasing is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Calculate the ceiling price for chasing.
    ///
    /// For buy orders: ceiling = 1.0 - other_leg_price - min_margin
    /// For sell orders: floor = other_leg_price + min_margin (we don't go below)
    pub fn calculate_ceiling(
        &self,
        side: Side,
        other_leg_price: Decimal,
    ) -> Decimal {
        match side {
            Side::Buy => {
                // Max we can pay = 1.0 - other_leg - min_margin
                let ceiling = Decimal::ONE - other_leg_price - self.config.min_margin;
                // Ensure ceiling is positive
                if ceiling > Decimal::ZERO {
                    ceiling
                } else {
                    Decimal::ZERO
                }
            }
            Side::Sell => {
                // Min we can sell at = other_leg_price + min_margin
                // (to ensure combined > 1.0 - min_margin)
                // For arb, we're typically buying, not selling, so this is less common
                other_leg_price + self.config.min_margin
            }
        }
    }

    /// Chase a single order until filled or stopped.
    ///
    /// This method modifies prices and resubmits orders to the executor
    /// until the order is filled, ceiling is reached, or timeout occurs.
    pub async fn chase_order<E: Executor>(
        &self,
        executor: &mut E,
        request: OrderRequest,
        other_leg_price: Decimal,
    ) -> Result<ChaseResult, ExecutorError> {
        // If chasing disabled, just place the order once
        if !self.config.enabled {
            return self.place_without_chase(executor, request).await;
        }

        let start = Instant::now();
        let ceiling = self.calculate_ceiling(request.side, other_leg_price);
        let start_price = request.price.unwrap_or(Decimal::new(5, 1)); // 0.50 default

        // Validate starting price
        if request.side == Side::Buy && start_price > ceiling {
            warn!(
                start_price = %start_price,
                ceiling = %ceiling,
                "Starting price already exceeds ceiling, placing at ceiling"
            );
        }

        let mut current_price = start_price.min(ceiling);
        let mut remaining_size = request.size;
        let mut fills: Vec<ChaseFill> = Vec::new();
        let mut iteration = 0u32;
        let mut current_order_id: Option<String> = None;

        let timeout = Duration::from_millis(self.config.max_chase_time_ms);
        let check_interval = Duration::from_millis(self.config.check_interval_ms);

        debug!(
            event_id = %request.event_id,
            token_id = %request.token_id,
            start_price = %start_price,
            ceiling = %ceiling,
            size = %request.size,
            "Starting price chase"
        );

        loop {
            iteration += 1;
            let elapsed = start.elapsed();

            // Check timeout
            if elapsed >= timeout {
                info!(
                    iterations = iteration,
                    elapsed_ms = elapsed.as_millis(),
                    filled = %total_filled(&fills),
                    "Chase timeout reached"
                );
                return Ok(build_result(
                    fills,
                    remaining_size,
                    current_price,
                    elapsed.as_millis() as u64,
                    iteration,
                    Some(ChaseStopReason::Timeout),
                ));
            }

            // Check if remaining size is too small
            if remaining_size < self.config.min_chase_size {
                debug!(
                    remaining = %remaining_size,
                    min = %self.config.min_chase_size,
                    "Remaining size below minimum, stopping"
                );
                return Ok(build_result(
                    fills,
                    remaining_size,
                    current_price,
                    elapsed.as_millis() as u64,
                    iteration,
                    Some(ChaseStopReason::SizeTooSmall),
                ));
            }

            // Cancel previous order if exists
            if let Some(ref order_id) = current_order_id {
                match executor.cancel_order(order_id).await {
                    Ok(cancellation) => {
                        // If there was a partial fill before cancellation, record it
                        if cancellation.filled_size > Decimal::ZERO {
                            let fill = ChaseFill {
                                size: cancellation.filled_size,
                                price: current_price,
                                fee: cancellation.filled_size * current_price * Decimal::new(1, 3), // Estimate fee
                                iteration,
                            };
                            remaining_size -= cancellation.filled_size;
                            fills.push(fill);
                        }
                    }
                    Err(e) => {
                        // Order might already be filled or expired, continue
                        debug!(order_id = %order_id, error = %e, "Failed to cancel order");
                    }
                }
            }

            // Place new order at current price
            let order = OrderRequest {
                request_id: format!("{}-chase-{}", request.request_id, iteration),
                event_id: request.event_id.clone(),
                token_id: request.token_id.clone(),
                outcome: request.outcome,
                side: request.side,
                size: remaining_size,
                price: Some(current_price),
                order_type: OrderType::Limit,
                timeout_ms: Some(self.config.check_interval_ms),
                timestamp: Utc::now(),
            };

            match executor.place_order(order).await {
                Ok(OrderResult::Filled(fill)) => {
                    // Full fill!
                    fills.push(ChaseFill {
                        size: fill.size,
                        price: fill.price,
                        fee: fill.fee,
                        iteration,
                    });

                    info!(
                        iterations = iteration,
                        final_price = %fill.price,
                        "Chase completed with full fill"
                    );

                    return Ok(build_result(
                        fills,
                        Decimal::ZERO,
                        fill.price,
                        start.elapsed().as_millis() as u64,
                        iteration,
                        None,
                    ));
                }

                Ok(OrderResult::PartialFill(partial)) => {
                    // Partial fill - record and continue
                    fills.push(ChaseFill {
                        size: partial.filled_size,
                        price: partial.avg_price,
                        fee: partial.fee,
                        iteration,
                    });
                    remaining_size -= partial.filled_size;
                    current_order_id = Some(partial.order_id);

                    debug!(
                        filled = %partial.filled_size,
                        remaining = %remaining_size,
                        price = %partial.avg_price,
                        "Partial fill during chase"
                    );
                }

                Ok(OrderResult::Pending(pending)) => {
                    // Order is pending, wait and check
                    current_order_id = Some(pending.order_id);
                }

                Ok(OrderResult::Rejected(rejection)) => {
                    warn!(
                        reason = %rejection.reason,
                        iteration = iteration,
                        "Order rejected during chase"
                    );
                    return Ok(build_result(
                        fills,
                        remaining_size,
                        current_price,
                        start.elapsed().as_millis() as u64,
                        iteration,
                        Some(ChaseStopReason::OrderRejected),
                    ));
                }

                Ok(OrderResult::Cancelled(_)) => {
                    // Unexpected cancellation
                    debug!("Order cancelled unexpectedly during chase");
                }

                Err(ExecutorError::MarketClosed) => {
                    return Ok(build_result(
                        fills,
                        remaining_size,
                        current_price,
                        start.elapsed().as_millis() as u64,
                        iteration,
                        Some(ChaseStopReason::MarketClosed),
                    ));
                }

                Err(e) => {
                    warn!(error = %e, "Executor error during chase");
                    return Ok(build_result(
                        fills,
                        remaining_size,
                        current_price,
                        start.elapsed().as_millis() as u64,
                        iteration,
                        Some(ChaseStopReason::ExecutorError),
                    ));
                }
            }

            // Wait before next iteration
            tokio::time::sleep(check_interval).await;

            // Check order status before bumping price
            if let Some(ref order_id) = current_order_id
                && let Some(status) = executor.order_status(order_id).await
            {
                match status {
                    OrderResult::Filled(fill) => {
                        fills.push(ChaseFill {
                            size: fill.size,
                            price: fill.price,
                            fee: fill.fee,
                            iteration,
                        });

                        return Ok(build_result(
                            fills,
                            Decimal::ZERO,
                            fill.price,
                            start.elapsed().as_millis() as u64,
                            iteration,
                            None,
                        ));
                    }
                    OrderResult::PartialFill(partial) => {
                        // Already recorded above or new partial
                        if partial.filled_size > total_filled(&fills) {
                            let new_fill = partial.filled_size - total_filled(&fills);
                            fills.push(ChaseFill {
                                size: new_fill,
                                price: partial.avg_price,
                                fee: partial.fee,
                                iteration,
                            });
                            remaining_size -= new_fill;
                        }
                    }
                    _ => {}
                }
            }

            // Bump price if not at ceiling
            let next_price = match request.side {
                Side::Buy => {
                    let bumped = current_price + self.config.step_size;
                    if bumped > ceiling {
                        // At ceiling, one more try at ceiling
                        if current_price >= ceiling {
                            info!(
                                ceiling = %ceiling,
                                iterations = iteration,
                                "Ceiling reached, stopping chase"
                            );
                            return Ok(build_result(
                                fills,
                                remaining_size,
                                current_price,
                                start.elapsed().as_millis() as u64,
                                iteration,
                                Some(ChaseStopReason::CeilingReached),
                            ));
                        }
                        ceiling
                    } else {
                        bumped
                    }
                }
                Side::Sell => {
                    let bumped = current_price - self.config.step_size;
                    let floor = self.calculate_ceiling(Side::Sell, other_leg_price);
                    if bumped < floor {
                        if current_price <= floor {
                            return Ok(build_result(
                                fills,
                                remaining_size,
                                current_price,
                                start.elapsed().as_millis() as u64,
                                iteration,
                                Some(ChaseStopReason::CeilingReached),
                            ));
                        }
                        floor
                    } else {
                        bumped
                    }
                }
            };

            debug!(
                iteration = iteration,
                current_price = %current_price,
                next_price = %next_price,
                remaining = %remaining_size,
                "Bumping price"
            );

            current_price = next_price;
        }
    }

    /// Place order without chasing (single attempt).
    async fn place_without_chase<E: Executor>(
        &self,
        executor: &mut E,
        request: OrderRequest,
    ) -> Result<ChaseResult, ExecutorError> {
        let start = Instant::now();
        let price = request.price.unwrap_or(Decimal::new(5, 1));

        match executor.place_order(request).await? {
            OrderResult::Filled(fill) => Ok(ChaseResult::from_immediate_fill(
                fill.size,
                fill.price,
                fill.fee,
            )),
            OrderResult::PartialFill(partial) => Ok(ChaseResult {
                success: false,
                filled_size: partial.filled_size,
                avg_price: partial.avg_price,
                total_cost: partial.filled_size * partial.avg_price,
                total_fee: partial.fee,
                iterations: 1,
                final_price: partial.avg_price,
                stop_reason: Some(ChaseStopReason::Disabled),
                elapsed_ms: start.elapsed().as_millis() as u64,
                fills: vec![ChaseFill {
                    size: partial.filled_size,
                    price: partial.avg_price,
                    fee: partial.fee,
                    iteration: 1,
                }],
            }),
            OrderResult::Rejected(_rejection) => Ok(ChaseResult {
                success: false,
                filled_size: Decimal::ZERO,
                avg_price: price,
                total_cost: Decimal::ZERO,
                total_fee: Decimal::ZERO,
                iterations: 1,
                final_price: price,
                stop_reason: Some(ChaseStopReason::OrderRejected),
                elapsed_ms: start.elapsed().as_millis() as u64,
                fills: Vec::new(),
            }),
            OrderResult::Pending(_) | OrderResult::Cancelled(_) => Ok(ChaseResult {
                success: false,
                filled_size: Decimal::ZERO,
                avg_price: price,
                total_cost: Decimal::ZERO,
                total_fee: Decimal::ZERO,
                iterations: 1,
                final_price: price,
                stop_reason: Some(ChaseStopReason::Disabled),
                elapsed_ms: start.elapsed().as_millis() as u64,
                fills: Vec::new(),
            }),
        }
    }
}

/// Calculate total filled size from fills vector.
fn total_filled(fills: &[ChaseFill]) -> Decimal {
    fills.iter().map(|f| f.size).sum()
}

/// Calculate weighted average price from fills.
fn weighted_avg_price(fills: &[ChaseFill]) -> Decimal {
    let total_size = total_filled(fills);
    if total_size == Decimal::ZERO {
        return Decimal::ZERO;
    }

    let total_value: Decimal = fills.iter().map(|f| f.size * f.price).sum();
    total_value / total_size
}

/// Calculate total fees from fills.
fn total_fees(fills: &[ChaseFill]) -> Decimal {
    fills.iter().map(|f| f.fee).sum()
}

/// Build a ChaseResult from accumulated fills.
fn build_result(
    fills: Vec<ChaseFill>,
    remaining_size: Decimal,
    final_price: Decimal,
    elapsed_ms: u64,
    iterations: u32,
    stop_reason: Option<ChaseStopReason>,
) -> ChaseResult {
    let filled_size = total_filled(&fills);
    let avg_price = weighted_avg_price(&fills);
    let total_cost = fills.iter().map(|f| f.size * f.price).sum();
    let total_fee = total_fees(&fills);

    ChaseResult {
        success: remaining_size == Decimal::ZERO && stop_reason.is_none(),
        filled_size,
        avg_price,
        total_cost,
        total_fee,
        iterations,
        final_price,
        stop_reason,
        elapsed_ms,
        fills,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::paper::{PaperExecutor, PaperExecutorConfig};
    use poly_common::types::Outcome;
    use rust_decimal_macros::dec;

    fn test_config() -> ChaseConfig {
        ChaseConfig {
            enabled: true,
            step_size: dec!(0.01),
            check_interval_ms: 10, // Fast for tests
            max_chase_time_ms: 1000,
            min_chase_size: dec!(1),
            min_margin: dec!(0.005),
        }
    }

    #[test]
    fn test_chase_config_default() {
        let config = ChaseConfig::default();
        assert!(config.enabled);
        assert_eq!(config.step_size, dec!(0.001));
        assert_eq!(config.check_interval_ms, 100);
        assert_eq!(config.max_chase_time_ms, 5000);
    }

    #[test]
    fn test_calculate_ceiling_buy() {
        let chaser = PriceChaser::new(ChaseConfig {
            min_margin: dec!(0.005),
            ..Default::default()
        });

        // Other leg at 0.50, ceiling = 1.0 - 0.50 - 0.005 = 0.495
        let ceiling = chaser.calculate_ceiling(Side::Buy, dec!(0.50));
        assert_eq!(ceiling, dec!(0.495));

        // Other leg at 0.40, ceiling = 1.0 - 0.40 - 0.005 = 0.595
        let ceiling = chaser.calculate_ceiling(Side::Buy, dec!(0.40));
        assert_eq!(ceiling, dec!(0.595));

        // Other leg at 0.996, ceiling would be negative (1.0 - 0.996 - 0.005 = -0.001), returns 0
        let ceiling = chaser.calculate_ceiling(Side::Buy, dec!(0.996));
        assert_eq!(ceiling, Decimal::ZERO);
    }

    #[test]
    fn test_calculate_ceiling_sell() {
        let chaser = PriceChaser::new(ChaseConfig {
            min_margin: dec!(0.005),
            ..Default::default()
        });

        // For sell, floor = other_leg + min_margin
        let floor = chaser.calculate_ceiling(Side::Sell, dec!(0.50));
        assert_eq!(floor, dec!(0.505));
    }

    #[test]
    fn test_chase_result_empty() {
        let result = ChaseResult::empty();
        assert!(!result.success);
        assert_eq!(result.filled_size, Decimal::ZERO);
        assert_eq!(result.stop_reason, Some(ChaseStopReason::Disabled));
    }

    #[test]
    fn test_chase_result_immediate_fill() {
        let result = ChaseResult::from_immediate_fill(
            dec!(100),
            dec!(0.45),
            dec!(0.045),
        );
        assert!(result.success);
        assert_eq!(result.filled_size, dec!(100));
        assert_eq!(result.avg_price, dec!(0.45));
        assert_eq!(result.total_cost, dec!(45));
        assert_eq!(result.iterations, 1);
        assert!(result.is_complete());
    }

    #[test]
    fn test_chase_result_arb_profit() {
        let result = ChaseResult::from_immediate_fill(
            dec!(100),
            dec!(0.45),
            dec!(0.045),
        );

        // If other leg is at 0.50, combined = 0.95, profit = 100 * 0.05 - 0.045 = 4.955
        let profit = result.arb_profit(dec!(0.50));
        assert_eq!(profit, dec!(4.955));
    }

    #[test]
    fn test_chase_fill_accumulation() {
        let fills = vec![
            ChaseFill {
                size: dec!(50),
                price: dec!(0.45),
                fee: dec!(0.0225),
                iteration: 1,
            },
            ChaseFill {
                size: dec!(30),
                price: dec!(0.46),
                fee: dec!(0.0138),
                iteration: 2,
            },
            ChaseFill {
                size: dec!(20),
                price: dec!(0.47),
                fee: dec!(0.0094),
                iteration: 3,
            },
        ];

        let total = total_filled(&fills);
        assert_eq!(total, dec!(100));

        let avg = weighted_avg_price(&fills);
        // (50*0.45 + 30*0.46 + 20*0.47) / 100 = (22.5 + 13.8 + 9.4) / 100 = 0.457
        assert_eq!(avg, dec!(0.457));

        let fees = total_fees(&fills);
        assert_eq!(fees, dec!(0.0457));
    }

    #[test]
    fn test_build_result_success() {
        let fills = vec![ChaseFill {
            size: dec!(100),
            price: dec!(0.45),
            fee: dec!(0.045),
            iteration: 1,
        }];

        let result = build_result(
            fills,
            Decimal::ZERO,
            dec!(0.45),
            100,
            1,
            None,
        );

        assert!(result.success);
        assert_eq!(result.filled_size, dec!(100));
        assert_eq!(result.stop_reason, None);
    }

    #[test]
    fn test_build_result_partial() {
        let fills = vec![ChaseFill {
            size: dec!(50),
            price: dec!(0.45),
            fee: dec!(0.0225),
            iteration: 1,
        }];

        let result = build_result(
            fills,
            dec!(50), // 50 remaining
            dec!(0.48),
            500,
            5,
            Some(ChaseStopReason::Timeout),
        );

        assert!(!result.success);
        assert_eq!(result.filled_size, dec!(50));
        assert_eq!(result.stop_reason, Some(ChaseStopReason::Timeout));
        assert_eq!(result.iterations, 5);
    }

    #[test]
    fn test_stop_reason_variants() {
        assert_eq!(
            format!("{:?}", ChaseStopReason::Timeout),
            "Timeout"
        );
        assert_eq!(
            format!("{:?}", ChaseStopReason::CeilingReached),
            "CeilingReached"
        );
        assert_eq!(
            format!("{:?}", ChaseStopReason::OrderRejected),
            "OrderRejected"
        );
    }

    #[tokio::test]
    async fn test_chase_order_immediate_fill() {
        let config = PaperExecutorConfig {
            initial_balance: dec!(1000),
            fill_latency_ms: 0,
            fee_rate: dec!(0.001),
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO,
            market_order_slippage: Decimal::ZERO,
        };
        let mut executor = PaperExecutor::new(config);

        let chase_config = test_config();
        let chaser = PriceChaser::new(chase_config);

        let request = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.45),
        );

        let result = chaser
            .chase_order(&mut executor, request, dec!(0.50))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.filled_size, dec!(100));
        assert_eq!(result.avg_price, dec!(0.45));
    }

    #[tokio::test]
    async fn test_chase_disabled() {
        let config = PaperExecutorConfig {
            initial_balance: dec!(1000),
            fill_latency_ms: 0,
            fee_rate: dec!(0.001),
            ..Default::default()
        };
        let mut executor = PaperExecutor::new(config);

        let chase_config = ChaseConfig {
            enabled: false,
            ..Default::default()
        };
        let chaser = PriceChaser::new(chase_config);

        assert!(!chaser.is_enabled());

        let request = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.45),
        );

        let result = chaser
            .chase_order(&mut executor, request, dec!(0.50))
            .await
            .unwrap();

        // Should still fill (paper executor fills immediately)
        assert!(result.success);
        assert_eq!(result.iterations, 1);
    }

    #[tokio::test]
    async fn test_chase_order_insufficient_funds() {
        let config = PaperExecutorConfig {
            initial_balance: dec!(10), // Low balance
            fill_latency_ms: 0,
            fee_rate: dec!(0.001),
            enforce_balance: true,
            max_position_per_market: Decimal::ZERO,
            market_order_slippage: Decimal::ZERO,
        };
        let mut executor = PaperExecutor::new(config);

        let chase_config = test_config();
        let chaser = PriceChaser::new(chase_config);

        let request = OrderRequest::limit(
            "req-1".to_string(),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(100),
            dec!(0.45), // Cost = 45, but only have 10
        );

        let result = chaser
            .chase_order(&mut executor, request, dec!(0.50))
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.stop_reason, Some(ChaseStopReason::OrderRejected));
    }

    #[test]
    fn test_from_execution_config() {
        let config = ChaseConfig::from_execution_config(
            true,
            dec!(0.002),
            150,
            10000,
            dec!(0.01),
        );

        assert!(config.enabled);
        assert_eq!(config.step_size, dec!(0.002));
        assert_eq!(config.check_interval_ms, 150);
        assert_eq!(config.max_chase_time_ms, 10000);
        assert_eq!(config.min_margin, dec!(0.01));
    }

    #[test]
    fn test_chase_result_serialization() {
        let result = ChaseResult::from_immediate_fill(
            dec!(100),
            dec!(0.45),
            dec!(0.045),
        );

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("success"));
        assert!(json.contains("filled_size"));

        let deserialized: ChaseResult = serde_json::from_str(&json).unwrap();
        assert!(deserialized.success);
        assert_eq!(deserialized.filled_size, dec!(100));
    }

    #[test]
    fn test_chase_fill_serialization() {
        let fill = ChaseFill {
            size: dec!(50),
            price: dec!(0.45),
            fee: dec!(0.0225),
            iteration: 1,
        };

        let json = serde_json::to_string(&fill).unwrap();
        let deserialized: ChaseFill = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.size, dec!(50));
        assert_eq!(deserialized.price, dec!(0.45));
        assert_eq!(deserialized.iteration, 1);
    }

    #[test]
    fn test_price_chaser_with_defaults() {
        let chaser = PriceChaser::with_defaults();
        assert!(chaser.is_enabled());
    }
}
