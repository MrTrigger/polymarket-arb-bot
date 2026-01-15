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

pub mod aggregator;
pub mod arb;
pub mod atr;
pub mod confidence;
pub mod confidence_sizing;
pub mod directional;
pub mod maker;
pub mod position;
pub mod pricing;
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

// ============================================================================
// Active Maker Order Tracking
// ============================================================================

/// Configuration for maker order management.
#[derive(Debug, Clone)]
pub struct MakerOrderConfig {
    /// Maximum age (ms) before an order is considered stale and should be refreshed.
    pub stale_threshold_ms: i64,
    /// Minimum price change (bps) to trigger order refresh.
    pub price_refresh_threshold_bps: u32,
    /// Maximum number of active maker orders per market.
    pub max_orders_per_market: usize,
}

impl Default for MakerOrderConfig {
    fn default() -> Self {
        Self {
            stale_threshold_ms: 5000,        // 5 seconds
            price_refresh_threshold_bps: 20, // 0.2% price change
            max_orders_per_market: 4,        // 2 per side (YES/NO)
        }
    }
}

/// An active maker order being tracked by the strategy.
#[derive(Debug, Clone)]
pub struct ActiveMakerOrder {
    /// Order ID assigned by the exchange.
    pub order_id: String,
    /// Token ID this order is for.
    pub token_id: String,
    /// Buy or Sell.
    pub side: Side,
    /// Order price.
    pub price: Decimal,
    /// Original order size.
    pub original_size: Decimal,
    /// Remaining unfilled size.
    pub remaining_size: Decimal,
    /// Filled size so far.
    pub filled_size: Decimal,
    /// Timestamp when order was placed (ms).
    pub placed_at_ms: i64,
    /// Timestamp of last update (ms).
    pub updated_at_ms: i64,
}

impl ActiveMakerOrder {
    /// Create a new active maker order.
    pub fn new(
        order_id: String,
        token_id: String,
        side: Side,
        price: Decimal,
        size: Decimal,
        now_ms: i64,
    ) -> Self {
        Self {
            order_id,
            token_id,
            side,
            price,
            original_size: size,
            remaining_size: size,
            filled_size: Decimal::ZERO,
            placed_at_ms: now_ms,
            updated_at_ms: now_ms,
        }
    }

    /// Check if this order is stale (older than threshold).
    #[inline]
    pub fn is_stale(&self, now_ms: i64, threshold_ms: i64) -> bool {
        now_ms - self.placed_at_ms > threshold_ms
    }

    /// Check if the price has moved significantly from the order price.
    ///
    /// Returns true if the optimal price differs from the order price
    /// by more than the threshold.
    pub fn needs_price_refresh(&self, optimal_price: Decimal, threshold_bps: u32) -> bool {
        if self.price.is_zero() {
            return false;
        }
        let diff = (optimal_price - self.price).abs();
        let diff_bps_decimal = diff / self.price * Decimal::from(10000u32);
        let diff_bps: u32 = diff_bps_decimal.try_into().unwrap_or(0);
        diff_bps > threshold_bps
    }

    /// Record a partial fill.
    pub fn record_fill(&mut self, filled_size: Decimal, now_ms: i64) {
        self.filled_size += filled_size;
        self.remaining_size = self.original_size - self.filled_size;
        self.updated_at_ms = now_ms;
    }

    /// Check if order is fully filled.
    #[inline]
    pub fn is_fully_filled(&self) -> bool {
        self.remaining_size <= Decimal::ZERO
    }
}

use crate::config::{EnginesConfig, TradingConfig};
use crate::data_source::{
    BookDeltaEvent, BookSnapshotEvent, DataSource, DataSourceError, FillEvent, MarketEvent,
    SpotPriceEvent, WindowCloseEvent, WindowOpenEvent,
};
use crate::executor::{Executor, ExecutorError, OrderRequest, OrderResult, OrderType};
use crate::state::GlobalState;
use crate::types::{EngineType, Inventory, MarketState, OrderBook};

pub use arb::{ArbDetector, ArbOpportunity, ArbRejection, ArbThresholds};
pub use confidence::{
    Confidence, ConfidenceCalculator, ConfidenceFactors, MAX_MULTIPLIER, MIN_MULTIPLIER,
};
pub use confidence_sizing::{ConfidenceSizer, OrderSizeResult, SizeRejection};
pub use signal::{
    calculate_distance, distance_bps, get_signal, get_thresholds, Signal, SignalThresholds,
};
pub use sizing::{
    create_sizer, HybridSizer, PositionSizer, SizingAdjustments, SizingConfig, SizingInput,
    SizingLimit, SizingMode, SizingResult, SizingStrategy, UnifiedSizingResult,
};
pub use toxic::{
    ToxicFlowConfig, ToxicFlowDetector, ToxicFlowWarning, ToxicIndicators, ToxicSeverity,
};
pub use directional::{
    DirectionalConfig, DirectionalDetector, DirectionalOpportunity, DirectionalSkipReason,
};
pub use maker::{
    MakerConfig, MakerDetector, MakerOpportunity, MakerSkipReason,
};
pub use aggregator::{
    AggregatedDecision, DecisionAggregator, DecisionSummary, EngineDecision,
};

// ============================================================================
// Maker Price Calculation
// ============================================================================

use rust_decimal_macros::dec;

/// Calculate optimal maker price inside the spread.
///
/// The maker price is positioned inside the bid-ask spread based on spread width:
/// - Wide spread (>3%): Place 40% from bid toward ask
/// - Medium spread (1-3%): Place 30% from bid toward ask
/// - Tight spread (<1%): Place at bid price
///
/// This ensures we're offering a better price than existing bids (for buys)
/// while still capturing maker rebates.
fn calculate_maker_price(book: &OrderBook, side: Side) -> Option<Decimal> {
    let bid = book.best_bid()?;
    let ask = book.best_ask()?;
    let spread = ask - bid;
    let mid = (bid + ask) / Decimal::TWO;

    if mid <= Decimal::ZERO {
        return None;
    }

    // Spread as a ratio of mid price
    let spread_ratio = spread / mid;

    // Determine placement based on spread width
    let placement_ratio = if spread_ratio > dec!(0.03) {
        // Wide spread (>3%): aggressive, 40% from bid
        dec!(0.40)
    } else if spread_ratio > dec!(0.01) {
        // Medium spread (1-3%): moderate, 30% from bid
        dec!(0.30)
    } else {
        // Tight spread (<1%): conservative, at bid
        dec!(0.0)
    };

    match side {
        Side::Buy => {
            // For buying, we place above bid but below ask
            let price = bid + (spread * placement_ratio);
            // Round to 2 decimal places (Polymarket uses 0.01 ticks)
            Some(price.round_dp(2))
        }
        Side::Sell => {
            // For selling, we place below ask but above bid
            let price = ask - (spread * placement_ratio);
            // Round to 2 decimal places
            Some(price.round_dp(2))
        }
    }
}

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

/// Extended trade decision with engine source for multi-engine strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiEngineDecision {
    /// Unique decision ID.
    pub decision_id: u64,
    /// Event ID for this market.
    pub event_id: String,
    /// Asset being traded.
    pub asset: CryptoAsset,
    /// Which engine produced this decision.
    pub engine: EngineType,
    /// The aggregated decision (primary + concurrent).
    pub aggregated: AggregatedDecision,
    /// Sizing result (if applicable).
    pub sizing: Option<SizingResult>,
    /// Toxic flow warning (if any).
    pub toxic_warning: Option<ToxicFlowWarning>,
    /// Action taken.
    pub action: TradeAction,
    /// Decision timestamp.
    pub timestamp: DateTime<Utc>,
    /// Latency from event to decision (microseconds).
    pub latency_us: u64,
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
    /// Active maker orders for this market (keyed by order_id).
    active_maker_orders: HashMap<String, ActiveMakerOrder>,
    /// Phase-based position manager for this market.
    /// Controls when/how much to trade based on confidence and budget.
    position_manager: position::PositionManager,
}

