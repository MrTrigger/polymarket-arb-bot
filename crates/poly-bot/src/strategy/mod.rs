//! Trading strategy implementations.
//!
//! This module contains the core strategy logic for the arbitrage bot:
//! - Arbitrage detection and opportunity scoring
//! - Toxic flow detection
//! - Position sizing
//! - Main strategy event loop
//!
//! ## Hot Path Requirements
//!
//! The strategy loop runs on the hot path and must be fast:
//! - Arb detection: <1μs
//! - Toxic flow check: <1μs
//! - No allocations in critical path
//!
//! ## Strategy Loop
//!
//! The main strategy loop (`StrategyLoop`) processes market events and generates
//! trade decisions:
//!
//! 1. Receive event from `DataSource`
//! 2. Update internal state (prices, order books)
//! 3. Check `can_trade()` (single atomic load)
//! 4. For each active market, detect arbitrage opportunities
//! 5. Check for toxic flow warnings
//! 6. Calculate position size
//! 7. Send order to executor
//! 8. Fire-and-forget decision to observability channel

pub mod arb;
pub mod confidence;
pub mod confidence_sizing;
pub mod signal;
pub mod sizing;
pub mod toxic;

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use poly_common::types::{CryptoAsset, Outcome, Side};

use crate::config::TradingConfig;
use crate::data_source::{
    BookDeltaEvent, BookSnapshotEvent, DataSource, DataSourceError, FillEvent, MarketEvent,
    SpotPriceEvent, WindowCloseEvent, WindowOpenEvent,
};
use crate::executor::{Executor, ExecutorError, OrderRequest};
use crate::state::GlobalState;
use crate::types::{Inventory, MarketState};

pub use arb::{ArbDetector, ArbOpportunity, ArbRejection, ArbThresholds};
pub use confidence::{
    Confidence, ConfidenceCalculator, ConfidenceFactors, MAX_MULTIPLIER, MIN_MULTIPLIER,
};
pub use confidence_sizing::{ConfidenceSizer, OrderSizeResult, SizeRejection};
pub use signal::{
    calculate_distance, distance_bps, get_signal, get_thresholds, Signal, SignalThresholds,
};
pub use sizing::{PositionSizer, SizingAdjustments, SizingConfig, SizingLimit, SizingResult};
pub use toxic::{
    ToxicFlowConfig, ToxicFlowDetector, ToxicFlowWarning, ToxicIndicators, ToxicSeverity,
};

/// Errors that can occur in the strategy loop.
#[derive(Debug, Error)]
pub enum StrategyError {
    #[error("Data source error: {0}")]
    DataSource(#[from] DataSourceError),

    #[error("Executor error: {0}")]
    Executor(#[from] ExecutorError),

    #[error("Shutdown requested")]
    Shutdown,

    #[error("Internal error: {0}")]
    Internal(String),
}

/// Decision made by the strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeDecision {
    /// Unique decision ID.
    pub decision_id: u64,
    /// Event ID for this market.
    pub event_id: String,
    /// Asset being traded.
    pub asset: CryptoAsset,
    /// The detected opportunity.
    pub opportunity: ArbOpportunity,
    /// Sizing result.
    pub sizing: SizingResult,
    /// Toxic flow warning (if any).
    pub toxic_warning: Option<ToxicFlowWarning>,
    /// Action taken.
    pub action: TradeAction,
    /// Decision timestamp.
    pub timestamp: DateTime<Utc>,
    /// Latency from event to decision (microseconds).
    pub latency_us: u64,
}

/// Action taken by the strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradeAction {
    /// Placed orders for both legs.
    Execute,
    /// Skipped due to sizing constraints.
    SkipSizing,
    /// Skipped due to toxic flow.
    SkipToxic,
    /// Skipped due to trading disabled.
    SkipDisabled,
    /// Skipped due to circuit breaker.
    SkipCircuitBreaker,
}

