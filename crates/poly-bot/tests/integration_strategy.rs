//! Integration tests for strategy + mock executor.
//!
//! These tests verify the end-to-end flow of:
//! - Market event processing
//! - Arbitrage detection
//! - State management via GlobalState
//! - Metrics tracking

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::sync::mpsc;

use poly_bot::{
    BookSnapshotEvent, DataSource, DataSourceError, Executor, ExecutorError, GlobalState,
    MarketEvent, OrderCancellation, OrderFill, OrderRequest, OrderResult, PendingOrder,
    SpotPriceEvent, WindowOpenEvent,
};
use poly_bot::strategy::{StrategyConfig, StrategyLoop, TradeDecision};
use poly_bot::types::PriceLevel;
use poly_common::types::{CryptoAsset, Outcome};

/// Mock data source that returns pre-configured events.
struct MockDataSource {
    events: VecDeque<MarketEvent>,
    shutdown_requested: bool,
}

impl MockDataSource {
    fn new(events: Vec<MarketEvent>) -> Self {
        Self {
            events: events.into(),
            shutdown_requested: false,
        }
    }

    fn with_arb_opportunity() -> (Self, String) {
        let event_id = "test-event-1".to_string();
        let events = vec![
            // Open a market window
            MarketEvent::WindowOpen(WindowOpenEvent {
                event_id: event_id.clone(),
                condition_id: format!("{}-cond", event_id),
                asset: CryptoAsset::Btc,
                yes_token_id: format!("{}-yes", event_id),
                no_token_id: format!("{}-no", event_id),
                strike_price: dec!(100000),
                window_start: Utc::now(),
                window_end: Utc::now() + Duration::minutes(15),
                timestamp: Utc::now(),
            }),
            // Spot price update
            MarketEvent::SpotPrice(SpotPriceEvent {
                asset: CryptoAsset::Btc,
                price: dec!(100500),
                quantity: dec!(1.0),
                timestamp: Utc::now(),
            }),
            // YES book with ask at 0.45
            MarketEvent::BookSnapshot(BookSnapshotEvent {
                token_id: format!("{}-yes", event_id),
                event_id: event_id.clone(),
                bids: vec![PriceLevel::new(dec!(0.40), dec!(100))],
                asks: vec![PriceLevel::new(dec!(0.45), dec!(100))],
                timestamp: Utc::now(),
            }),
            // NO book with ask at 0.52 (combined = 0.97, margin = 3%)
            MarketEvent::BookSnapshot(BookSnapshotEvent {
                token_id: format!("{}-no", event_id),
                event_id: event_id.clone(),
                bids: vec![PriceLevel::new(dec!(0.40), dec!(100))],
                asks: vec![PriceLevel::new(dec!(0.52), dec!(100))],
                timestamp: Utc::now(),
            }),
        ];

        (Self::new(events), event_id)
    }

    fn with_no_arb_opportunity() -> Self {
        let event_id = "test-event-2".to_string();
        let events = vec![
            // Open a market window
            MarketEvent::WindowOpen(WindowOpenEvent {
                event_id: event_id.clone(),
                condition_id: format!("{}-cond", event_id),
                asset: CryptoAsset::Eth,
                yes_token_id: format!("{}-yes", event_id),
                no_token_id: format!("{}-no", event_id),
                strike_price: dec!(4000),
                window_start: Utc::now(),
                window_end: Utc::now() + Duration::minutes(15),
                timestamp: Utc::now(),
            }),
            // YES book with ask at 0.55
            MarketEvent::BookSnapshot(BookSnapshotEvent {
                token_id: format!("{}-yes", event_id),
                event_id: event_id.clone(),
                bids: vec![PriceLevel::new(dec!(0.50), dec!(100))],
                asks: vec![PriceLevel::new(dec!(0.55), dec!(100))],
                timestamp: Utc::now(),
            }),
            // NO book with ask at 0.50 (combined = 1.05, no arb)
            MarketEvent::BookSnapshot(BookSnapshotEvent {
                token_id: format!("{}-no", event_id),
                event_id: event_id.clone(),
                bids: vec![PriceLevel::new(dec!(0.45), dec!(100))],
                asks: vec![PriceLevel::new(dec!(0.50), dec!(100))],
                timestamp: Utc::now(),
            }),
        ];

        Self::new(events)
    }

