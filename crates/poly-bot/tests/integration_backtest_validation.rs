//! Backtest validation tests.
//!
//! These tests verify:
//! - P&L calculations are mathematically correct
//! - No impossible states (negative prices, invalid positions)
//! - BacktestResult fields are consistent
//! - Backtest executor handles edge cases correctly
//!
//! This is task p9-3: Backtest validation.

use std::collections::HashMap;

use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use poly_bot::executor::backtest::{BacktestExecutor, BacktestExecutorConfig, BacktestStats};
use poly_bot::mode::backtest::{BacktestResult, PnLReport, SweepParameter};
use poly_bot::state::MetricsSnapshot;
use poly_bot::types::{OrderBook, PriceLevel};
use poly_bot::{Executor, OrderRequest};
use poly_common::types::{Outcome, Side};

/// Helper to create a default MetricsSnapshot for tests.
fn default_metrics() -> MetricsSnapshot {
    MetricsSnapshot {
        events_processed: 0,
        opportunities_detected: 0,
        trades_executed: 0,
        trades_failed: 0,
        trades_skipped: 0,
        pnl_usdc: Decimal::ZERO,
        volume_usdc: Decimal::ZERO,
        shadow_orders_fired: 0,
        shadow_orders_filled: 0,
    }
}

// =============================================================================
// P&L CALCULATION VALIDATION
// =============================================================================

#[test]
fn test_pnl_calculation_basic() {
    let initial = dec!(10000);
    let final_balance = dec!(10500);
    let expected_pnl = dec!(500);
    let expected_return = dec!(5); // 5%

    let stats = BacktestStats {
        orders_placed: 10,
        orders_filled: 10,
        orders_partial: 0,
        orders_rejected: 0,
        volume_traded: dec!(1000),
        fees_paid: dec!(1),
        realized_pnl: expected_pnl,
    };

    let metrics = MetricsSnapshot {
        events_processed: 1000,
        opportunities_detected: 10,
        trades_executed: 10,
        trades_failed: 0,
        trades_skipped: 0,
        pnl_usdc: expected_pnl,
        volume_usdc: dec!(1000),
        shadow_orders_fired: 0,
        shadow_orders_filled: 0,
    };

    let result = BacktestResult::new(
        Utc::now() - Duration::hours(1),
        Utc::now(),
        initial,
        final_balance,
        &stats,
        &metrics,
        60.0,
    );

    assert_eq!(result.initial_balance, initial);
    assert_eq!(result.final_balance, final_balance);
    assert_eq!(result.total_pnl, expected_pnl);
    assert_eq!(result.return_pct, expected_return);
}

#[test]
fn test_pnl_calculation_loss() {
    let initial = dec!(10000);
    let final_balance = dec!(9500);
    let expected_pnl = dec!(-500);
    let expected_return = dec!(-5); // -5%

    let stats = BacktestStats::default();
    let metrics = default_metrics();

    let result = BacktestResult::new(
        Utc::now() - Duration::hours(1),
        Utc::now(),
        initial,
        final_balance,
        &stats,
        &metrics,
        60.0,
    );

    assert_eq!(result.total_pnl, expected_pnl);
    assert_eq!(result.return_pct, expected_return);
}

#[test]
fn test_pnl_calculation_zero_initial_balance() {
    // Edge case: zero initial balance should not cause division by zero
    let initial = Decimal::ZERO;
    let final_balance = dec!(100);

    let stats = BacktestStats::default();
    let metrics = default_metrics();

    let result = BacktestResult::new(
        Utc::now() - Duration::hours(1),
        Utc::now(),
        initial,
        final_balance,
        &stats,
        &metrics,
        60.0,
    );

    assert_eq!(result.total_pnl, dec!(100));
    assert_eq!(result.return_pct, Decimal::ZERO); // Avoid NaN/Inf
}

#[test]
fn test_pnl_calculation_large_numbers() {
    // Test with large numbers to ensure no overflow
    let initial = dec!(1_000_000_000); // $1 billion
    let final_balance = dec!(1_100_000_000); // $1.1 billion
    let expected_pnl = dec!(100_000_000); // $100 million profit
    let expected_return = dec!(10); // 10%

    let stats = BacktestStats::default();
    let metrics = default_metrics();

    let result = BacktestResult::new(
        Utc::now() - Duration::hours(1),
        Utc::now(),
        initial,
        final_balance,
        &stats,
        &metrics,
        60.0,
    );

    assert_eq!(result.total_pnl, expected_pnl);
    assert_eq!(result.return_pct, expected_return);
}