/// Internal market state tracked by the strategy.
#[derive(Debug)]
#[allow(dead_code)] // Fields used for debugging and future features
struct TrackedMarket {
    /// Event ID.
    event_id: String,
    /// Asset.
    asset: CryptoAsset,
    /// YES token ID.
    yes_token_id: String,
    /// NO token ID.
    no_token_id: String,
    /// Strike price.
    strike_price: Decimal,
    /// Window end time.
    window_end: DateTime<Utc>,
    /// Current market state.
    state: MarketState,
    /// Current inventory.
    inventory: Inventory,
    /// Latest toxic warning for YES token.
    yes_toxic_warning: Option<ToxicFlowWarning>,
    /// Latest toxic warning for NO token.
    no_toxic_warning: Option<ToxicFlowWarning>,
}

impl TrackedMarket {
    fn new(event: &WindowOpenEvent) -> Self {
        let state = MarketState::new(
            event.event_id.clone(),
            event.asset,
            event.yes_token_id.clone(),
            event.no_token_id.clone(),
            event.strike_price,
            (event.window_end - Utc::now()).num_seconds(),
        );
        Self {
            event_id: event.event_id.clone(),
            asset: event.asset,
            yes_token_id: event.yes_token_id.clone(),
            no_token_id: event.no_token_id.clone(),
            strike_price: event.strike_price,
            window_end: event.window_end,
            state,
            inventory: Inventory::new(event.event_id.clone()),
            yes_toxic_warning: None,
            no_toxic_warning: None,
        }
    }

    /// Update seconds remaining.
    fn update_time(&mut self) {
        self.state.seconds_remaining = (self.window_end - Utc::now()).num_seconds().max(0);
    }

    /// Check if window has expired.
    fn is_expired(&self) -> bool {
        Utc::now() >= self.window_end
    }
}

/// Configuration for the strategy loop.
#[derive(Debug, Clone)]
pub struct StrategyConfig {
    /// Arbitrage thresholds.
    pub arb_thresholds: ArbThresholds,
    /// Toxic flow detection config.
    pub toxic_config: ToxicFlowConfig,
    /// Position sizing config.
    pub sizing_config: SizingConfig,
    /// Maximum consecutive failures before circuit breaker trips.
    pub max_consecutive_failures: u32,
    /// Whether to block trades on high toxic severity.
    pub block_on_toxic_high: bool,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            arb_thresholds: ArbThresholds::default(),
            toxic_config: ToxicFlowConfig::default(),
            sizing_config: SizingConfig::default(),
            max_consecutive_failures: 3,
            block_on_toxic_high: true,
        }
    }
}

impl StrategyConfig {
    /// Create from trading config.
    pub fn from_trading_config(config: &TradingConfig) -> Self {
        Self {
            arb_thresholds: ArbThresholds {
                early: config.min_margin_early,
                mid: config.min_margin_mid,
                late: config.min_margin_late,
                early_threshold_secs: config.early_threshold_secs,
                mid_threshold_secs: config.mid_threshold_secs,
                min_time_remaining_secs: config.min_time_remaining_secs,
            },
            sizing_config: SizingConfig::from_trading_config(config),
            toxic_config: ToxicFlowConfig::default(),
            max_consecutive_failures: 3,
            block_on_toxic_high: true,
        }
    }
}

/// Main strategy loop that processes events and generates trade decisions.
///
/// The strategy loop:
/// 1. Receives events from a `DataSource`
/// 2. Updates internal state (prices, books, inventory)
/// 3. Detects arbitrage opportunities
/// 4. Checks for toxic flow
/// 5. Calculates position size
/// 6. Submits orders via `Executor`
/// 7. Sends decisions to observability channel
pub struct StrategyLoop<D: DataSource, E: Executor> {
    /// Data source for market events.
    data_source: D,
    /// Executor for order submission.
    executor: E,
    /// Global shared state.
    state: Arc<GlobalState>,
    /// Strategy configuration.
    config: StrategyConfig,
    /// Arbitrage detector.
    arb_detector: ArbDetector,
    /// Toxic flow detector.
    toxic_detector: ToxicFlowDetector,
    /// Position sizer.
    sizer: PositionSizer,
    /// Tracked markets by event_id.
    markets: HashMap<String, TrackedMarket>,
    /// Token ID to event ID mapping.
    token_to_event: HashMap<String, String>,
    /// Latest spot prices by asset.
    spot_prices: HashMap<CryptoAsset, Decimal>,
    /// Decision counter for unique IDs.
    decision_counter: u64,
    /// Observability channel sender (optional).
    obs_sender: Option<mpsc::Sender<TradeDecision>>,
}