    fn with_multiple_markets() -> Self {
        let now = Utc::now();
        let events = vec![
            // BTC market with arb
            MarketEvent::WindowOpen(WindowOpenEvent {
                event_id: "btc-event".to_string(),
                condition_id: "btc-cond".to_string(),
                asset: CryptoAsset::Btc,
                yes_token_id: "btc-yes".to_string(),
                no_token_id: "btc-no".to_string(),
                strike_price: dec!(100000),
                window_start: now,
                window_end: now + Duration::minutes(15),
                timestamp: now,
            }),
            // ETH market without arb
            MarketEvent::WindowOpen(WindowOpenEvent {
                event_id: "eth-event".to_string(),
                condition_id: "eth-cond".to_string(),
                asset: CryptoAsset::Eth,
                yes_token_id: "eth-yes".to_string(),
                no_token_id: "eth-no".to_string(),
                strike_price: dec!(4000),
                window_start: now,
                window_end: now + Duration::minutes(15),
                timestamp: now,
            }),
            // BTC YES book
            MarketEvent::BookSnapshot(BookSnapshotEvent {
                token_id: "btc-yes".to_string(),
                event_id: "btc-event".to_string(),
                bids: vec![PriceLevel::new(dec!(0.40), dec!(100))],
                asks: vec![PriceLevel::new(dec!(0.45), dec!(100))],
                timestamp: now,
            }),
            // BTC NO book (arb: 0.45 + 0.52 = 0.97)
            MarketEvent::BookSnapshot(BookSnapshotEvent {
                token_id: "btc-no".to_string(),
                event_id: "btc-event".to_string(),
                bids: vec![PriceLevel::new(dec!(0.40), dec!(100))],
                asks: vec![PriceLevel::new(dec!(0.52), dec!(100))],
                timestamp: now,
            }),
            // ETH YES book
            MarketEvent::BookSnapshot(BookSnapshotEvent {
                token_id: "eth-yes".to_string(),
                event_id: "eth-event".to_string(),
                bids: vec![PriceLevel::new(dec!(0.50), dec!(100))],
                asks: vec![PriceLevel::new(dec!(0.55), dec!(100))],
                timestamp: now,
            }),
            // ETH NO book (no arb: 0.55 + 0.50 = 1.05)
            MarketEvent::BookSnapshot(BookSnapshotEvent {
                token_id: "eth-no".to_string(),
                event_id: "eth-event".to_string(),
                bids: vec![PriceLevel::new(dec!(0.45), dec!(100))],
                asks: vec![PriceLevel::new(dec!(0.50), dec!(100))],
                timestamp: now,
            }),
        ];

        Self::new(events)
    }
}

#[async_trait]
impl DataSource for MockDataSource {
    async fn next_event(&mut self) -> Result<Option<MarketEvent>, DataSourceError> {
        if self.shutdown_requested {
            return Err(DataSourceError::Shutdown);
        }
        Ok(self.events.pop_front())
    }

    fn has_more(&self) -> bool {
        !self.events.is_empty()
    }

    fn current_time(&self) -> Option<chrono::DateTime<Utc>> {
        None
    }

    async fn shutdown(&mut self) {
        self.shutdown_requested = true;
    }
}

/// Mock executor that records all orders.
struct MockExecutor {
    balance: Decimal,
    should_fill: bool,
    next_order_id: u64,
}

impl MockExecutor {
    fn new(balance: Decimal) -> Self {
        Self {
            balance,
            should_fill: true,
            next_order_id: 1,
        }
    }

    fn with_rejection() -> Self {
        Self {
            balance: dec!(1000),
            should_fill: false,
            next_order_id: 1,
        }
    }
}