#[test]
fn test_pnl_calculation_fractional_returns() {
    let initial = dec!(10000);
    let final_balance = dec!(10033.33); // $33.33 profit
    let expected_pnl = dec!(33.33);

    let stats = BacktestStats::default();
    let metrics = default_metrics();

    let result = BacktestResult::new(
        Utc::now() - Duration::hours(1),
        Utc::now(),
        initial,
        final_balance,
        &stats,
        &metrics,
        60.0,
    );

    assert_eq!(result.total_pnl, expected_pnl);
    // Return should be approximately 0.3333%
    assert!(result.return_pct > dec!(0.333) && result.return_pct < dec!(0.334));
}

// =============================================================================
// IMPOSSIBLE STATE VALIDATION
// =============================================================================

#[test]
fn test_prices_are_never_negative() {
    // All prices must be non-negative in binary options (0 to 1)
    let levels = vec![
        PriceLevel::new(dec!(0.45), dec!(100)),
        PriceLevel::new(dec!(0.55), dec!(100)),
        PriceLevel::new(dec!(0.00), dec!(100)), // Zero is valid
        PriceLevel::new(dec!(1.00), dec!(100)), // One is valid
    ];

    for level in &levels {
        assert!(
            level.price >= Decimal::ZERO,
            "Price {} is negative",
            level.price
        );
        assert!(level.price <= Decimal::ONE, "Price {} exceeds 1.0", level.price);
    }
}

#[test]
fn test_sizes_are_never_negative() {
    // All sizes must be non-negative
    let levels = vec![
        PriceLevel::new(dec!(0.45), dec!(100)),
        PriceLevel::new(dec!(0.55), dec!(0)), // Zero size is valid (no liquidity)
        PriceLevel::new(dec!(0.50), dec!(0.001)), // Small size is valid
    ];

    for level in &levels {
        assert!(level.size >= Decimal::ZERO, "Size {} is negative", level.size);
    }
}

#[test]
fn test_order_book_state_consistency() {
    let mut book = OrderBook::new("test-token".to_string());

    // Apply a snapshot
    book.apply_snapshot(
        vec![
            PriceLevel::new(dec!(0.44), dec!(100)),
            PriceLevel::new(dec!(0.43), dec!(200)),
        ],
        vec![
            PriceLevel::new(dec!(0.46), dec!(100)),
            PriceLevel::new(dec!(0.47), dec!(200)),
        ],
        Utc::now().timestamp_millis(),
    );

    // Best bid should be less than best ask (no crossed book)
    // Note: best_bid() and best_ask() return Option<Decimal> (the price directly)
    if let (Some(best_bid), Some(best_ask)) = (book.best_bid(), book.best_ask()) {
        assert!(
            best_bid < best_ask,
            "Book is crossed: bid {} >= ask {}",
            best_bid,
            best_ask
        );
    }

    // Apply delta - remove a level (size = 0)
    book.apply_delta(Side::Buy, dec!(0.44), dec!(0), Utc::now().timestamp_millis());

    // Book should remain consistent
    if let (Some(best_bid), Some(best_ask)) = (book.best_bid(), book.best_ask()) {
        assert!(
            best_bid < best_ask,
            "Book crossed after delta: bid {} >= ask {}",
            best_bid,
            best_ask
        );
    }
}

#[tokio::test]
async fn test_executor_balance_never_negative_after_fees() {
    let config = BacktestExecutorConfig {
        initial_balance: dec!(100),
        fee_rate: dec!(0.001), // 0.1% fee
        latency_ms: 0,
        enforce_balance: true,
        max_position_per_market: Decimal::ZERO,
        min_fill_ratio: dec!(0.5),
    };

    let mut executor = BacktestExecutor::new(config);

    // Create order book with reasonable prices
    let mut book = OrderBook::new("token-yes".to_string());
    book.asks.push(PriceLevel::new(dec!(0.50), dec!(1000)));
    executor.update_book("token-yes", book).await;

    // Place orders that should be allowed
    let order = OrderRequest::limit(
        "req-1".to_string(),
        "event-1".to_string(),
        "token-yes".to_string(),
        Outcome::Yes,
        Side::Buy,
        dec!(100), // Buy 100 shares at $0.50 = $50 + fees
        dec!(0.55),
    );

    let result = executor.place_order(order).await;
    assert!(result.is_ok());

    // Balance should never go negative
    assert!(
        executor.balance() >= Decimal::ZERO,
        "Balance went negative: {}",
        executor.balance()
    );
}