impl<D: DataSource, E: Executor> StrategyLoop<D, E> {
    /// Create a new strategy loop.
    pub fn new(
        data_source: D,
        executor: E,
        state: Arc<GlobalState>,
        config: StrategyConfig,
    ) -> Self {
        let arb_detector = ArbDetector::new(config.arb_thresholds.clone());
        let toxic_detector = ToxicFlowDetector::new(config.toxic_config.clone());
        let sizer = PositionSizer::new(config.sizing_config.clone());

        Self {
            data_source,
            executor,
            state,
            config,
            arb_detector,
            toxic_detector,
            sizer,
            markets: HashMap::new(),
            token_to_event: HashMap::new(),
            spot_prices: HashMap::new(),
            decision_counter: 0,
            obs_sender: None,
        }
    }

    /// Set observability channel for decision capture.
    ///
    /// Decisions are sent via `try_send()` to avoid blocking the hot path.
    pub fn with_observability(mut self, sender: mpsc::Sender<TradeDecision>) -> Self {
        self.obs_sender = Some(sender);
        self
    }

    /// Run the strategy loop until shutdown or error.
    pub async fn run(&mut self) -> Result<(), StrategyError> {
        info!("Strategy loop starting");

        loop {
            // Check for shutdown
            if self.state.control.is_shutdown_requested() {
                info!("Shutdown requested, stopping strategy loop");
                return Err(StrategyError::Shutdown);
            }

            // Get next event
            let event = match self.data_source.next_event().await {
                Ok(Some(event)) => event,
                Ok(None) => {
                    debug!("Data source exhausted");
                    break;
                }
                Err(DataSourceError::Shutdown) => {
                    info!("Data source shutdown");
                    return Err(StrategyError::Shutdown);
                }
                Err(e) => {
                    warn!("Data source error: {}", e);
                    continue;
                }
            };

            // Process the event
            let start = std::time::Instant::now();
            self.process_event(event).await?;
            let elapsed_us = start.elapsed().as_micros() as u64;

            // Track metrics
            self.state.metrics.inc_events();

            // Log slow processing
            if elapsed_us > 1000 {
                debug!("Slow event processing: {}us", elapsed_us);
            }
        }

        Ok(())
    }

    /// Process a single market event.
    async fn process_event(&mut self, event: MarketEvent) -> Result<(), StrategyError> {
        let event_time = std::time::Instant::now();

        match event {
            MarketEvent::SpotPrice(e) => self.handle_spot_price(e),
            MarketEvent::BookSnapshot(e) => self.handle_book_snapshot(e),
            MarketEvent::BookDelta(e) => self.handle_book_delta(e),
            MarketEvent::Fill(e) => self.handle_fill(e).await?,
            MarketEvent::WindowOpen(e) => self.handle_window_open(e),
            MarketEvent::WindowClose(e) => self.handle_window_close(e),
            MarketEvent::Heartbeat(_) => {
                // Heartbeat - just update time remaining on all markets
                self.update_all_market_times();
                return Ok(());
            }
        }

        // After state update, check for opportunities in all active markets
        let latency_us = event_time.elapsed().as_micros() as u64;
        self.check_opportunities(latency_us).await?;

        Ok(())
    }

    /// Handle spot price update.
    fn handle_spot_price(&mut self, event: SpotPriceEvent) {
        trace!("Spot price: {} @ {}", event.asset, event.price);

        // Update local cache
        self.spot_prices.insert(event.asset, event.price);

        // Update global state
        self.state.market_data.update_spot_price(
            &event.asset.to_string(),
            event.price,
            event.timestamp.timestamp_millis(),
        );

        // Update all markets for this asset
        for market in self.markets.values_mut() {
            if market.asset == event.asset {
                market.state.spot_price = Some(event.price);
            }
        }
    }