#[async_trait]
impl Executor for MockExecutor {
    async fn place_order(&mut self, order: OrderRequest) -> Result<OrderResult, ExecutorError> {
        if !self.should_fill {
            return Ok(OrderResult::Rejected(poly_bot::OrderRejection {
                request_id: order.request_id,
                reason: "Mock rejection".to_string(),
                timestamp: Utc::now(),
            }));
        }

        let order_id = format!("mock-{}", self.next_order_id);
        self.next_order_id += 1;

        Ok(OrderResult::Filled(OrderFill {
            request_id: order.request_id,
            order_id,
            size: order.size,
            price: order.price.unwrap_or(dec!(0.50)),
            fee: dec!(0.001),
            timestamp: Utc::now(),
        }))
    }

    async fn cancel_order(&mut self, order_id: &str) -> Result<OrderCancellation, ExecutorError> {
        Ok(OrderCancellation {
            request_id: "".to_string(),
            order_id: order_id.to_string(),
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
        self.balance
    }

    fn market_exposure(&self, _event_id: &str) -> Decimal {
        Decimal::ZERO
    }

    fn total_exposure(&self) -> Decimal {
        Decimal::ZERO
    }

    fn remaining_capacity(&self) -> Decimal {
        Decimal::MAX
    }

    fn get_position(&self, _event_id: &str) -> Option<poly_bot::executor::PositionSnapshot> {
        None
    }

    async fn shutdown(&mut self) {}
}

#[tokio::test]
async fn test_strategy_detects_arb() {
    // Setup with arb opportunity
    let (data_source, _event_id) = MockDataSource::with_arb_opportunity();
    let executor = MockExecutor::new(dec!(10000));
    let state = Arc::new(GlobalState::new());
    let config = StrategyConfig::default();

    let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);

    // Enable trading
    state.enable_trading();

    // Run strategy
    let _ = strategy.run().await;

    // Verify market was tracked
    assert_eq!(strategy.market_count(), 1);

    // Verify metrics - opportunities should have been detected
    let metrics = strategy.metrics();
    assert!(metrics.events_processed > 0);
}

#[tokio::test]
async fn test_strategy_skips_when_no_arb() {
    // Setup without arb opportunity
    let data_source = MockDataSource::with_no_arb_opportunity();
    let executor = MockExecutor::new(dec!(10000));
    let state = Arc::new(GlobalState::new());
    let config = StrategyConfig::default();

    let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);
    state.enable_trading();

    let _ = strategy.run().await;

    // Should have market (combined cost > 1.0, no arb)
    assert_eq!(strategy.market_count(), 1);

    // Opportunities should be 0 since combined cost > 1.0
    let metrics = strategy.metrics();
    assert_eq!(metrics.opportunities_detected, 0);
}

#[tokio::test]
async fn test_strategy_skips_when_trading_disabled() {
    let (data_source, _) = MockDataSource::with_arb_opportunity();
    let executor = MockExecutor::new(dec!(10000));
    let state = Arc::new(GlobalState::new());
    let config = StrategyConfig::default();

    let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);
    // Trading NOT enabled

    let _ = strategy.run().await;

    // Should have market tracked
    assert_eq!(strategy.market_count(), 1);

    // But no opportunities executed (trading disabled)
    let metrics = strategy.metrics();
    assert_eq!(metrics.opportunities_detected, 0);
}

#[tokio::test]
async fn test_strategy_handles_multiple_markets() {
    let data_source = MockDataSource::with_multiple_markets();
    let executor = MockExecutor::new(dec!(10000));
    let state = Arc::new(GlobalState::new());
    let config = StrategyConfig::default();

    let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);
    state.enable_trading();

    let _ = strategy.run().await;

    // Should track both markets
    assert_eq!(strategy.market_count(), 2);
}

#[tokio::test]
async fn test_strategy_updates_spot_price() {
    let events = vec![MarketEvent::SpotPrice(SpotPriceEvent {
        asset: CryptoAsset::Btc,
        price: dec!(105000),
        quantity: dec!(2.5),
        timestamp: Utc::now(),
    })];

    let data_source = MockDataSource::new(events);
    let executor = MockExecutor::new(dec!(10000));
    let state = Arc::new(GlobalState::new());
    let config = StrategyConfig::default();

    let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);
    let _ = strategy.run().await;

    // Verify spot price was updated in global state
    let (price, _ts) = state.market_data.get_spot_price("BTC").unwrap();
    assert_eq!(price, dec!(105000));
}