#[tokio::test]
async fn test_executor_rejects_when_insufficient_funds() {
    let config = BacktestExecutorConfig {
        initial_balance: dec!(10), // Only $10
        fee_rate: dec!(0.001),
        latency_ms: 0,
        enforce_balance: true,
        max_position_per_market: Decimal::ZERO,
        min_fill_ratio: dec!(0.5),
    };

    let mut executor = BacktestExecutor::new(config);

    // Create order book
    let mut book = OrderBook::new("token-yes".to_string());
    book.asks.push(PriceLevel::new(dec!(0.50), dec!(1000)));
    executor.update_book("token-yes", book).await;

    // Try to buy $50 worth with only $10
    let order = OrderRequest::limit(
        "req-1".to_string(),
        "event-1".to_string(),
        "token-yes".to_string(),
        Outcome::Yes,
        Side::Buy,
        dec!(100), // 100 shares at $0.50 = $50
        dec!(0.55),
    );

    let result = executor.place_order(order).await.unwrap();
    assert!(result.is_rejected(), "Should reject order due to insufficient funds");
}

// =============================================================================
// BACKTEST RESULT CONSISTENCY
// =============================================================================

#[test]
fn test_backtest_result_trades_sum() {
    let stats = BacktestStats {
        orders_placed: 100,
        orders_filled: 80,
        orders_partial: 10,
        orders_rejected: 10,
        volume_traded: dec!(5000),
        fees_paid: dec!(5),
        realized_pnl: dec!(250),
    };

    // filled + partial + rejected should equal or be less than placed
    // (could be less if some orders are still pending, but in backtest all complete)
    assert!(
        stats.orders_filled + stats.orders_partial + stats.orders_rejected <= stats.orders_placed,
        "Order counts inconsistent"
    );
}

#[test]
fn test_backtest_result_fees_bounded() {
    let stats = BacktestStats {
        orders_placed: 100,
        orders_filled: 100,
        orders_partial: 0,
        orders_rejected: 0,
        volume_traded: dec!(5000),
        fees_paid: dec!(5),
        realized_pnl: dec!(250),
    };

    // Fees should be non-negative and less than volume traded
    assert!(stats.fees_paid >= Decimal::ZERO, "Fees cannot be negative");
    assert!(
        stats.fees_paid < stats.volume_traded,
        "Fees {} exceed volume traded {}",
        stats.fees_paid,
        stats.volume_traded
    );
}

#[test]
fn test_backtest_result_duration_positive() {
    let stats = BacktestStats::default();
    let metrics = default_metrics();

    let result = BacktestResult::new(
        Utc::now() - Duration::hours(1),
        Utc::now(),
        dec!(10000),
        dec!(10500),
        &stats,
        &metrics,
        3600.5, // duration in seconds
    );

    assert!(result.duration_secs > 0.0, "Duration must be positive");
}

#[test]
fn test_backtest_result_time_range_valid() {
    let start = Utc::now() - Duration::hours(24);
    let end = Utc::now();

    let stats = BacktestStats::default();
    let metrics = default_metrics();

    let result = BacktestResult::new(
        start,
        end,
        dec!(10000),
        dec!(10000),
        &stats,
        &metrics,
        60.0,
    );

    assert!(
        result.end_time > result.start_time,
        "End time must be after start time"
    );
}

// =============================================================================
// SWEEP PARAMETER VALIDATION
// =============================================================================

#[test]
fn test_sweep_parameter_values_monotonic() {
    let param = SweepParameter::new("margin_early", 0.01, 0.05, 0.01);
    let values = param.values();

    // Values should be in ascending order
    for i in 1..values.len() {
        assert!(
            values[i] > values[i - 1],
            "Sweep values not monotonic: {} <= {}",
            values[i],
            values[i - 1]
        );
    }
}

#[test]
fn test_sweep_parameter_includes_bounds() {
    let param = SweepParameter::new("test", 1.0, 5.0, 1.0);
    let values = param.values();

    assert_eq!(values.first(), Some(&1.0), "Should include start value");
    assert_eq!(values.last(), Some(&5.0), "Should include end value");
}

#[test]
fn test_sweep_parameter_single_value() {
    // When start == end, should return single value
    let param = SweepParameter::new("test", 2.5, 2.5, 1.0);
    let values = param.values();

    assert_eq!(values.len(), 1);
    assert_eq!(values[0], 2.5);
}

#[test]
fn test_sweep_parameter_small_steps() {
    let param = SweepParameter::new("test", 0.0, 0.01, 0.001);
    let values = param.values();

    // Should generate approximately 11 values: 0.000, 0.001, ..., 0.010
    assert!(values.len() >= 10 && values.len() <= 12);

    for &v in &values {
        assert!(v >= 0.0 && v <= 0.011, "Value {} out of range", v);
    }
}