    /// Handle order book snapshot.
    fn handle_book_snapshot(&mut self, event: BookSnapshotEvent) {
        trace!("Book snapshot: {} ({} bids, {} asks)",
            event.token_id, event.bids.len(), event.asks.len());

        // Find the market this token belongs to
        let event_id = match self.token_to_event.get(&event.token_id) {
            Some(id) => id.clone(),
            None => {
                trace!("Unknown token {}, ignoring snapshot", event.token_id);
                return;
            }
        };

        let market = match self.markets.get_mut(&event_id) {
            Some(m) => m,
            None => return,
        };

        // Determine which book to update and whether it's YES or NO
        let is_yes = event.token_id == market.yes_token_id;
        let book = if is_yes {
            &mut market.state.yes_book
        } else if event.token_id == market.no_token_id {
            &mut market.state.no_book
        } else {
            return;
        };

        // Apply snapshot
        book.apply_snapshot(
            event.bids,
            event.asks,
            event.timestamp.timestamp_millis(),
        );

        // Record for toxic flow tracking and store warning
        if let Some(best_ask_size) = book.best_ask_size() {
            let warning = self.toxic_detector.check_order(
                &event.token_id,
                Side::Sell,
                best_ask_size,
                event.timestamp.timestamp_millis(),
                book.best_bid(),
                book.best_ask(),
            );
            // Get market again after toxic_detector borrow ends
            if let Some(market) = self.markets.get_mut(&event_id) {
                if is_yes {
                    market.yes_toxic_warning = warning;
                } else {
                    market.no_toxic_warning = warning;
                }
            }
        }
    }

    /// Handle order book delta.
    fn handle_book_delta(&mut self, event: BookDeltaEvent) {
        trace!("Book delta: {} {} @ {} (size {})",
            event.token_id, event.side, event.price, event.size);

        // Find the market this token belongs to
        let event_id = match self.token_to_event.get(&event.token_id) {
            Some(id) => id.clone(),
            None => return,
        };

        let market = match self.markets.get_mut(&event_id) {
            Some(m) => m,
            None => return,
        };

        // Determine which book to update
        let is_yes = event.token_id == market.yes_token_id;
        let book = if is_yes {
            &mut market.state.yes_book
        } else if event.token_id == market.no_token_id {
            &mut market.state.no_book
        } else {
            return;
        };

        // Record for toxic flow detection before applying
        let warning = if event.size > Decimal::ZERO {
            self.toxic_detector.check_order(
                &event.token_id,
                event.side,
                event.size,
                event.timestamp.timestamp_millis(),
                book.best_bid(),
                book.best_ask(),
            )
        } else {
            None
        };

        // Apply delta
        book.apply_delta(
            event.side,
            event.price,
            event.size,
            event.timestamp.timestamp_millis(),
        );

        // Store toxic warning if detected
        if warning.is_some()
            && let Some(market) = self.markets.get_mut(&event_id)
        {
            if is_yes {
                market.yes_toxic_warning = warning;
            } else {
                market.no_toxic_warning = warning;
            }
        }
    }

    /// Handle fill event.
    async fn handle_fill(&mut self, event: FillEvent) -> Result<(), StrategyError> {
        info!("Fill: {} {} {} @ {}",
            event.event_id, event.outcome, event.size, event.price);

        // Update inventory
        if let Some(market) = self.markets.get_mut(&event.event_id) {
            let cost = event.size * event.price + event.fee;
            market.inventory.record_fill(event.outcome, event.size, cost);

            // Update global state inventory
            let pos = crate::state::InventoryPosition {
                event_id: event.event_id.clone(),
                yes_shares: market.inventory.yes_shares,
                no_shares: market.inventory.no_shares,
                yes_cost_basis: market.inventory.yes_cost_basis,
                no_cost_basis: market.inventory.no_cost_basis,
                realized_pnl: market.inventory.realized_pnl,
            };
            self.state.market_data.update_inventory(&event.event_id, pos);

            // Record success
            self.state.record_success();
        }

        Ok(())
    }

    /// Handle new market window.
    fn handle_window_open(&mut self, event: WindowOpenEvent) {
        info!("Window open: {} {} strike={}",
            event.event_id, event.asset, event.strike_price);

        // Create tracked market
        let market = TrackedMarket::new(&event);

        // Update token mapping
        self.token_to_event.insert(event.yes_token_id.clone(), event.event_id.clone());
        self.token_to_event.insert(event.no_token_id.clone(), event.event_id.clone());

        // Add to tracked markets
        self.markets.insert(event.event_id, market);
    }