impl TrackedMarket {
    fn new(event: &WindowOpenEvent, market_budget: Decimal) -> Self {
        let state = MarketState::new(
            event.event_id.clone(),
            event.asset,
            event.yes_token_id.clone(),
            event.no_token_id.clone(),
            event.strike_price,
            (event.window_end - Utc::now()).num_seconds(),
        );
        // Use asset-specific ATR for proper volatility normalization
        let atr = event.asset.estimated_atr_15m();
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
            active_maker_orders: HashMap::new(),
            position_manager: position::PositionManager::with_budget_and_atr(market_budget, atr),
        }
    }

    /// Check if we should trade using phase-based position management.
    ///
    /// This implements the spec's intelligent capital allocation:
    /// 1. **Reserve capital for later phases** when signals are clearer
    /// 2. **Gate trades by confidence threshold** - stricter early, looser late
    /// 3. **Scale position size by confidence** - bigger when more certain
    /// 4. **Enforce position limits** - never go all-in on one side
    fn should_trade(&self, distance_dollars: Decimal, seconds_remaining: i64) -> position::TradeDecision {
        self.position_manager.should_trade(distance_dollars, seconds_remaining)
    }

    /// Record a completed trade in the position manager.
    fn record_trade(&mut self, size: Decimal, up_amount: Decimal, down_amount: Decimal, seconds_remaining: i64) {
        self.position_manager.record_trade(size, up_amount, down_amount, seconds_remaining);
    }

    /// Check if we have any position in this market.
    fn has_position(&self) -> bool {
        self.inventory.total_exposure() > Decimal::ZERO
    }

    /// Get the position manager's budget remaining.
    fn budget_remaining(&self) -> Decimal {
        self.position_manager.budget_remaining()
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
/// 3. Runs all enabled engines (arbitrage, directional, maker)
/// 4. Aggregates decisions based on priority
/// 5. Checks for toxic flow
/// 6. Calculates position size
/// 7. Submits orders via `Executor`
/// 8. Sends decisions to observability channel with engine source
pub struct StrategyLoop<D: DataSource, E: Executor> {
    /// Data source for market events.
    data_source: D,
    /// Executor for order submission.
    executor: E,
    /// Global shared state.
    state: Arc<GlobalState>,
    /// Strategy configuration.
    config: StrategyConfig,
    /// Engine configuration (which engines are enabled and priority).
    engines_config: EnginesConfig,
    /// Arbitrage detector.
    arb_detector: ArbDetector,
    /// Directional detector.
    directional_detector: DirectionalDetector,
    /// Maker rebate detector.
    maker_detector: MakerDetector,
    /// Decision aggregator for multi-engine strategy.
    aggregator: DecisionAggregator,
    /// Toxic flow detector.
    toxic_detector: ToxicFlowDetector,
    /// Position sizer (limit-based).
    sizer: PositionSizer,
    /// Confidence-based sizer for directional trades.
    confidence_sizer: ConfidenceSizer,
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
    /// Multi-engine observability channel sender (optional).
    multi_obs_sender: Option<mpsc::Sender<MultiEngineDecision>>,
    /// Last status log time for periodic logging.
    last_status_log: std::time::Instant,
    /// Dynamic ATR tracker for volatility-adjusted position sizing.
    atr_tracker: atr::AtrTracker,
}

impl<D: DataSource, E: Executor> StrategyLoop<D, E> {
    /// Create a new strategy loop.
    ///
    /// Uses default engines config (arbitrage only, for backward compatibility).
    pub fn new(
        data_source: D,
        executor: E,
        state: Arc<GlobalState>,
        config: StrategyConfig,
    ) -> Self {
        Self::with_engines(data_source, executor, state, config, EnginesConfig::default())
    }

    /// Create a new strategy loop with custom engine configuration.
    pub fn with_engines(
        data_source: D,
        executor: E,
        state: Arc<GlobalState>,
        config: StrategyConfig,
        engines_config: EnginesConfig,
    ) -> Self {
        let arb_detector = ArbDetector::new(config.arb_thresholds.clone());
        let directional_detector = DirectionalDetector::new();
        let maker_detector = MakerDetector::new();
        let aggregator = DecisionAggregator::new(&engines_config);
        let toxic_detector = ToxicFlowDetector::new(config.toxic_config.clone());
        let sizer = PositionSizer::new(config.sizing_config.clone());
        // Create confidence sizer with the available balance from sizing config
        let confidence_sizer = ConfidenceSizer::with_balance(config.sizing_config.base_order_size * Decimal::from(200u32));

        Self {
            data_source,
            executor,
            state,
            config,
            engines_config,
            arb_detector,
            directional_detector,
            maker_detector,
            aggregator,
            toxic_detector,
            sizer,
            confidence_sizer,
            markets: HashMap::new(),
            token_to_event: HashMap::new(),
            spot_prices: HashMap::new(),
            decision_counter: 0,
            obs_sender: None,
            multi_obs_sender: None,
            last_status_log: std::time::Instant::now(),
            atr_tracker: atr::AtrTracker::default(),
        }
    }

    /// Set observability channel for decision capture (legacy arb-only).
    ///
    /// Decisions are sent via `try_send()` to avoid blocking the hot path.
    pub fn with_observability(mut self, sender: mpsc::Sender<TradeDecision>) -> Self {
        self.obs_sender = Some(sender);
        self
    }

    /// Set observability channel for multi-engine decision capture.
    ///
    /// Decisions include engine source for multi-engine strategy analysis.
    pub fn with_multi_observability(mut self, sender: mpsc::Sender<MultiEngineDecision>) -> Self {
        self.multi_obs_sender = Some(sender);
        self
    }

    /// Get the current engines configuration.
    pub fn engines_config(&self) -> &EnginesConfig {
        &self.engines_config
    }

    /// Check if a specific engine is enabled.
    pub fn is_engine_enabled(&self, engine: EngineType) -> bool {
        match engine {
            EngineType::Arbitrage => self.engines_config.arbitrage.enabled,
            EngineType::Directional => self.engines_config.directional.enabled,
            EngineType::MakerRebates => self.engines_config.maker.enabled,
        }
    }

    /// Warm up the ATR tracker with historical prices.
    ///
    /// Call this before running the strategy loop to ensure accurate ATR
    /// from the first trade. Pass prices from the last 5-10 minutes.
    ///
    /// # Arguments
    /// * `asset` - The asset to warm up
    /// * `prices` - Historical prices in chronological order (oldest first)
    pub fn warmup_atr(&mut self, asset: CryptoAsset, prices: &[Decimal]) {
        if prices.is_empty() {
            return;
        }
        info!(
            "Warming up ATR for {} with {} historical prices",
            asset,
            prices.len()
        );
        self.atr_tracker.warmup(asset, prices);
        let atr = self.atr_tracker.get_atr(asset);
        info!("ATR for {} warmed up to ${:.2}", asset, atr);
    }

    /// Check if ATR is warmed up for all tracked assets.
    pub fn is_atr_warmed_up(&self) -> bool {
        self.atr_tracker.is_warmed_up_all()
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

        // Record price for dynamic ATR calculation
        self.atr_tracker.record_price(event.asset, event.price);

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

                // For Up/Down markets, if strike is still 0, set it from first spot price
                if market.state.strike_price.is_zero() {
                    info!(
                        "Setting strike price for Up/Down market {} from spot={}",
                        market.event_id, event.price
                    );
                    market.state.strike_price = event.price;
                }
            }
        }
    }

    /// Log periodic market status for debugging.
    fn log_market_status(&self) {
        // Find the closest market to expiry
        let mut closest_market: Option<(&String, &TrackedMarket)> = None;
        for (event_id, market) in &self.markets {
            if market.is_expired() {
                continue;
            }
            match &closest_market {
                None => closest_market = Some((event_id, market)),
                Some((_, existing)) => {
                    if market.state.seconds_remaining < existing.state.seconds_remaining {
                        closest_market = Some((event_id, market));
                    }
                }
            }
        }

        if let Some((event_id, market)) = closest_market {
            let state = &market.state;
            let mins = Decimal::from(state.seconds_remaining) / dec!(60);
            let thresholds = get_thresholds(mins);

            if let Some(spot) = state.spot_price {
                let distance = if !state.strike_price.is_zero() {
                    (spot - state.strike_price) / state.strike_price * dec!(100)
                } else {
                    Decimal::ZERO
                };
                let has_book = state.yes_book.best_ask().is_some() && state.no_book.best_ask().is_some();
                let dynamic_atr = self.atr_tracker.get_atr(market.asset);

                info!(
                    "📊 Status {} {}: spot={} strike={} dist={:.4}% time={}min atr=${:.2} thresholds=[lean={:.4}% strong={:.4}%] book={}",
                    event_id,
                    market.asset,
                    spot,
                    state.strike_price,
                    distance,
                    mins.round(),
                    dynamic_atr,
                    thresholds.lean * dec!(100),
                    thresholds.strong * dec!(100),
                    if has_book { "ready" } else { "waiting" }
                );
            } else {
                info!(
                    "📊 Status {} {}: NO SPOT PRICE strike={} time={}min",
                    event_id, market.asset, state.strike_price, mins.round()
                );
            }
        } else {
            info!("📊 Status: No active markets");
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
    fn handle_window_open(&mut self, mut event: WindowOpenEvent) {
        // For "Up or Down" relative markets, strike_price is 0 from discovery.
        // Set strike from current spot price (the opening price).
        if event.strike_price.is_zero() {
            if let Some(&spot) = self.spot_prices.get(&event.asset) {
                info!(
                    "Window open: {} {} (Up/Down market, setting strike from spot={})",
                    event.event_id, event.asset, spot
                );
                event.strike_price = spot;
            } else {
                warn!(
                    "Window open: {} {} has no strike price and no spot price available. \
                     Market will not trade until spot price is received.",
                    event.event_id, event.asset
                );
            }
        } else {
            info!("Window open: {} {} strike={}",
                event.event_id, event.asset, event.strike_price);
        }

        // Calculate budget per market from sizing config
        let market_budget = self.config.sizing_config.max_position_per_market;

        // Create tracked market with phase-based position management
        let market = TrackedMarket::new(&event, market_budget);

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

    /// Check all markets for opportunities using all enabled engines.
    ///
    /// This method runs all enabled engines (arbitrage, directional, maker) and
    /// aggregates their decisions based on the configured priority order.
    /// Maker orders can run concurrently with other strategies since they are passive.
    async fn check_opportunities(&mut self, event_latency_us: u64) -> Result<(), StrategyError> {
        // Fast path: check if trading is enabled (single atomic load)
        if !self.state.can_trade() {
            trace!("Trading disabled, skipping opportunity check");
            return Ok(());
        }

        // Periodic status log (every 15 seconds)
        if self.last_status_log.elapsed() > std::time::Duration::from_secs(15) {
            self.last_status_log = std::time::Instant::now();
            self.log_market_status();
        }

        // Calculate total exposure once
        let total_exposure = self.state.market_data.total_exposure();

        // Check each market
        let event_ids: Vec<String> = self.markets.keys().cloned().collect();

        for event_id in event_ids {
            // === Phase 1: Gather market state ===
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

            // Update spot price in market state if we have it
            if let Some(&spot) = self.spot_prices.get(&market.state.asset) {
                market.state.spot_price = Some(spot);
            }

            let market_state = &market.state;
            let asset = market.asset;

            // === Phase 2: Run all enabled engines ===

            // Arbitrage detection
            let arb_opportunity = if self.engines_config.arbitrage.enabled {
                match self.arb_detector.detect(market_state) {
                    Ok(opp) => {
                        trace!("Arb detected for {}: margin={}bps", event_id, opp.margin_bps);
                        Some(opp)
                    }
                    Err(reason) => {
                        trace!("No arb in {}: {}", event_id, reason);
                        None
                    }
                }
            } else {
                None
            };

            // Directional detection
            let directional_opportunity = if self.engines_config.directional.enabled {
                match self.directional_detector.detect(market_state) {
                    Ok(opp) => {
                        // Only log at debug level to avoid spam - meaningful logging happens later
                        trace!(
                            "Directional {} {}: signal={:?} dist={:.4}%",
                            event_id, asset, opp.signal, opp.distance * dec!(100)
                        );
                        Some(opp)
                    }
                    Err(reason) => {
                        trace!("No directional in {}: {}", event_id, reason);
                        None
                    }
                }
            } else {
                None
            };

            // Maker detection
            let maker_opportunities = if self.engines_config.maker.enabled {
                let opps = self.maker_detector.detect(market_state, None, None);
                if !opps.is_empty() {
                    trace!("Maker detected for {}: {} opportunities", event_id, opps.len());
                }
                opps
            } else {
                Vec::new()
            };

            // === Phase 3: Aggregate decisions ===
            let aggregated = self.aggregator.aggregate(
                arb_opportunity.clone(),
                directional_opportunity.clone(),
                maker_opportunities.clone(),
            );

            // Skip if no opportunity detected by any engine
            if !aggregated.should_act() {
                continue;
            }

            // Track opportunity detected
            self.state.metrics.inc_opportunities();

            // === Phase 4: Toxic flow check ===
            let yes_toxic = market.yes_toxic_warning.clone();
            let no_toxic = market.no_toxic_warning.clone();
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
                // Record skipped decision for observability
                if let Some(primary_engine) = aggregated.primary_engine() {
                    self.record_multi_decision(
                        &event_id,
                        asset,
                        primary_engine,
                        aggregated.clone(),
                        None,
                        toxic_warning.clone(),
                        TradeAction::SkipToxic,
                        event_latency_us,
                    );
                }
                self.state.metrics.inc_skipped();
                continue;
            }

            // === Phase 5: Execute based on winning engine ===
            let action = match &aggregated.primary {
                Some(EngineDecision::Arbitrage(opp)) => {
                    // Calculate position size for arb
                    let market = self.markets.get(&event_id).unwrap();

                    // For arb: check if we already have a position.
                    // Arb locks in profit regardless of outcome, so we shouldn't
                    // keep adding once we're already positioned.
                    if market.has_position() {
                        trace!(
                            "Already have arb position for {}, skipping",
                            event_id
                        );
                        continue; // Silent skip - already have position
                    }

                    let sizing = self.sizer.calculate_size(
                        opp,
                        Some(&market.inventory),
                        total_exposure,
                        toxic_warning.as_ref(),
                    );

                    if !sizing.is_valid {
                        debug!("Invalid sizing for arb {}: {:?}", event_id, sizing.limit_reason);
                        self.record_multi_decision(
                            &event_id,
                            asset,
                            EngineType::Arbitrage,
                            aggregated.clone(),
                            Some(sizing),
                            toxic_warning.clone(),
                            TradeAction::SkipSizing,
                            event_latency_us,
                        );
                        self.state.metrics.inc_skipped();
                        continue;
                    }

                    // Execute arbitrage trade
                    info!("Executing arb: {} size={} margin={}bps engine=Arbitrage",
                        event_id, sizing.size, opp.margin_bps);

                    let action = self.execute_arb(opp, &sizing).await?;

                    // Record decision with engine source
                    self.record_multi_decision(
                        &event_id,
                        asset,
                        EngineType::Arbitrage,
                        aggregated.clone(),
                        Some(sizing),
                        toxic_warning.clone(),
                        action,
                        event_latency_us,
                    );

                    // Also record legacy decision for backward compatibility
                    if let Some(arb_opp) = &arb_opportunity {
                        let sizing_for_legacy = self.sizer.calculate_size(
                            arb_opp,
                            self.markets.get(&event_id).map(|m| &m.inventory),
                            total_exposure,
                            toxic_warning.as_ref(),
                        );
                        self.record_decision(arb_opp, sizing_for_legacy, toxic_warning.clone(), action, event_latency_us);
                    }

                    action
                }
                Some(EngineDecision::Directional(opp)) => {
                    // Update position manager with dynamic ATR before making decision
                    let dynamic_atr = self.atr_tracker.get_atr(asset);
                    if let Some(market) = self.markets.get_mut(&event_id) {
                        market.position_manager.set_atr(dynamic_atr);
                    }

                    // Use phase-based position management for trade decisions
                    let market = self.markets.get(&event_id).unwrap();

                    // Calculate distance in dollars for position manager
                    let distance_dollars = opp.distance * market.strike_price;
                    let seconds_remaining = market.state.seconds_remaining;

                    // Debug: log confidence calculation for position manager
                    let mins = Decimal::from(seconds_remaining) / dec!(60);
                    let phase = position::Phase::from_seconds(seconds_remaining);
                    let pm_conf = market.position_manager.calculate_confidence(distance_dollars, mins);
                    let atr = market.position_manager.atr();
                    let atr_mult = if atr > Decimal::ZERO { distance_dollars.abs() / atr } else { Decimal::ZERO };

                    // Check if we should trade based on phase, confidence, and budget
                    let trade_decision = market.should_trade(distance_dollars, seconds_remaining);

                    let (total_size, pm_confidence, phase) = match trade_decision {
                        position::TradeDecision::Skip(reason) => {
                            // Log skips with confidence details (debug level for low confidence to reduce spam)
                            if matches!(reason, position::SkipReason::LowConfidence) {
                                debug!(
                                    "⏭️ {} {} LowConf: signal={:?} dist=${:.2} atr_mult={:.2} conf={:.2} need={:.2} phase={}",
                                    event_id, asset, opp.signal, distance_dollars, atr_mult, pm_conf, phase.min_confidence(), phase
                                );
                            } else {
                                info!(
                                    "⏭️ Position manager skip {} {}: {:?} (signal={:?} dist={:.4}%)",
                                    event_id, asset, reason, opp.signal, opp.distance * dec!(100)
                                );
                            }
                            // Only record decision for meaningful skips, not low confidence
                            if !matches!(reason, position::SkipReason::LowConfidence) {
                                self.record_multi_decision(
                                    &event_id,
                                    asset,
                                    EngineType::Directional,
                                    aggregated.clone(),
                                    None,
                                    toxic_warning.clone(),
                                    TradeAction::SkipSizing,
                                    event_latency_us,
                                );
                                self.state.metrics.inc_skipped();
                            }
                            continue;
                        }
                        position::TradeDecision::Trade { size, confidence, phase } => {
                            (size, confidence, phase)
                        }
                    };

                    // Verify size is meaningful
                    if total_size < Decimal::ONE {
                        debug!("Directional size too small for {}: {}", event_id, total_size);
                        self.record_multi_decision(
                            &event_id,
                            asset,
                            EngineType::Directional,
                            aggregated.clone(),
                            None,
                            toxic_warning.clone(),
                            TradeAction::SkipSizing,
                            event_latency_us,
                        );
                        self.state.metrics.inc_skipped();
                        continue;
                    }

                    // Execute directional trade
                    info!("Executing directional: {} signal={:?} size={:.2} phase={} conf={:.2} engine=Directional",
                        event_id, opp.signal, total_size, phase, pm_confidence);

                    let action = self.execute_directional(opp, total_size).await?;

                    // Record decision with engine source
                    self.record_multi_decision(
                        &event_id,
                        asset,
                        EngineType::Directional,
                        aggregated.clone(),
                        None, // Directional uses confidence sizing, not SizingResult
                        toxic_warning.clone(),
                        action,
                        event_latency_us,
                    );

                    action
                }
                Some(EngineDecision::Maker(opps)) => {
                    // Execute maker orders
                    let mut action = TradeAction::Execute;
                    for opp in opps {
                        info!("Executing maker: {} token={} side={:?} price={} engine=MakerRebates",
                            event_id, opp.token_id, opp.side, opp.price);

                        match self.execute_maker(opp).await {
                            Ok(a) => action = a,
                            Err(e) => {
                                warn!("Maker execution error: {}", e);
                                action = TradeAction::SkipSizing;
                            }
                        }
                    }

                    // Record decision with engine source
                    self.record_multi_decision(
                        &event_id,
                        asset,
                        EngineType::MakerRebates,
                        aggregated.clone(),
                        None,
                        toxic_warning.clone(),
                        action,
                        event_latency_us,
                    );

                    action
                }
                None => continue,
            };

            // === Phase 6: Execute concurrent maker orders ===
            if let Some(concurrent_makers) = &aggregated.concurrent_maker
                && action == TradeAction::Execute
            {
                for opp in concurrent_makers {
                    trace!("Executing concurrent maker: {} token={}", event_id, opp.token_id);
                    // Fire and forget - don't block on concurrent makers
                    if let Err(e) = self.execute_maker(opp).await {
                        trace!("Concurrent maker failed (non-blocking): {}", e);
                    }
                }
            }
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

        // Track fills for inventory update
        let mut yes_filled_size = Decimal::ZERO;
        let mut yes_filled_cost = Decimal::ZERO;
        let mut no_filled_size = Decimal::ZERO;
        let mut no_filled_cost = Decimal::ZERO;

        // Submit YES order
        let yes_result = self.executor.place_order(yes_order).await;
        match &yes_result {
            Ok(result) if result.is_filled() => {
                yes_filled_size = result.filled_size();
                yes_filled_cost = result.filled_cost();
                debug!("YES order filled: {} shares, cost={}", yes_filled_size, yes_filled_cost);
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
                no_filled_size = result.filled_size();
                no_filled_cost = result.filled_cost();
                debug!("NO order filled: {} shares, cost={}", no_filled_size, no_filled_cost);
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

        // CRITICAL: Update inventory immediately after fills to prevent over-trading.
        // This ensures subsequent orderbook updates see the correct position.
        if yes_filled_size > Decimal::ZERO || no_filled_size > Decimal::ZERO {
            if let Some(market) = self.markets.get_mut(&opportunity.event_id) {
                if yes_filled_size > Decimal::ZERO {
                    market.inventory.record_fill(Outcome::Yes, yes_filled_size, yes_filled_cost);
                }
                if no_filled_size > Decimal::ZERO {
                    market.inventory.record_fill(Outcome::No, no_filled_size, no_filled_cost);
                }

                // Update global state inventory for sizing calculations
                let pos = crate::state::InventoryPosition {
                    event_id: opportunity.event_id.clone(),
                    yes_shares: market.inventory.yes_shares,
                    no_shares: market.inventory.no_shares,
                    yes_cost_basis: market.inventory.yes_cost_basis,
                    no_cost_basis: market.inventory.no_cost_basis,
                    realized_pnl: market.inventory.realized_pnl,
                };
                self.state.market_data.update_inventory(&opportunity.event_id, pos);

                debug!(
                    "Inventory updated for {}: YES={} NO={} total_exposure={}",
                    opportunity.event_id,
                    market.inventory.yes_shares,
                    market.inventory.no_shares,
                    market.inventory.total_exposure()
                );
            }
        }

        // Update volume metrics
        let volume_cents = (sizing.expected_cost * Decimal::new(100, 0))
            .try_into()
            .unwrap_or(0u64);
        self.state.metrics.add_volume_cents(volume_cents);

        Ok(TradeAction::Execute)
    }

    /// Execute a directional trade based on signal.
    ///
    /// Places maker orders (GTC/post-only) to capture the directional signal
    /// while earning maker rebates. The UP/DOWN allocation follows the signal
    /// ratios from the directional opportunity.
    ///
    /// # Arguments
    ///
    /// * `opportunity` - The detected directional opportunity with signal ratios
    /// * `total_size` - Total USDC to deploy across UP and DOWN
    ///
    /// # Returns
    ///
    /// The trade action taken (Execute, SkipSizing, etc.)
    async fn execute_directional(
        &mut self,
        opportunity: &DirectionalOpportunity,
        total_size: Decimal,
    ) -> Result<TradeAction, StrategyError> {
        let event_id = opportunity.event_id.clone();

        // Get the tracked market for order book access
        let market = match self.markets.get(&event_id) {
            Some(m) => m,
            None => {
                warn!("No tracked market for directional: {}", event_id);
                return Ok(TradeAction::SkipSizing);
            }
        };

        // Calculate UP and DOWN sizes based on signal ratios
        let up_size = total_size * opportunity.up_ratio;
        let down_size = total_size * opportunity.down_ratio;

        // Get order books
        let (yes_book, no_book) = (&market.state.yes_book, &market.state.no_book);

        // Calculate maker prices (inside the spread)
        let up_price = match calculate_maker_price(yes_book, Side::Buy) {
            Some(p) => p,
            None => {
                debug!("Cannot calculate UP maker price for {}", event_id);
                return Ok(TradeAction::SkipSizing);
            }
        };

        let down_price = match calculate_maker_price(no_book, Side::Buy) {
            Some(p) => p,
            None => {
                debug!("Cannot calculate DOWN maker price for {}", event_id);
                return Ok(TradeAction::SkipSizing);
            }
        };

        // Extract token IDs before we need to borrow self.markets mutably
        let yes_token_id = market.yes_token_id.clone();
        let no_token_id = market.no_token_id.clone();

        info!(
            "Directional trade: {} signal={:?} distance={:.4}% conf={:.2}x up_size={} down_size={} up_price={} down_price={}",
            event_id,
            opportunity.signal,
            opportunity.distance * Decimal::ONE_HUNDRED,
            opportunity.confidence.total_multiplier(),
            up_size,
            down_size,
            up_price,
            down_price
        );

        // Generate request IDs
        let req_id_base = self.decision_counter;
        self.decision_counter += 1;

        // Track total volume for metrics and fills for inventory
        let mut total_volume = Decimal::ZERO;
        let mut up_filled_size = Decimal::ZERO;
        let mut up_filled_cost = Decimal::ZERO;
        let mut down_filled_size = Decimal::ZERO;
        let mut down_filled_cost = Decimal::ZERO;

        // Place UP (YES) order if size is significant
        if up_size >= Decimal::ONE {
            let up_order = OrderRequest {
                request_id: format!("dir-{}-up", req_id_base),
                event_id: event_id.clone(),
                token_id: yes_token_id.clone(),
                outcome: Outcome::Yes,
                side: Side::Buy,
                size: up_size,
                price: Some(up_price),
                order_type: OrderType::Gtc, // Good-till-cancelled for maker
                timeout_ms: None,
                timestamp: Utc::now(),
            };

            let up_result = self.executor.place_order(up_order).await;
            match up_result {
                Ok(result) if result.is_filled() => {
                    up_filled_size = result.filled_size();
                    up_filled_cost = result.filled_cost();
                    total_volume += up_filled_cost;
                    debug!("UP order filled: {} shares, cost={}", up_filled_size, up_filled_cost);
                }
                Ok(result) => {
                    debug!("UP order not filled: {:?}", result);
                }
                Err(e) => {
                    warn!("UP order failed: {}", e);
                    if self.state.record_failure(self.config.max_consecutive_failures) {
                        self.state.trip_circuit_breaker();
                        warn!("Circuit breaker tripped after consecutive failures");
                    }
                }
            }
        }

        // Place DOWN (NO) order if size is significant
        if down_size >= Decimal::ONE {
            let down_order = OrderRequest {
                request_id: format!("dir-{}-down", req_id_base),
                event_id: event_id.clone(),
                token_id: no_token_id.clone(),
                outcome: Outcome::No,
                side: Side::Buy,
                size: down_size,
                price: Some(down_price),
                order_type: OrderType::Gtc, // Good-till-cancelled for maker
                timeout_ms: None,
                timestamp: Utc::now(),
            };

            let down_result = self.executor.place_order(down_order).await;
            match down_result {
                Ok(result) if result.is_filled() => {
                    down_filled_size = result.filled_size();
                    down_filled_cost = result.filled_cost();
                    total_volume += down_filled_cost;
                    debug!("DOWN order filled: {} shares, cost={}", down_filled_size, down_filled_cost);
                }
                Ok(result) => {
                    debug!("DOWN order not filled: {:?}", result);
                }
                Err(e) => {
                    warn!("DOWN order failed: {}", e);
                    if self.state.record_failure(self.config.max_consecutive_failures) {
                        self.state.trip_circuit_breaker();
                        warn!("Circuit breaker tripped after consecutive failures");
                    }
                }
            }
        }

        // CRITICAL: Update inventory immediately after fills to prevent over-trading.
        // This ensures subsequent orderbook updates see the correct position.
        if up_filled_size > Decimal::ZERO || down_filled_size > Decimal::ZERO {
            if let Some(market) = self.markets.get_mut(&event_id) {
                if up_filled_size > Decimal::ZERO {
                    market.inventory.record_fill(Outcome::Yes, up_filled_size, up_filled_cost);
                }
                if down_filled_size > Decimal::ZERO {
                    market.inventory.record_fill(Outcome::No, down_filled_size, down_filled_cost);
                }

                // Update global state inventory for sizing calculations
                let pos = crate::state::InventoryPosition {
                    event_id: event_id.clone(),
                    yes_shares: market.inventory.yes_shares,
                    no_shares: market.inventory.no_shares,
                    yes_cost_basis: market.inventory.yes_cost_basis,
                    no_cost_basis: market.inventory.no_cost_basis,
                    realized_pnl: market.inventory.realized_pnl,
                };
                self.state.market_data.update_inventory(&event_id, pos);

                // Record trade in position manager to update phase budgets
                let seconds_remaining = market.state.seconds_remaining;
                market.record_trade(total_volume, up_filled_cost, down_filled_cost, seconds_remaining);

                debug!(
                    "Inventory updated for {}: YES={} NO={} total_exposure={}",
                    event_id,
                    market.inventory.yes_shares,
                    market.inventory.no_shares,
                    market.inventory.total_exposure()
                );
            }
        }

        // Record the trade in confidence sizer
        if total_volume > Decimal::ZERO {
            self.confidence_sizer.record_trade(total_volume);

            // Update volume metrics
            let volume_cents = (total_volume * Decimal::new(100, 0))
                .try_into()
                .unwrap_or(0u64);
            self.state.metrics.add_volume_cents(volume_cents);
        }

        Ok(TradeAction::Execute)
    }

    /// Execute maker strategy for passive rebate capture.
    ///
    /// Places GTC (Good-Till-Cancelled) orders at optimal maker prices inside
    /// the spread. These orders provide liquidity and earn rebates when filled.
    ///
    /// This method:
    /// 1. Checks for existing active orders that need refresh or cancellation
    /// 2. Cancels stale orders or orders with prices too far from optimal
    /// 3. Places new maker orders at the current optimal price
    ///
    /// # Arguments
    ///
    /// * `opportunity` - The detected maker opportunity with price and size
    ///
    /// # Returns
    ///
    /// The trade action taken (Execute, SkipSizing, etc.)
    #[allow(dead_code)] // Used in phase 7 when engines are integrated
    async fn execute_maker(
        &mut self,
        opportunity: &MakerOpportunity,
    ) -> Result<TradeAction, StrategyError> {
        let event_id = &opportunity.event_id;
        let token_id = &opportunity.token_id;
        let now_ms = chrono::Utc::now().timestamp_millis();

        // Get the tracked market
        let market = match self.markets.get_mut(event_id) {
            Some(m) => m,
            None => {
                warn!("No tracked market for maker: {}", event_id);
                return Ok(TradeAction::SkipSizing);
            }
        };

        // Default maker order config
        let maker_config = MakerOrderConfig::default();

        // Check for existing orders on this token that need management
        let orders_to_cancel: Vec<String> = market
            .active_maker_orders
            .iter()
            .filter(|(_, order)| {
                order.token_id == *token_id
                    && (order.is_stale(now_ms, maker_config.stale_threshold_ms)
                        || order.needs_price_refresh(
                            opportunity.price,
                            maker_config.price_refresh_threshold_bps,
                        )
                        || order.is_fully_filled())
            })
            .map(|(id, _)| id.clone())
            .collect();

        // Cancel stale/misplaced orders
        for order_id in &orders_to_cancel {
            debug!("Cancelling stale maker order: {}", order_id);
            match self.executor.cancel_order(order_id).await {
                Ok(cancellation) => {
                    // Record any partial fills from cancelled order
                    if cancellation.filled_size > Decimal::ZERO
                        && let Some(order) = market.active_maker_orders.get(order_id)
                    {
                        let cost = cancellation.filled_size * order.price;
                        let outcome = if order.token_id == market.yes_token_id {
                            Outcome::Yes
                        } else {
                            Outcome::No
                        };
                        market.inventory.record_fill(outcome, cancellation.filled_size, cost);
                    }
                    market.active_maker_orders.remove(order_id);
                }
                Err(e) => {
                    warn!("Failed to cancel maker order {}: {}", order_id, e);
                    // Remove from tracking anyway to avoid repeated cancellation attempts
                    market.active_maker_orders.remove(order_id);
                }
            }
        }

        // Check if we already have an active order at this price for this token
        let has_existing_at_price = market
            .active_maker_orders
            .values()
            .any(|o| o.token_id == *token_id && o.side == opportunity.side && o.price == opportunity.price);

        if has_existing_at_price {
            trace!("Already have maker order at {} for {}", opportunity.price, token_id);
            return Ok(TradeAction::Execute);
        }

        // Check if we're at max orders for this market
        if market.active_maker_orders.len() >= maker_config.max_orders_per_market {
            trace!("Max maker orders reached for {}", event_id);
            return Ok(TradeAction::SkipSizing);
        }

        // Determine outcome based on token
        let outcome = if *token_id == market.yes_token_id {
            Outcome::Yes
        } else {
            Outcome::No
        };

        // Generate request ID
        let req_id_base = self.decision_counter;
        self.decision_counter += 1;

        // Create the maker order
        let order = OrderRequest {
            request_id: format!("maker-{}-{}", req_id_base, if outcome == Outcome::Yes { "yes" } else { "no" }),
            event_id: event_id.clone(),
            token_id: token_id.clone(),
            outcome,
            side: opportunity.side,
            size: opportunity.size,
            price: Some(opportunity.price),
            order_type: OrderType::Gtc, // Good-till-cancelled for passive maker
            timeout_ms: None,
            timestamp: Utc::now(),
        };

        info!(
            "Placing maker order: {} {} {} @ {} size={} expected_rebate={}",
            event_id,
            if outcome == Outcome::Yes { "YES" } else { "NO" },
            opportunity.side,
            opportunity.price,
            opportunity.size,
            opportunity.expected_rebate
        );

        // Place the order
        let result = self.executor.place_order(order).await;

        match result {
            Ok(OrderResult::Filled(fill)) => {
                // Immediate fill - record it
                let cost = fill.size * fill.price + fill.fee;
                market.inventory.record_fill(outcome, fill.size, cost);

                // Update volume metrics
                let volume_cents = (fill.size * fill.price * Decimal::new(100, 0))
                    .try_into()
                    .unwrap_or(0u64);
                self.state.metrics.add_volume_cents(volume_cents);
                self.state.record_success();

                debug!("Maker order filled immediately: {} @ {}", fill.size, fill.price);
            }
            Ok(OrderResult::PartialFill(fill)) => {
                // Partial fill - record filled portion and track remaining
                let cost = fill.filled_size * fill.avg_price + fill.fee;
                market.inventory.record_fill(outcome, fill.filled_size, cost);

                // Track remaining as active order
                let remaining_size = fill.requested_size - fill.filled_size;
                if remaining_size > Decimal::ZERO {
                    let active_order = ActiveMakerOrder::new(
                        fill.order_id.clone(),
                        token_id.clone(),
                        opportunity.side,
                        opportunity.price,
                        remaining_size,
                        now_ms,
                    );
                    market.active_maker_orders.insert(fill.order_id, active_order);
                }

                // Update metrics
                let volume_cents = (fill.filled_size * fill.avg_price * Decimal::new(100, 0))
                    .try_into()
                    .unwrap_or(0u64);
                self.state.metrics.add_volume_cents(volume_cents);
                self.state.record_success();

                debug!("Maker order partially filled: {} / {} @ {}",
                    fill.filled_size, fill.requested_size, fill.avg_price);
            }
            Ok(OrderResult::Pending(pending)) => {
                // Order is pending - track it
                let active_order = ActiveMakerOrder::new(
                    pending.order_id.clone(),
                    token_id.clone(),
                    opportunity.side,
                    opportunity.price,
                    opportunity.size,
                    now_ms,
                );
                market.active_maker_orders.insert(pending.order_id, active_order);
                debug!("Maker order pending: {}", pending.request_id);
            }
            Ok(OrderResult::Rejected(rejection)) => {
                warn!("Maker order rejected: {}", rejection.reason);
                return Ok(TradeAction::SkipSizing);
            }
            Ok(OrderResult::Cancelled(_)) => {
                // Shouldn't happen for new orders
                warn!("Maker order unexpectedly cancelled");
                return Ok(TradeAction::SkipSizing);
            }
            Err(e) => {
                warn!("Maker order failed: {}", e);
                if self.state.record_failure(self.config.max_consecutive_failures) {
                    self.state.trip_circuit_breaker();
                    warn!("Circuit breaker tripped after consecutive failures");
                }
                return Ok(TradeAction::SkipSizing);
            }
        }

        Ok(TradeAction::Execute)
    }

    /// Refresh active maker orders that have become stale or mispriced.
    ///
    /// This method should be called periodically (e.g., on heartbeat) to:
    /// 1. Cancel orders that are too old
    /// 2. Cancel orders where price has moved significantly
    /// 3. Place new orders at current optimal prices
    ///
    /// # Arguments
    ///
    /// * `event_id` - The market to refresh orders for
    /// * `maker_config` - Configuration for staleness thresholds
    #[allow(dead_code)] // Used in phase 7 when engines are integrated
    async fn refresh_maker_orders(
        &mut self,
        event_id: &str,
        maker_config: &MakerOrderConfig,
    ) -> Result<(), StrategyError> {
        let now_ms = chrono::Utc::now().timestamp_millis();

        // Get the market - need to do this in steps to avoid borrow issues
        let market = match self.markets.get(event_id) {
            Some(m) => m,
            None => return Ok(()),
        };

        // Collect orders that need refresh
        let orders_to_refresh: Vec<(String, String, Side)> = market
            .active_maker_orders
            .iter()
            .filter(|(_, order)| {
                order.is_stale(now_ms, maker_config.stale_threshold_ms)
                    || order.is_fully_filled()
            })
            .map(|(id, order)| (id.clone(), order.token_id.clone(), order.side))
            .collect();

        if orders_to_refresh.is_empty() {
            return Ok(());
        }

        debug!("Refreshing {} stale maker orders for {}", orders_to_refresh.len(), event_id);

        // Cancel each stale order
        for (order_id, token_id, _side) in orders_to_refresh {
            match self.executor.cancel_order(&order_id).await {
                Ok(cancellation) => {
                    // Need to get market again due to borrow
                    if let Some(market) = self.markets.get_mut(event_id) {
                        // Record any partial fills
                        if cancellation.filled_size > Decimal::ZERO
                            && let Some(order) = market.active_maker_orders.get(&order_id)
                        {
                            let cost = cancellation.filled_size * order.price;
                            let outcome = if token_id == market.yes_token_id {
                                Outcome::Yes
                            } else {
                                Outcome::No
                            };
                            market.inventory.record_fill(outcome, cancellation.filled_size, cost);
                        }
                        market.active_maker_orders.remove(&order_id);
                    }
                }
                Err(e) => {
                    warn!("Failed to cancel stale maker order {}: {}", order_id, e);
                    // Remove from tracking to avoid repeated attempts
                    if let Some(market) = self.markets.get_mut(event_id) {
                        market.active_maker_orders.remove(&order_id);
                    }
                }
            }
        }

        Ok(())
    }

    /// Update maker order tracking when a fill event is received.
    ///
    /// This handles partial fills on active maker orders by updating
    /// the tracked remaining size.
    #[allow(dead_code)] // Used in phase 7 when engines are integrated
    fn handle_maker_fill(&mut self, event_id: &str, order_id: &str, filled_size: Decimal) {
        let now_ms = chrono::Utc::now().timestamp_millis();

        if let Some(market) = self.markets.get_mut(event_id)
            && let Some(order) = market.active_maker_orders.get_mut(order_id)
        {
            order.record_fill(filled_size, now_ms);

            // Remove if fully filled
            if order.is_fully_filled() {
                market.active_maker_orders.remove(order_id);
            }
        }
    }

    /// Get count of active maker orders for a market.
    pub fn active_maker_order_count(&self, event_id: &str) -> usize {
        self.markets
            .get(event_id)
            .map(|m| m.active_maker_orders.len())
            .unwrap_or(0)
    }

    /// Record a decision for observability (legacy arb-only format).
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

    /// Record a multi-engine decision for observability.
    ///
    /// This includes the engine source for multi-engine strategy analysis.
    #[allow(clippy::too_many_arguments)]
    fn record_multi_decision(
        &mut self,
        event_id: &str,
        asset: CryptoAsset,
        engine: EngineType,
        aggregated: AggregatedDecision,
        sizing: Option<SizingResult>,
        toxic_warning: Option<ToxicFlowWarning>,
        action: TradeAction,
        latency_us: u64,
    ) {
        let decision = MultiEngineDecision {
            decision_id: self.decision_counter,
            event_id: event_id.to_string(),
            asset,
            engine,
            aggregated,
            sizing,
            toxic_warning,
            action,
            timestamp: Utc::now(),
            latency_us,
        };

        self.decision_counter += 1;

        // Fire-and-forget to multi-engine observability channel
        if let Some(ref sender) = self.multi_obs_sender {
            // Use try_send to avoid blocking the hot path
            if sender.try_send(decision).is_err() {
                trace!("Multi-engine observability channel full, dropping decision");
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

    // =========================================================================
    // Maker Price Calculation Tests
    // =========================================================================

    #[test]
    fn test_calculate_maker_price_wide_spread() {
        // Wide spread (>3%): 40% from bid toward ask
        let mut book = crate::types::OrderBook::new("test".to_string());
        book.bids.push(PriceLevel::new(dec!(0.40), dec!(100))); // bid
        book.asks.push(PriceLevel::new(dec!(0.50), dec!(100))); // ask
        // spread = 0.10, mid = 0.45, spread_ratio = 0.10/0.45 = 22% > 3%

        // Buy: price = bid + spread * 0.40 = 0.40 + 0.10 * 0.40 = 0.44
        let buy_price = calculate_maker_price(&book, Side::Buy);
        assert!(buy_price.is_some());
        assert_eq!(buy_price.unwrap(), dec!(0.44));

        // Sell: price = ask - spread * 0.40 = 0.50 - 0.10 * 0.40 = 0.46
        let sell_price = calculate_maker_price(&book, Side::Sell);
        assert!(sell_price.is_some());
        assert_eq!(sell_price.unwrap(), dec!(0.46));
    }

    #[test]
    fn test_calculate_maker_price_medium_spread() {
        // Medium spread (1-3%): 30% from bid toward ask
        let mut book = crate::types::OrderBook::new("test".to_string());
        book.bids.push(PriceLevel::new(dec!(0.49), dec!(100))); // bid
        book.asks.push(PriceLevel::new(dec!(0.51), dec!(100))); // ask
        // spread = 0.02, mid = 0.50, spread_ratio = 0.02/0.50 = 4% -- wait that's > 3%
        // Let me use different values
        book.bids.clear();
        book.asks.clear();
        book.bids.push(PriceLevel::new(dec!(0.495), dec!(100))); // bid
        book.asks.push(PriceLevel::new(dec!(0.505), dec!(100))); // ask
        // spread = 0.01, mid = 0.50, spread_ratio = 0.01/0.50 = 2% (between 1-3%)

        // Buy: price = bid + spread * 0.30 = 0.495 + 0.01 * 0.30 = 0.498
        let buy_price = calculate_maker_price(&book, Side::Buy);
        assert!(buy_price.is_some());
        // Rounded to 2 decimal places: 0.50
        assert_eq!(buy_price.unwrap(), dec!(0.50));

        // Sell: price = ask - spread * 0.30 = 0.505 - 0.01 * 0.30 = 0.502
        let sell_price = calculate_maker_price(&book, Side::Sell);
        assert!(sell_price.is_some());
        // Rounded to 2 decimal places: 0.50
        assert_eq!(sell_price.unwrap(), dec!(0.50));
    }

    #[test]
    fn test_calculate_maker_price_tight_spread() {
        // Tight spread (<1%): at bid for buy, at ask for sell
        let mut book = crate::types::OrderBook::new("test".to_string());
        book.bids.push(PriceLevel::new(dec!(0.498), dec!(100))); // bid
        book.asks.push(PriceLevel::new(dec!(0.502), dec!(100))); // ask
        // spread = 0.004, mid = 0.50, spread_ratio = 0.004/0.50 = 0.8% < 1%

        // Buy: price = bid + spread * 0 = bid = 0.498
        let buy_price = calculate_maker_price(&book, Side::Buy);
        assert!(buy_price.is_some());
        // Rounded to 2 decimal places: 0.50
        assert_eq!(buy_price.unwrap(), dec!(0.50));

        // Sell: price = ask - spread * 0 = ask = 0.502
        let sell_price = calculate_maker_price(&book, Side::Sell);
        assert!(sell_price.is_some());
        // Rounded to 2 decimal places: 0.50
        assert_eq!(sell_price.unwrap(), dec!(0.50));
    }

    #[test]
    fn test_calculate_maker_price_no_bid() {
        let mut book = crate::types::OrderBook::new("test".to_string());
        book.asks.push(PriceLevel::new(dec!(0.50), dec!(100)));
        // No bids

        let price = calculate_maker_price(&book, Side::Buy);
        assert!(price.is_none());
    }

    #[test]
    fn test_calculate_maker_price_no_ask() {
        let mut book = crate::types::OrderBook::new("test".to_string());
        book.bids.push(PriceLevel::new(dec!(0.45), dec!(100)));
        // No asks

        let price = calculate_maker_price(&book, Side::Buy);
        assert!(price.is_none());
    }

    #[test]
    fn test_calculate_maker_price_empty_book() {
        let book = crate::types::OrderBook::new("test".to_string());

        let price = calculate_maker_price(&book, Side::Buy);
        assert!(price.is_none());

        let price = calculate_maker_price(&book, Side::Sell);
        assert!(price.is_none());
    }

    // =========================================================================
    // MakerOrderConfig Tests
    // =========================================================================

    #[test]
    fn test_maker_order_config_default() {
        let config = MakerOrderConfig::default();
        assert_eq!(config.stale_threshold_ms, 5000);
        assert_eq!(config.price_refresh_threshold_bps, 20);
        assert_eq!(config.max_orders_per_market, 4);
    }

    // =========================================================================
    // ActiveMakerOrder Tests
    // =========================================================================

    #[test]
    fn test_active_maker_order_new() {
        let order = ActiveMakerOrder::new(
            "order-123".to_string(),
            "token-abc".to_string(),
            Side::Buy,
            dec!(0.45),
            dec!(100),
            1000,
        );

        assert_eq!(order.order_id, "order-123");
        assert_eq!(order.token_id, "token-abc");
        assert_eq!(order.side, Side::Buy);
        assert_eq!(order.price, dec!(0.45));
        assert_eq!(order.original_size, dec!(100));
        assert_eq!(order.remaining_size, dec!(100));
        assert_eq!(order.filled_size, Decimal::ZERO);
        assert_eq!(order.placed_at_ms, 1000);
        assert_eq!(order.updated_at_ms, 1000);
    }

    #[test]
    fn test_active_maker_order_is_stale() {
        let order = ActiveMakerOrder::new(
            "order-1".to_string(),
            "token-1".to_string(),
            Side::Buy,
            dec!(0.50),
            dec!(50),
            1000, // placed at 1000ms
        );

        // Not stale yet (only 4000ms passed, threshold is 5000)
        assert!(!order.is_stale(5000, 5000));

        // Now it's stale (5001ms passed > 5000 threshold)
        assert!(order.is_stale(6001, 5000));

        // Exactly at threshold - not stale
        assert!(!order.is_stale(6000, 5000));
    }

    #[test]
    fn test_active_maker_order_needs_price_refresh() {
        let order = ActiveMakerOrder::new(
            "order-1".to_string(),
            "token-1".to_string(),
            Side::Buy,
            dec!(0.50),
            dec!(50),
            1000,
        );

        // Small price change (0.1% = 10 bps) - no refresh needed at 20 bps threshold
        assert!(!order.needs_price_refresh(dec!(0.4995), 20));

        // Large price change (1% = 100 bps) - needs refresh
        assert!(order.needs_price_refresh(dec!(0.495), 20));

        // Price at exactly threshold (0.2% = 20 bps)
        // diff = 0.001, price = 0.50, diff/price = 0.002 = 20 bps
        assert!(!order.needs_price_refresh(dec!(0.499), 20)); // Not > threshold

        // Just over threshold
        assert!(order.needs_price_refresh(dec!(0.4989), 20)); // >20 bps
    }

    #[test]
    fn test_active_maker_order_needs_price_refresh_zero_price() {
        let order = ActiveMakerOrder::new(
            "order-1".to_string(),
            "token-1".to_string(),
            Side::Buy,
            Decimal::ZERO, // Zero price
            dec!(50),
            1000,
        );

        // Should not trigger refresh with zero price
        assert!(!order.needs_price_refresh(dec!(0.50), 20));
    }

    #[test]
    fn test_active_maker_order_record_fill() {
        let mut order = ActiveMakerOrder::new(
            "order-1".to_string(),
            "token-1".to_string(),
            Side::Buy,
            dec!(0.50),
            dec!(100),
            1000,
        );

        // First partial fill
        order.record_fill(dec!(30), 2000);
        assert_eq!(order.filled_size, dec!(30));
        assert_eq!(order.remaining_size, dec!(70));
        assert_eq!(order.updated_at_ms, 2000);
        assert!(!order.is_fully_filled());

        // Second partial fill
        order.record_fill(dec!(50), 3000);
        assert_eq!(order.filled_size, dec!(80));
        assert_eq!(order.remaining_size, dec!(20));
        assert_eq!(order.updated_at_ms, 3000);
        assert!(!order.is_fully_filled());

        // Final fill
        order.record_fill(dec!(20), 4000);
        assert_eq!(order.filled_size, dec!(100));
        assert_eq!(order.remaining_size, Decimal::ZERO);
        assert_eq!(order.updated_at_ms, 4000);
        assert!(order.is_fully_filled());
    }

    #[test]
    fn test_active_maker_order_is_fully_filled() {
        let mut order = ActiveMakerOrder::new(
            "order-1".to_string(),
            "token-1".to_string(),
            Side::Buy,
            dec!(0.50),
            dec!(100),
            1000,
        );

        assert!(!order.is_fully_filled());

        order.record_fill(dec!(100), 2000);
        assert!(order.is_fully_filled());
    }

    #[test]
    fn test_active_maker_order_overfill() {
        let mut order = ActiveMakerOrder::new(
            "order-1".to_string(),
            "token-1".to_string(),
            Side::Buy,
            dec!(0.50),
            dec!(100),
            1000,
        );

        // Overfill (shouldn't happen but handle gracefully)
        order.record_fill(dec!(150), 2000);
        assert_eq!(order.filled_size, dec!(150));
        assert_eq!(order.remaining_size, dec!(-50)); // Negative remaining
        assert!(order.is_fully_filled()); // Still counts as fully filled
    }

    #[tokio::test]
    async fn test_active_maker_order_count() {
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

        // No maker orders have been placed yet
        assert_eq!(strategy.active_maker_order_count("event1"), 0);
        assert_eq!(strategy.active_maker_order_count("nonexistent"), 0);
    }

    #[test]
    fn test_tracked_market_has_active_maker_orders() {
        let event = WindowOpenEvent {
            event_id: "test-event".to_string(),
            asset: CryptoAsset::Btc,
            yes_token_id: "yes-token".to_string(),
            no_token_id: "no-token".to_string(),
            strike_price: dec!(100000),
            window_start: Utc::now(),
            window_end: Utc::now() + chrono::Duration::minutes(15),
            timestamp: Utc::now(),
        };

        let market = TrackedMarket::new(&event, dec!(100));
        assert!(market.active_maker_orders.is_empty());
    }
}