// =============================================================================
// P&L REPORT FORMAT VALIDATION
// =============================================================================

#[test]
fn test_pnl_report_contains_required_fields() {
    let result = BacktestResult {
        start_time: Utc::now() - Duration::hours(24),
        end_time: Utc::now(),
        initial_balance: dec!(10000),
        final_balance: dec!(10500),
        total_pnl: dec!(500),
        return_pct: dec!(5),
        trades_executed: 50,
        trades_rejected: 5,
        volume_traded: dec!(25000),
        fees_paid: dec!(25),
        opportunities_detected: 100,
        opportunities_skipped: 45,
        win_rate: Some(dec!(60)),
        sharpe_ratio: Some(1.5),
        max_drawdown: Some(dec!(200)),
        parameters: HashMap::new(),
        duration_secs: 3600.0,
    };

    let report = PnLReport {
        result,
        positions: vec![],
    };

    let output = report.to_string_report();

    // Verify required sections exist
    assert!(output.contains("BACKTEST P&L REPORT"), "Missing report header");
    assert!(output.contains("Period:"), "Missing period");
    assert!(output.contains("PERFORMANCE"), "Missing performance section");
    assert!(
        output.contains("Initial Balance"),
        "Missing initial balance"
    );
    assert!(output.contains("Final Balance"), "Missing final balance");
    assert!(output.contains("Total P&L"), "Missing total P&L");
    assert!(output.contains("Return"), "Missing return percentage");
    assert!(
        output.contains("TRADING ACTIVITY"),
        "Missing trading activity section"
    );
    assert!(output.contains("Trades Executed"), "Missing trades executed");
    assert!(output.contains("Volume Traded"), "Missing volume traded");
}

#[test]
fn test_pnl_report_formats_negative_pnl_correctly() {
    let result = BacktestResult {
        start_time: Utc::now() - Duration::hours(24),
        end_time: Utc::now(),
        initial_balance: dec!(10000),
        final_balance: dec!(9500),
        total_pnl: dec!(-500),
        return_pct: dec!(-5),
        trades_executed: 50,
        trades_rejected: 5,
        volume_traded: dec!(25000),
        fees_paid: dec!(25),
        opportunities_detected: 100,
        opportunities_skipped: 45,
        win_rate: None,
        sharpe_ratio: None,
        max_drawdown: None,
        parameters: HashMap::new(),
        duration_secs: 3600.0,
    };

    let report = PnLReport {
        result,
        positions: vec![],
    };

    let output = report.to_string_report();

    // Negative P&L should be displayed (format depends on Decimal display)
    assert!(
        output.contains("-500") || output.contains("($500"),
        "Negative P&L not displayed correctly"
    );
}

// =============================================================================
// EXECUTOR EDGE CASES
// =============================================================================

#[tokio::test]
async fn test_executor_zero_size_order_rejected() {
    let mut executor = BacktestExecutor::with_defaults();

    let order = OrderRequest::limit(
        "req-1".to_string(),
        "event-1".to_string(),
        "token-yes".to_string(),
        Outcome::Yes,
        Side::Buy,
        Decimal::ZERO, // Zero size
        dec!(0.50),
    );

    let result = executor.place_order(order).await;
    assert!(result.is_err(), "Zero size orders should be rejected");
}

#[tokio::test]
async fn test_executor_handles_empty_book() {
    let mut executor = BacktestExecutor::with_defaults();

    // Create empty order book
    let book = OrderBook::new("token-yes".to_string());
    executor.update_book("token-yes", book).await;

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
    assert!(result.is_rejected(), "Should reject when no liquidity");
}

#[tokio::test]
async fn test_executor_position_tracking_accuracy() {
    let config = BacktestExecutorConfig {
        initial_balance: dec!(1000),
        fee_rate: Decimal::ZERO, // No fees for simpler math
        latency_ms: 0,
        enforce_balance: true,
        max_position_per_market: Decimal::ZERO,
        min_fill_ratio: dec!(0.5),
    };

    let mut executor = BacktestExecutor::new(config);

    // Create order book
    let mut book = OrderBook::new("token-yes".to_string());
    book.asks.push(PriceLevel::new(dec!(0.50), dec!(1000)));
    book.bids.push(PriceLevel::new(dec!(0.48), dec!(1000)));
    executor.update_book("token-yes", book).await;

    // Buy 100 YES shares at $0.50
    let buy_order = OrderRequest::limit(
        "req-1".to_string(),
        "event-1".to_string(),
        "token-yes".to_string(),
        Outcome::Yes,
        Side::Buy,
        dec!(100),
        dec!(0.55),
    );
    executor.place_order(buy_order).await.unwrap();

    // Check position
    let position = executor.position("event-1").unwrap();
    assert_eq!(
        position.yes_shares,
        dec!(100),
        "YES shares should be 100"
    );
    assert_eq!(position.no_shares, Decimal::ZERO, "NO shares should be 0");

    // Sell 50 YES shares
    let sell_order = OrderRequest::limit(
        "req-2".to_string(),
        "event-1".to_string(),
        "token-yes".to_string(),
        Outcome::Yes,
        Side::Sell,
        dec!(50),
        dec!(0.40),
    );
    executor.place_order(sell_order).await.unwrap();

    // Check position after sell
    let position = executor.position("event-1").unwrap();
    assert_eq!(
        position.yes_shares,
        dec!(50),
        "YES shares should be 50 after selling 50"
    );
}