    /// Handle window close.
    fn handle_window_close(&mut self, event: WindowCloseEvent) {
        info!("Window close: {} outcome={:?}", event.event_id, event.outcome);

        // Remove from tracked markets
        if let Some(market) = self.markets.remove(&event.event_id) {
            self.token_to_event.remove(&market.yes_token_id);
            self.token_to_event.remove(&market.no_token_id);
        }
    }

    /// Update time remaining on all markets.
    fn update_all_market_times(&mut self) {
        // Collect expired market IDs
        let expired: Vec<String> = self.markets
            .iter()
            .filter(|(_, m)| m.is_expired())
            .map(|(id, _)| id.clone())
            .collect();

        // Remove expired markets
        for id in expired {
            if let Some(market) = self.markets.remove(&id) {
                self.token_to_event.remove(&market.yes_token_id);
                self.token_to_event.remove(&market.no_token_id);
                debug!("Removed expired market: {}", id);
            }
        }

        // Update remaining markets
        for market in self.markets.values_mut() {
            market.update_time();
        }
    }

    /// Check all markets for arbitrage opportunities.
    async fn check_opportunities(&mut self, event_latency_us: u64) -> Result<(), StrategyError> {
        // Fast path: check if trading is enabled (single atomic load)
        if !self.state.can_trade() {
            trace!("Trading disabled, skipping opportunity check");
            return Ok(());
        }

        // Calculate total exposure once
        let total_exposure = self.state.market_data.total_exposure();

        // Check each market
        let event_ids: Vec<String> = self.markets.keys().cloned().collect();

        for event_id in event_ids {
            let market = match self.markets.get_mut(&event_id) {
                Some(m) => m,
                None => continue,
            };

            // Update time
            market.update_time();

            // Skip expired markets
            if market.is_expired() {
                continue;
            }

            // Fast filter: check if there's potential arb
            if !ArbDetector::has_potential_arb(&market.state) {
                continue;
            }

            // Detect opportunity
            let opportunity = match self.arb_detector.detect(&market.state) {
                Ok(opp) => opp,
                Err(reason) => {
                    trace!("No arb in {}: {}", event_id, reason);
                    continue;
                }
            };

            // Track opportunity detected
            self.state.metrics.inc_opportunities();

            // Use cached toxic flow warnings from recent book updates
            let yes_toxic = market.yes_toxic_warning.clone();
            let no_toxic = market.no_toxic_warning.clone();

            // Use the more severe warning
            let toxic_warning = match (&yes_toxic, &no_toxic) {
                (Some(y), Some(n)) => {
                    if y.severity >= n.severity { yes_toxic } else { no_toxic }
                }
                (Some(_), None) => yes_toxic,
                (None, Some(_)) => no_toxic,
                (None, None) => None,
            };

            // Check if toxic flow should block the trade
            if self.config.block_on_toxic_high
                && let Some(ref warning) = toxic_warning
                && warning.severity.should_block_trading()
            {
                debug!("Blocking trade due to toxic flow: {:?}", warning.severity);
                self.record_decision(
                    &opportunity,
                    SizingResult::invalid(SizingLimit::NoOpportunity),
                    toxic_warning.clone(),
                    TradeAction::SkipToxic,
                    event_latency_us,
                );
                self.state.metrics.inc_skipped();
                continue;
            }

            // Calculate position size
            let sizing = self.sizer.calculate_size(
                &opportunity,
                Some(&market.inventory),
                total_exposure,
                toxic_warning.as_ref(),
            );

            if !sizing.is_valid {
                debug!("Invalid sizing for {}: {:?}", event_id, sizing.limit_reason);
                self.record_decision(
                    &opportunity,
                    sizing,
                    toxic_warning.clone(),
                    TradeAction::SkipSizing,
                    event_latency_us,
                );
                self.state.metrics.inc_skipped();
                continue;
            }

            // Execute the trade
            info!("Executing arb: {} size={} margin={}bps",
                event_id, sizing.size, opportunity.margin_bps);

            let action = self.execute_arb(&opportunity, &sizing).await?;

            // Record decision
            self.record_decision(
                &opportunity,
                sizing,
                toxic_warning,
                action,
                event_latency_us,
            );
        }

        Ok(())
    }