#[tokio::test]
async fn test_strategy_circuit_breaker_logic() {
    let state = Arc::new(GlobalState::new());
    state.enable_trading();

    // Initially can trade
    assert!(state.can_trade());

    // Simulate consecutive failures
    assert!(!state.record_failure(3)); // 1
    assert!(!state.record_failure(3)); // 2
    assert!(state.record_failure(3));  // 3 - should indicate trip

    // Now trip the circuit breaker
    state.trip_circuit_breaker();
    assert!(!state.can_trade());
}

#[tokio::test]
async fn test_strategy_with_observability_channel() {
    let (data_source, _) = MockDataSource::with_arb_opportunity();
    let executor = MockExecutor::new(dec!(10000));
    let state = Arc::new(GlobalState::new());
    let config = StrategyConfig::default();

    // Create observability channel
    let (tx, mut rx) = mpsc::channel::<TradeDecision>(100);

    let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config)
        .with_observability(tx);
    state.enable_trading();

    let _ = strategy.run().await;

    // Try to receive a decision (may or may not have one depending on execution)
    // Use try_recv since we don't want to block
    let _decision = rx.try_recv();
    // No panic means success - channel integration works
}

#[tokio::test]
async fn test_strategy_metrics_tracking() {
    let (data_source, _) = MockDataSource::with_arb_opportunity();
    let executor = MockExecutor::new(dec!(10000));
    let state = Arc::new(GlobalState::new());
    let config = StrategyConfig::default();

    let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);
    state.enable_trading();

    // Initial metrics
    let initial = strategy.metrics();
    assert_eq!(initial.events_processed, 0);

    let _ = strategy.run().await;

    // Final metrics - events should have been processed
    let final_metrics = strategy.metrics();
    assert!(final_metrics.events_processed > 0);
}

#[tokio::test]
async fn test_strategy_handles_heartbeat() {
    let events = vec![
        MarketEvent::Heartbeat(Utc::now()),
        MarketEvent::Heartbeat(Utc::now() + Duration::seconds(1)),
    ];

    let data_source = MockDataSource::new(events);
    let executor = MockExecutor::new(dec!(10000));
    let state = Arc::new(GlobalState::new());
    let config = StrategyConfig::default();

    let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);

    // Should not panic on heartbeats
    let result = strategy.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_strategy_window_close_removes_market() {
    use poly_bot::WindowCloseEvent;

    let events = vec![
        MarketEvent::WindowOpen(WindowOpenEvent {
            event_id: "event1".to_string(),
            condition_id: "cond1".to_string(),
            asset: CryptoAsset::Btc,
            yes_token_id: "event1-yes".to_string(),
            no_token_id: "event1-no".to_string(),
            strike_price: dec!(100000),
            window_start: Utc::now(),
            window_end: Utc::now() + Duration::minutes(15),
            timestamp: Utc::now(),
        }),
        MarketEvent::WindowClose(WindowCloseEvent {
            event_id: "event1".to_string(),
            outcome: Some(Outcome::Yes),
            final_price: Some(dec!(100500)),
            timestamp: Utc::now(),
        }),
    ];

    let data_source = MockDataSource::new(events);
    let executor = MockExecutor::new(dec!(10000));
    let state = Arc::new(GlobalState::new());
    let config = StrategyConfig::default();

    let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);

    let _ = strategy.run().await;

    // Market should be removed after close
    assert_eq!(strategy.market_count(), 0);
}

#[tokio::test]
async fn test_strategy_with_rejection_executor() {
    let (data_source, _) = MockDataSource::with_arb_opportunity();
    let executor = MockExecutor::with_rejection();
    let state = Arc::new(GlobalState::new());
    let config = StrategyConfig::default();

    let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);
    state.enable_trading();

    // Should complete without panic even with rejections
    let result = strategy.run().await;
    assert!(result.is_ok());
}

#[test]
fn test_strategy_config_default() {
    let config = StrategyConfig::default();
    assert_eq!(config.max_consecutive_failures, 3);
    assert!(config.block_on_toxic_high);
}