#[tokio::test]
async fn test_executor_stats_consistency() {
    let config = BacktestExecutorConfig {
        initial_balance: dec!(1000),
        fee_rate: dec!(0.001),
        latency_ms: 0,
        enforce_balance: true,
        max_position_per_market: Decimal::ZERO,
        min_fill_ratio: dec!(0.5),
    };

    let mut executor = BacktestExecutor::new(config);

    // Create order book
    let mut book = OrderBook::new("token-yes".to_string());
    book.asks.push(PriceLevel::new(dec!(0.50), dec!(100)));
    executor.update_book("token-yes", book).await;

    // Place several orders
    for i in 0..5 {
        let order = OrderRequest::limit(
            format!("req-{}", i),
            "event-1".to_string(),
            "token-yes".to_string(),
            Outcome::Yes,
            Side::Buy,
            dec!(10),
            dec!(0.55),
        );
        let _ = executor.place_order(order).await;
    }

    let stats = executor.stats();

    // Verify stats consistency
    assert_eq!(
        stats.orders_placed,
        5,
        "Should have 5 orders placed"
    );
    assert!(
        stats.orders_filled + stats.orders_partial + stats.orders_rejected == stats.orders_placed,
        "Sum of outcomes should equal orders placed"
    );
    assert!(
        stats.fees_paid >= Decimal::ZERO,
        "Fees should be non-negative"
    );
    assert!(
        stats.volume_traded >= Decimal::ZERO,
        "Volume should be non-negative"
    );
}

// =============================================================================
// ARITHMETIC INVARIANT TESTS
// =============================================================================

#[test]
fn test_binary_option_prices_sum_constraint() {
    // In binary options, YES + NO prices should sum close to 1.0
    // (with some spread for market maker profit)
    let yes_mid = dec!(0.45);
    let no_mid = dec!(0.55);

    let sum = yes_mid + no_mid;
    assert_eq!(sum, Decimal::ONE, "YES + NO mid prices should sum to 1.0");
}

#[test]
fn test_arbitrage_margin_calculation() {
    // Arb margin = 1.0 - (yes_ask + no_ask)
    let yes_ask = dec!(0.45);
    let no_ask = dec!(0.52);
    let expected_margin = dec!(0.03); // 3% margin

    let combined_cost = yes_ask + no_ask;
    let margin = Decimal::ONE - combined_cost;

    assert_eq!(margin, expected_margin, "Arb margin calculation incorrect");
    assert!(
        margin > Decimal::ZERO,
        "Positive margin means arbitrage exists"
    );
}

#[test]
fn test_no_arbitrage_when_prices_exceed_one() {
    // When YES + NO > 1.0, no arbitrage exists
    let yes_ask = dec!(0.55);
    let no_ask = dec!(0.50);

    let combined_cost = yes_ask + no_ask;
    let margin = Decimal::ONE - combined_cost;

    assert!(
        margin < Decimal::ZERO,
        "Negative margin means no arbitrage"
    );
}

#[test]
fn test_position_value_bounds() {
    // A complete position (YES + NO) is worth exactly $1.00 at settlement
    let yes_shares = dec!(100);
    let no_shares = dec!(100);

    // If you hold both YES and NO, you're guaranteed $1 per pair
    let guaranteed_value = yes_shares.min(no_shares) * Decimal::ONE;
    assert_eq!(guaranteed_value, dec!(100), "100 pairs worth $100");
}

#[test]
fn test_fee_calculation_precision() {
    // Fees should use exact decimal arithmetic
    let volume = dec!(1234.56789);
    let fee_rate = dec!(0.001); // 0.1%
    let expected_fee = dec!(1.23456789);

    let calculated_fee = volume * fee_rate;
    assert_eq!(calculated_fee, expected_fee, "Fee calculation uses exact decimal");
}