    /// Execute an arbitrage trade (buy YES and NO).
    async fn execute_arb(
        &mut self,
        opportunity: &ArbOpportunity,
        sizing: &SizingResult,
    ) -> Result<TradeAction, StrategyError> {
        // Generate request IDs
        let req_id_base = self.decision_counter;
        self.decision_counter += 1;

        // Create YES order
        let yes_order = OrderRequest::limit(
            format!("arb-{}-yes", req_id_base),
            opportunity.event_id.clone(),
            opportunity.yes_token_id.clone(),
            Outcome::Yes,
            Side::Buy,
            sizing.size,
            opportunity.yes_ask,
        );

        // Create NO order
        let no_order = OrderRequest::limit(
            format!("arb-{}-no", req_id_base),
            opportunity.event_id.clone(),
            opportunity.no_token_id.clone(),
            Outcome::No,
            Side::Buy,
            sizing.size,
            opportunity.no_ask,
        );

        // Submit YES order
        let yes_result = self.executor.place_order(yes_order).await;
        match &yes_result {
            Ok(result) if result.is_filled() => {
                debug!("YES order filled: {} shares", result.filled_size());
            }
            Ok(result) => {
                debug!("YES order not filled: {:?}", result);
            }
            Err(e) => {
                warn!("YES order failed: {}", e);
                if self.state.record_failure(self.config.max_consecutive_failures) {
                    self.state.trip_circuit_breaker();
                    warn!("Circuit breaker tripped after consecutive failures");
                }
                return Ok(TradeAction::Execute);
            }
        }

        // Submit NO order
        let no_result = self.executor.place_order(no_order).await;
        match &no_result {
            Ok(result) if result.is_filled() => {
                debug!("NO order filled: {} shares", result.filled_size());
            }
            Ok(result) => {
                debug!("NO order not filled: {:?}", result);
            }
            Err(e) => {
                warn!("NO order failed: {}", e);
                if self.state.record_failure(self.config.max_consecutive_failures) {
                    self.state.trip_circuit_breaker();
                    warn!("Circuit breaker tripped after consecutive failures");
                }
            }
        }

        // Update volume metrics
        let volume_cents = (sizing.expected_cost * Decimal::new(100, 0))
            .try_into()
            .unwrap_or(0u64);
        self.state.metrics.add_volume_cents(volume_cents);

        Ok(TradeAction::Execute)
    }

    /// Record a decision for observability.
    fn record_decision(
        &mut self,
        opportunity: &ArbOpportunity,
        sizing: SizingResult,
        toxic_warning: Option<ToxicFlowWarning>,
        action: TradeAction,
        latency_us: u64,
    ) {
        let decision = TradeDecision {
            decision_id: self.decision_counter,
            event_id: opportunity.event_id.clone(),
            asset: opportunity.asset,
            opportunity: opportunity.clone(),
            sizing,
            toxic_warning,
            action,
            timestamp: Utc::now(),
            latency_us,
        };

        self.decision_counter += 1;

        // Fire-and-forget to observability channel
        if let Some(ref sender) = self.obs_sender {
            // Use try_send to avoid blocking the hot path
            if sender.try_send(decision).is_err() {
                trace!("Observability channel full, dropping decision");
            }
        }
    }

    /// Shutdown the strategy loop.
    pub async fn shutdown(&mut self) {
        info!("Shutting down strategy loop");
        self.data_source.shutdown().await;
        self.executor.shutdown().await;
    }

    /// Get current metrics snapshot.
    pub fn metrics(&self) -> crate::state::MetricsSnapshot {
        self.state.metrics.snapshot()
    }

    /// Get number of tracked markets.
    pub fn market_count(&self) -> usize {
        self.markets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_source::DataSourceError;
    use crate::executor::{
        ExecutorError, OrderCancellation, OrderFill, OrderResult, PendingOrder,
    };
    use crate::types::PriceLevel;
    use async_trait::async_trait;
    use rust_decimal_macros::dec;
    use std::collections::VecDeque;

    /// Mock data source for testing.
    struct MockDataSource {
        events: VecDeque<MarketEvent>,
        shutdown_called: bool,
    }

    impl MockDataSource {
        fn new(events: Vec<MarketEvent>) -> Self {
            Self {
                events: events.into(),
                shutdown_called: false,
            }
        }
    }

    #[async_trait]
    impl DataSource for MockDataSource {
        async fn next_event(&mut self) -> Result<Option<MarketEvent>, DataSourceError> {
            Ok(self.events.pop_front())
        }

        fn has_more(&self) -> bool {
            !self.events.is_empty()
        }

        fn current_time(&self) -> Option<DateTime<Utc>> {
            None
        }

        async fn shutdown(&mut self) {
            self.shutdown_called = true;
        }
    }

    /// Mock executor for testing.
    struct MockExecutor {
        orders: Vec<OrderRequest>,
        balance: Decimal,
    }

    impl MockExecutor {
        fn new(balance: Decimal) -> Self {
            Self {
                orders: Vec::new(),
                balance,
            }
        }
    }

    #[async_trait]
    impl Executor for MockExecutor {
        async fn place_order(&mut self, order: OrderRequest) -> Result<OrderResult, ExecutorError> {
            self.orders.push(order.clone());
            Ok(OrderResult::Filled(OrderFill {
                request_id: order.request_id,
                order_id: format!("mock-{}", self.orders.len()),
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

        async fn shutdown(&mut self) {}
    }

    fn create_window_open_event(event_id: &str, asset: CryptoAsset) -> MarketEvent {
        MarketEvent::WindowOpen(WindowOpenEvent {
            event_id: event_id.to_string(),
            asset,
            yes_token_id: format!("{}-yes", event_id),
            no_token_id: format!("{}-no", event_id),
            strike_price: dec!(100000),
            window_start: Utc::now(),
            window_end: Utc::now() + chrono::Duration::minutes(15),
            timestamp: Utc::now(),
        })
    }

    fn create_book_snapshot_event(token_id: &str, asks: Vec<(Decimal, Decimal)>) -> MarketEvent {
        MarketEvent::BookSnapshot(BookSnapshotEvent {
            token_id: token_id.to_string(),
            event_id: "".to_string(),
            bids: vec![PriceLevel::new(dec!(0.40), dec!(100))],
            asks: asks.into_iter().map(|(p, s)| PriceLevel::new(p, s)).collect(),
            timestamp: Utc::now(),
        })
    }

    #[tokio::test]
    async fn test_strategy_loop_creation() {
        let data_source = MockDataSource::new(vec![]);
        let executor = MockExecutor::new(dec!(1000));
        let state = Arc::new(GlobalState::new());
        let config = StrategyConfig::default();

        let strategy = StrategyLoop::new(data_source, executor, state, config);
        assert_eq!(strategy.market_count(), 0);
    }

    #[tokio::test]
    async fn test_window_open_tracking() {
        let events = vec![
            create_window_open_event("event1", CryptoAsset::Btc),
        ];

        let data_source = MockDataSource::new(events);
        let executor = MockExecutor::new(dec!(1000));
        let state = Arc::new(GlobalState::new());
        let config = StrategyConfig::default();

        let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);
        state.enable_trading();

        let _ = strategy.run().await;

        assert_eq!(strategy.market_count(), 1);
    }

    #[tokio::test]
    async fn test_spot_price_update() {
        let events = vec![
            MarketEvent::SpotPrice(SpotPriceEvent {
                asset: CryptoAsset::Btc,
                price: dec!(100500),
                quantity: dec!(1.5),
                timestamp: Utc::now(),
            }),
        ];

        let data_source = MockDataSource::new(events);
        let executor = MockExecutor::new(dec!(1000));
        let state = Arc::new(GlobalState::new());
        let config = StrategyConfig::default();

        let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);
        let _ = strategy.run().await;

        let (price, _) = state.market_data.get_spot_price("BTC").unwrap();
        assert_eq!(price, dec!(100500));
    }

    #[tokio::test]
    async fn test_arb_detection_and_execution() {
        // Setup: open window, then provide books with arb opportunity
        let events = vec![
            create_window_open_event("event1", CryptoAsset::Btc),
            // YES ask at 0.45 (100 shares)
            create_book_snapshot_event("event1-yes", vec![(dec!(0.45), dec!(100))]),
            // NO ask at 0.52 (100 shares) - combined = 0.97, margin = 3%
            create_book_snapshot_event("event1-no", vec![(dec!(0.52), dec!(100))]),
        ];

        let data_source = MockDataSource::new(events);
        let executor = MockExecutor::new(dec!(1000));
        let state = Arc::new(GlobalState::new());
        let config = StrategyConfig::default();

        let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);
        state.enable_trading();

        let _ = strategy.run().await;

        // Should have detected opportunity and executed
        assert!(state.metrics.opportunities_detected.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn test_trading_disabled_skips_opportunities() {
        let events = vec![
            create_window_open_event("event1", CryptoAsset::Btc),
            create_book_snapshot_event("event1-yes", vec![(dec!(0.45), dec!(100))]),
            create_book_snapshot_event("event1-no", vec![(dec!(0.52), dec!(100))]),
        ];

        let data_source = MockDataSource::new(events);
        let executor = MockExecutor::new(dec!(1000));
        let state = Arc::new(GlobalState::new());
        let config = StrategyConfig::default();

        let mut strategy = StrategyLoop::new(data_source, executor, state.clone(), config);
        // Trading NOT enabled

        let _ = strategy.run().await;

        // Opportunities should be detected but not executed (orders list empty)
        // Actually with trading disabled, we skip the opportunity check entirely
        assert_eq!(strategy.executor.orders.len(), 0);
    }

    #[tokio::test]
    async fn test_circuit_breaker_trips_on_failures() {
        let state = Arc::new(GlobalState::new());
        state.enable_trading();

        // Simulate consecutive failures
        assert!(!state.record_failure(3)); // 1
        assert!(!state.record_failure(3)); // 2
        assert!(state.record_failure(3));  // 3 - should trip

        // Now can_trade should return false
        state.trip_circuit_breaker();
        assert!(!state.can_trade());
    }

    #[test]
    fn test_trade_action_variants() {
        assert_eq!(TradeAction::Execute, TradeAction::Execute);
        assert_ne!(TradeAction::Execute, TradeAction::SkipSizing);
        assert_ne!(TradeAction::SkipToxic, TradeAction::SkipDisabled);
    }

    #[test]
    fn test_strategy_config_default() {
        let config = StrategyConfig::default();
        assert_eq!(config.max_consecutive_failures, 3);
        assert!(config.block_on_toxic_high);
    }

    #[test]
    fn test_trade_decision_serialization() {
        let opportunity = ArbOpportunity {
            event_id: "event1".to_string(),
            asset: CryptoAsset::Btc,
            yes_token_id: "yes".to_string(),
            no_token_id: "no".to_string(),
            yes_ask: dec!(0.45),
            no_ask: dec!(0.52),
            combined_cost: dec!(0.97),
            margin: dec!(0.03),
            margin_bps: 300,
            max_size: dec!(100),
            seconds_remaining: 300,
            phase: crate::state::WindowPhase::Early,
            required_threshold: dec!(0.025),
            confidence: 75,
            spot_price: Some(dec!(100500)),
            strike_price: dec!(100000),
            detected_at_ms: 0,
        };

        let sizing = SizingResult {
            size: dec!(50),
            is_valid: true,
            expected_cost: dec!(48.5),
            expected_profit: dec!(1.5),
            limit_reason: Some(SizingLimit::BaseSize),
            adjustments: SizingAdjustments::default(),
        };

        let decision = TradeDecision {
            decision_id: 1,
            event_id: "event1".to_string(),
            asset: CryptoAsset::Btc,
            opportunity,
            sizing,
            toxic_warning: None,
            action: TradeAction::Execute,
            timestamp: Utc::now(),
            latency_us: 500,
        };

        // Should serialize without panic
        let json = serde_json::to_string(&decision).unwrap();
        assert!(json.contains("event1"));
        assert!(json.contains("Execute"));
    }
}
