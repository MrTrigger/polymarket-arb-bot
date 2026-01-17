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
use rust_decimal::prelude::ToPrimitive;
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
use crate::dashboard::types::{DashboardOrderType, TradeRecord, TradeSide, TradeStatus};
use crate::data_source::{
    BookDeltaEvent, BookSnapshotEvent, DataSource, DataSourceError, FillEvent, MarketEvent,
    SpotPriceEvent, WindowCloseEvent, WindowOpenEvent,
};
use crate::executor::{Executor, ExecutorError, OrderRequest, OrderResult, OrderType};
use crate::executor::chase::PriceChaser;
use crate::state::{ActiveWindow, GlobalState};
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
    fn new(event: &WindowOpenEvent, market_budget: Decimal, strategy_config: &StrategyConfig) -> Self {
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

        // Build PositionConfig with confidence params from StrategyConfig
        let position_config = position::PositionConfig {
            total_budget: market_budget,
            atr,
            // Phase-based thresholds (legacy mode)
            early_threshold: strategy_config.early_threshold,
            build_threshold: strategy_config.build_threshold,
            core_threshold: strategy_config.core_threshold,
            final_threshold: strategy_config.final_threshold,
            // EV-based confidence params
            time_conf_floor: strategy_config.time_conf_floor,
            dist_conf_floor: strategy_config.dist_conf_floor,
            dist_conf_per_atr: strategy_config.dist_conf_per_atr,
            ..Default::default()
        };

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
            position_manager: position::PositionManager::new(position_config),
        }
    }

    /// Check if we should trade using phase-based position management.
    ///
    /// This implements the spec's intelligent capital allocation:
    /// 1. **Reserve capital for later phases** when signals are clearer
    /// 2. **Gate trades by confidence threshold** - stricter early, looser late
    /// 3. **Scale position size by confidence** - bigger when more certain
    /// 4. **Enforce position limits** - never go all-in on one side
    fn should_trade(
        &self,
        distance_dollars: Decimal,
        seconds_remaining: i64,
        favorable_price: Decimal,
        max_edge_factor: Decimal,
        window_duration_secs: i64,
    ) -> position::TradeDecision {
        self.position_manager.should_trade(
            distance_dollars,
            seconds_remaining,
            favorable_price,
            max_edge_factor,
            window_duration_secs,
        )
    }

    /// Check if we should trade using an externally-provided confidence value.
    /// This allows incorporating signal strength into the EV calculation.
    fn should_trade_with_confidence(
        &self,
        confidence: Decimal,
        seconds_remaining: i64,
        favorable_price: Decimal,
        max_edge_factor: Decimal,
        window_duration_secs: i64,
    ) -> position::TradeDecision {
        self.position_manager.should_trade_with_confidence(
            confidence,
            seconds_remaining,
            favorable_price,
            max_edge_factor,
            window_duration_secs,
        )
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
    #[allow(dead_code)]
    fn budget_remaining(&self) -> Decimal {
        self.position_manager.budget_remaining()
    }

    /// Update seconds remaining based on simulated time.
    fn update_time(&mut self, current_time: DateTime<Utc>) {
        self.state.seconds_remaining = (self.window_end - current_time).num_seconds().max(0);
    }

    /// Check if window has expired based on simulated time.
    fn is_expired(&self, current_time: DateTime<Utc>) -> bool {
        current_time >= self.window_end
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
    /// Window duration in seconds (900 for 15min, 3600 for 1h).
    /// Used for edge requirement calculation in directional trading.
    pub window_duration_secs: i64,

    // Phase-based confidence thresholds (configurable via sweep)
    /// Minimum confidence for early phase (>10 min remaining).
    pub early_threshold: Decimal,
    /// Minimum confidence for build phase (5-10 min remaining).
    pub build_threshold: Decimal,
    /// Minimum confidence for core phase (2-5 min remaining).
    pub core_threshold: Decimal,
    /// Minimum confidence for final phase (<2 min remaining).
    pub final_threshold: Decimal,

    /// Maximum edge factor (minimum EV required at window start).
    /// When > 0, uses EV-based: trade if (confidence - price) >= min_edge
    /// When = 0, uses phase-based thresholds (legacy mode).
    pub max_edge_factor: Decimal,

    // Confidence calculation params (EV-based mode)
    /// Minimum time confidence at window start (default 0.30).
    /// Formula: time_conf = floor + (1 - floor) * (1 - time_ratio)
    pub time_conf_floor: Decimal,
    /// Minimum distance confidence for tiny moves (default 0.20).
    pub dist_conf_floor: Decimal,
    /// Confidence gained per ATR of movement (default 0.50).
    /// Formula: dist_conf = clamp(floor + per_atr * atr_multiple, floor, 1.0)
    pub dist_conf_per_atr: Decimal,

    // Allocation ratios for directional trading (configurable via sweep)
    /// Strong UP signal allocation ratio (0.0-1.0).
    pub strong_up_ratio: Decimal,
    /// Lean UP signal allocation ratio (0.0-1.0).
    pub lean_up_ratio: Decimal,

    /// Execution configuration (order mode, chase settings).
    pub execution: crate::config::ExecutionConfig,

    /// Whether running in backtest mode (disables price chasing).
    pub is_backtest: bool,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            arb_thresholds: ArbThresholds::default(),
            toxic_config: ToxicFlowConfig::default(),
            sizing_config: SizingConfig::default(),
            max_consecutive_failures: 3,
            block_on_toxic_high: true,
            window_duration_secs: 900, // 15-minute windows by default
            // Legacy phase-based thresholds (used when max_edge_factor = 0)
            early_threshold: dec!(0.80),
            build_threshold: dec!(0.60),
            core_threshold: dec!(0.50),
            final_threshold: dec!(0.40),
            // EV-based mode: trade if (confidence - price) >= min_edge
            max_edge_factor: dec!(0.08), // 8% EV required at window start (sweep optimal)
            // Confidence calculation params
            time_conf_floor: dec!(0.30),     // 30% confidence at window start
            dist_conf_floor: dec!(0.15),     // 15% minimum for tiny moves (sweep optimal)
            dist_conf_per_atr: dec!(0.30),   // +30% per ATR of movement (sweep optimal)
            // Allocation ratios (optimized via param sweep)
            strong_up_ratio: dec!(0.82),
            lean_up_ratio: dec!(0.65),
            execution: crate::config::ExecutionConfig::default(),
            is_backtest: false,
        }
    }
}

impl StrategyConfig {
    /// Create from trading config with window duration.
    pub fn from_trading_config(config: &TradingConfig, window_duration_secs: i64) -> Self {
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
            window_duration_secs,
            // Legacy phase thresholds (used when max_edge_factor = 0)
            early_threshold: dec!(0.80),
            build_threshold: dec!(0.60),
            core_threshold: dec!(0.50),
            final_threshold: dec!(0.40),
            // EV-based mode params (sweep optimal)
            max_edge_factor: dec!(0.08),
            time_conf_floor: dec!(0.30),
            dist_conf_floor: dec!(0.15),
            dist_conf_per_atr: dec!(0.30),
            // Allocation ratios (optimized via param sweep)
            strong_up_ratio: dec!(0.82),
            lean_up_ratio: dec!(0.65),
            execution: crate::config::ExecutionConfig::default(),
            is_backtest: false,
        }
    }

    /// Create from trading config with custom phase thresholds.
    pub fn from_trading_config_with_phases(
        config: &TradingConfig,
        window_duration_secs: i64,
        early_threshold: Decimal,
        build_threshold: Decimal,
        core_threshold: Decimal,
        final_threshold: Decimal,
    ) -> Self {
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
            window_duration_secs,
            early_threshold,
            build_threshold,
            core_threshold,
            final_threshold,
            // EV-based mode params (sweep optimal)
            max_edge_factor: dec!(0.08),
            time_conf_floor: dec!(0.30),
            dist_conf_floor: dec!(0.15),
            dist_conf_per_atr: dec!(0.30),
            // Allocation ratios (optimized via param sweep)
            strong_up_ratio: dec!(0.82),
            lean_up_ratio: dec!(0.65),
            execution: crate::config::ExecutionConfig::default(),
            is_backtest: false,
        }
    }

    /// Set backtest mode (disables price chasing).
    pub fn with_backtest(mut self, is_backtest: bool) -> Self {
        self.is_backtest = is_backtest;
        self
    }

    /// Set the window duration on an existing config.
    pub fn with_window_duration(mut self, window_duration_secs: i64) -> Self {
        self.window_duration_secs = window_duration_secs;
        self
    }

    /// Set the execution configuration.
    pub fn with_execution(mut self, execution: crate::config::ExecutionConfig) -> Self {
        self.execution = execution;
        self
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
    /// Simulated time for backtest mode (uses event timestamps instead of wall-clock time).
    simulated_time: DateTime<Utc>,
    /// Price chaser for maker execution with fill polling.
    chaser: PriceChaser,
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
        // Create directional detector with config from engines_config
        let directional_config = DirectionalConfig {
            min_seconds_remaining: engines_config.directional.min_seconds_remaining as i64,
            max_combined_cost: engines_config.directional.max_combined_cost,
            max_spread_ratio: Decimal::from(engines_config.directional.max_spread_bps) / dec!(10000),
            min_favorable_depth: engines_config.directional.min_favorable_depth,
        };
        let directional_detector = DirectionalDetector::with_config(directional_config);
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
            simulated_time: Utc::now(),
            chaser: PriceChaser::with_defaults(),
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

        // Update simulated time from event timestamp (for backtest mode)
        self.simulated_time = event.timestamp();

        // Process event and get which markets were affected
        let affected_markets: Vec<String> = match event {
            MarketEvent::SpotPrice(e) => {
                let asset = e.asset;
                self.handle_spot_price(e);
                // Spot price affects all markets for this asset
                self.markets.iter()
                    .filter(|(_, m)| m.asset == asset)
                    .map(|(id, _)| id.clone())
                    .collect()
            }
            MarketEvent::BookSnapshot(e) => {
                let event_id = self.handle_book_snapshot(e).await;
                event_id.into_iter().collect()
            }
            MarketEvent::BookDelta(e) => {
                let event_id = self.handle_book_delta(e);
                event_id.into_iter().collect()
            }
            MarketEvent::Fill(e) => {
                self.handle_fill(e).await?;
                vec![] // Fills don't trigger new opportunity checks
            }
            MarketEvent::WindowOpen(e) => {
                let event_id = e.event_id.clone();
                self.handle_window_open(e);
                vec![event_id]
            }
            MarketEvent::WindowClose(e) => {
                self.handle_window_close(e).await;
                vec![] // Market is closed, no opportunities
            }
            MarketEvent::Heartbeat(_) => {
                // Heartbeat - update time remaining and settle expired markets
                self.update_all_market_times(self.simulated_time);
                self.settle_expired_markets().await;
                return Ok(());
            }
        };

        // Only check opportunities for affected markets
        if !affected_markets.is_empty() {
            let latency_us = event_time.elapsed().as_micros() as u64;
            self.check_opportunities_for_markets(&affected_markets, latency_us).await?;
        }

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
                    market.strike_price = event.price; // Also update the TrackedMarket field

                    // Also update ActiveWindow in GlobalState for dashboard display
                    if let Some(mut window) = self.state.market_data.active_windows.get_mut(&market.event_id) {
                        window.strike_price = event.price;
                    }
                }

                // Sync confidence data for dashboard display (if strike is set)
                if !market.strike_price.is_zero() {
                    let distance_dollars = (event.price - market.strike_price).abs();
                    let seconds_remaining = market.state.seconds_remaining;
                    let window_duration_secs = self.config.window_duration_secs;
                    let atr = market.position_manager.atr();

                    // Calculate confidence components
                    let pm_conf = market.position_manager.calculate_confidence(
                        distance_dollars,
                        seconds_remaining,
                        window_duration_secs,
                    );
                    let atr_mult = if atr > Decimal::ZERO { distance_dollars / atr } else { Decimal::ZERO };
                    let time_ratio = Decimal::new(seconds_remaining, 0) / Decimal::new(window_duration_secs, 0);
                    let time_conf = dec!(0.30) + dec!(0.70) * (Decimal::ONE - time_ratio);
                    let dist_conf = (dec!(0.20) + dec!(0.50) * atr_mult).min(Decimal::ONE);

                    // Calculate threshold
                    let max_edge_factor = self.engines_config.directional.max_edge_factor;
                    let min_edge = max_edge_factor * time_ratio;

                    // Use best ask as favorable price estimate (conservative)
                    let favorable_price = if event.price > market.strike_price {
                        // Price above strike - YES is favorable
                        market.state.yes_book.best_ask().unwrap_or(dec!(0.50))
                    } else {
                        // Price below strike - NO is favorable
                        market.state.no_book.best_ask().unwrap_or(dec!(0.50))
                    };

                    let ev = pm_conf - favorable_price;
                    let would_trade = ev >= min_edge;

                    let snapshot = crate::state::ConfidenceSnapshot::new(
                        market.event_id.clone(),
                        pm_conf,
                        time_conf,
                        dist_conf,
                        min_edge,
                        ev,
                        would_trade,
                        distance_dollars,
                        atr_mult,
                        favorable_price,
                    );
                    self.state.market_data.update_confidence(&market.event_id, snapshot);
                }
            }
        }
    }

    /// Log periodic market status for debugging.
    fn log_market_status(&self, current_time: DateTime<Utc>) {
        // Find the closest market to expiry
        let mut closest_market: Option<(&String, &TrackedMarket)> = None;
        for (event_id, market) in &self.markets {
            if market.is_expired(current_time) {
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

    /// Handle order book snapshot. Returns the affected event_id if any.
    async fn handle_book_snapshot(&mut self, event: BookSnapshotEvent) -> Option<String> {
        trace!("Book snapshot: {} ({} bids, {} asks)",
            event.token_id, event.bids.len(), event.asks.len());

        // Update executor's order book (for backtest simulation)
        self.executor.update_order_book(
            &event.token_id,
            event.bids.clone(),
            event.asks.clone(),
        ).await;

        // Find the market this token belongs to
        let event_id = match self.token_to_event.get(&event.token_id) {
            Some(id) => id.clone(),
            None => {
                trace!("Unknown token {}, ignoring snapshot", event.token_id);
                return None;
            }
        };

        let market = match self.markets.get_mut(&event_id) {
            Some(m) => m,
            None => return None,
        };

        // Determine which book to update and whether it's YES or NO
        let is_yes = event.token_id == market.yes_token_id;
        let book = if is_yes {
            &mut market.state.yes_book
        } else if event.token_id == market.no_token_id {
            &mut market.state.no_book
        } else {
            return None;
        };

        // Apply snapshot
        book.apply_snapshot(
            event.bids,
            event.asks,
            event.timestamp.timestamp_millis(),
        );

        // Sync to GlobalState for dashboard display
        let live_book = crate::state::LiveOrderBook {
            token_id: event.token_id.clone(),
            best_bid: book.best_bid().unwrap_or(Decimal::ZERO),
            best_bid_size: book.best_bid_size().unwrap_or(Decimal::ZERO),
            best_ask: book.best_ask().unwrap_or(Decimal::ZERO),
            best_ask_size: book.best_ask_size().unwrap_or(Decimal::ZERO),
            last_update_ms: book.last_update_ms,
        };
        self.state.market_data.update_order_book(&event.token_id, live_book);

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

        Some(event_id)
    }

    /// Handle order book delta. Returns the affected event_id if any.
    fn handle_book_delta(&mut self, event: BookDeltaEvent) -> Option<String> {
        trace!("Book delta: {} {} @ {} (size {})",
            event.token_id, event.side, event.price, event.size);

        // Find the market this token belongs to
        let event_id = match self.token_to_event.get(&event.token_id) {
            Some(id) => id.clone(),
            None => return None,
        };

        let market = match self.markets.get_mut(&event_id) {
            Some(m) => m,
            None => return None,
        };

        // Determine which book to update
        let is_yes = event.token_id == market.yes_token_id;
        let book = if is_yes {
            &mut market.state.yes_book
        } else if event.token_id == market.no_token_id {
            &mut market.state.no_book
        } else {
            return None;
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

        // Sync to GlobalState for dashboard display
        let live_book = crate::state::LiveOrderBook {
            token_id: event.token_id.clone(),
            best_bid: book.best_bid().unwrap_or(Decimal::ZERO),
            best_bid_size: book.best_bid_size().unwrap_or(Decimal::ZERO),
            best_ask: book.best_ask().unwrap_or(Decimal::ZERO),
            best_ask_size: book.best_ask_size().unwrap_or(Decimal::ZERO),
            last_update_ms: book.last_update_ms,
        };
        self.state.market_data.update_order_book(&event.token_id, live_book);

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

        Some(event_id)
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

            // Create and store trade record for dashboard
            let spot_price = self.spot_prices
                .get(&market.asset)
                .copied()
                .unwrap_or(Decimal::ZERO);

            // Calculate arb margin from order books
            let arb_margin = match (market.state.yes_book.best_ask(), market.state.no_book.best_ask()) {
                (Some(yes_ask), Some(no_ask)) if yes_ask > Decimal::ZERO && no_ask > Decimal::ZERO => {
                    Decimal::ONE - yes_ask - no_ask
                }
                _ => Decimal::ZERO,
            };

            let mut trade = TradeRecord::new(
                uuid::Uuid::new_v4(), // session_id - use a placeholder for now
                0, // decision_id - not tracked per trade currently
                event.event_id.clone(),
                event.token_id.clone(),
                event.outcome,
                TradeSide::from(event.side),
                DashboardOrderType::Market,
                event.price,
                event.size,
            );
            trade.fill_price = event.price;
            trade.fill_size = event.size;
            trade.fees = event.fee;
            trade.total_cost = cost;
            trade.fill_time = event.timestamp;
            trade.spot_price_at_fill = spot_price;
            trade.arb_margin_at_fill = arb_margin;
            trade.status = TradeStatus::Filled;

            self.state.market_data.add_trade(trade);

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
        // Pass strategy config so confidence params flow through to PositionManager
        let market = TrackedMarket::new(&event, market_budget, &self.config);

        // Update token mapping
        self.token_to_event.insert(event.yes_token_id.clone(), event.event_id.clone());
        self.token_to_event.insert(event.no_token_id.clone(), event.event_id.clone());

        // Add to tracked markets
        self.markets.insert(event.event_id.clone(), market);

        // Sync to global state for dashboard display
        let active_window = ActiveWindow {
            event_id: event.event_id.clone(),
            asset: event.asset,
            yes_token_id: event.yes_token_id,
            no_token_id: event.no_token_id,
            strike_price: event.strike_price,
            window_end: event.window_end,
        };
        self.state.market_data.active_windows.insert(event.event_id.clone(), active_window);
    }

    /// Handle window close.
    async fn handle_window_close(&mut self, event: WindowCloseEvent) {
        info!("Window close: {} outcome={:?}", event.event_id, event.outcome);

        // Settle positions before removing market
        if let Some(market) = self.markets.get(&event.event_id) {
            // Determine outcome: YES wins if final spot > strike
            let final_spot = self.spot_prices.get(&market.asset).copied().unwrap_or(Decimal::ZERO);
            let yes_wins = final_spot > market.strike_price;
            let asset = market.asset;

            let realized_pnl = self.executor.settle_market(&event.event_id, yes_wins).await;

            if realized_pnl != Decimal::ZERO {
                // Update global state with realized PnL (convert to cents)
                let pnl_cents = (realized_pnl * Decimal::new(100, 0)).to_i64().unwrap_or(0);
                self.state.metrics.add_pnl_cents(pnl_cents);

                info!(
                    event_id = %event.event_id,
                    asset = ?asset,
                    yes_wins = %yes_wins,
                    final_spot = %final_spot,
                    realized_pnl = %realized_pnl,
                    "Market settled at window close"
                );
            }
        }

        // Remove from tracked markets
        if let Some(market) = self.markets.remove(&event.event_id) {
            self.token_to_event.remove(&market.yes_token_id);
            self.token_to_event.remove(&market.no_token_id);
        }

        // Remove from global state (dashboard display)
        self.state.market_data.active_windows.remove(&event.event_id);
        self.state.market_data.remove_confidence(&event.event_id);
    }

    /// Update time remaining on all markets.
    fn update_all_market_times(&mut self, current_time: DateTime<Utc>) {
        // Update remaining markets
        for market in self.markets.values_mut() {
            market.update_time(current_time);
        }
    }

    /// Collect expired markets with settlement info.
    /// Returns Vec of (event_id, yes_token_id, no_token_id, yes_wins, asset).
    fn collect_expired_markets(&mut self, current_time: DateTime<Utc>) -> Vec<(String, String, String, bool, CryptoAsset)> {
        let expired: Vec<_> = self.markets
            .iter()
            .filter(|(_, m)| m.is_expired(current_time))
            .map(|(id, m)| {
                // For Up/Down markets: YES wins if final_spot > strike
                let final_spot = self.spot_prices.get(&m.asset).copied().unwrap_or(Decimal::ZERO);
                let yes_wins = final_spot > m.strike_price;
                (
                    id.clone(),
                    m.yes_token_id.clone(),
                    m.no_token_id.clone(),
                    yes_wins,
                    m.asset,
                )
            })
            .collect();

        // Remove expired markets from tracking
        for (id, yes_token_id, no_token_id, _, _) in &expired {
            self.markets.remove(id);
            self.token_to_event.remove(yes_token_id);
            self.token_to_event.remove(no_token_id);
            // Also remove from global state (dashboard display)
            self.state.market_data.active_windows.remove(id);
        }

        expired
    }

    /// Settle expired markets and update PnL.
    async fn settle_expired_markets(&mut self) {
        let expired = self.collect_expired_markets(self.simulated_time);

        for (event_id, _, _, yes_wins, asset) in expired {
            let realized_pnl = self.executor.settle_market(&event_id, yes_wins).await;

            if realized_pnl != Decimal::ZERO {
                // Update global state with realized PnL (convert to cents)
                let pnl_cents = (realized_pnl * Decimal::new(100, 0)).to_i64().unwrap_or(0);
                self.state.metrics.add_pnl_cents(pnl_cents);

                info!(
                    event_id = %event_id,
                    asset = ?asset,
                    yes_wins = %yes_wins,
                    realized_pnl = %realized_pnl,
                    "Market settled"
                );
            } else {
                debug!("Removed expired market: {} (no position)", event_id);
            }
        }
    }

    /// Check specific markets for opportunities using all enabled engines.
    ///
    /// This is a more efficient variant that only checks the specified markets,
    /// rather than iterating through all active markets.
    async fn check_opportunities_for_markets(
        &mut self,
        market_ids: &[String],
        event_latency_us: u64,
    ) -> Result<(), StrategyError> {
        // Fast path: check if trading is enabled (single atomic load)
        if !self.state.can_trade() {
            trace!("Trading disabled, skipping opportunity check");
            return Ok(());
        }

        // Periodic status log (every 15 seconds)
        if self.last_status_log.elapsed() > std::time::Duration::from_secs(15) {
            self.last_status_log = std::time::Instant::now();
            self.log_market_status(self.simulated_time);
        }

        // Calculate total exposure once
        let total_exposure = self.state.market_data.total_exposure();

        // Check only specified markets (clone to Vec to match check_opportunities)
        let event_ids: Vec<String> = market_ids.to_vec();

        for event_id in event_ids {
            // === Phase 1: Gather market state ===
            let market = match self.markets.get_mut(&event_id) {
                Some(m) => m,
                None => continue,
            };

            // Update time
            market.update_time(self.simulated_time);

            // Skip expired markets
            if market.is_expired(self.simulated_time) {
                continue;
            }

            // Skip markets that haven't started yet (strike_price = 0)
            if market.strike_price.is_zero() {
                trace!("Skipping market {} - not started yet (strike=0)", event_id);
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
                        // Log signal detection at trace level (use debug or info temporarily for diagnosis)
                        let mins = Decimal::from(market_state.seconds_remaining) / dec!(60);
                        let thresholds = get_thresholds(mins);
                        trace!(
                            "🎯 Signal {} {}: {:?} dist={:.4}% | thresholds: lean={:.4}% strong={:.4}% | ratio=UP:{}/DOWN:{} | yes_ask={} no_ask={}",
                            event_id, asset, opp.signal, opp.distance * dec!(100),
                            thresholds.lean * dec!(100), thresholds.strong * dec!(100),
                            opp.up_ratio, opp.down_ratio, opp.yes_ask, opp.no_ask
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

            debug!(
                "🎲 Opportunity {} {}: engine={:?} has_arb={} has_dir={} has_maker={}",
                event_id, asset, aggregated.primary_engine(),
                aggregated.summary.has_arb, aggregated.summary.has_directional, aggregated.summary.has_maker
            );

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
                    let window_duration_secs = self.config.window_duration_secs;

                    // Debug: log confidence calculation for position manager
                    let phase = position::Phase::from_seconds(seconds_remaining);
                    let pm_conf = market.position_manager.calculate_confidence(
                        distance_dollars,
                        seconds_remaining,
                        window_duration_secs,
                    );
                    let atr = market.position_manager.atr();
                    let atr_mult = if atr > Decimal::ZERO { distance_dollars.abs() / atr } else { Decimal::ZERO };

                    // Calculate time and distance confidence components for detailed logging (trace level)
                    // These now use the parameterized formulas from PositionManager
                    let time_ratio = Decimal::new(seconds_remaining, 0) / Decimal::new(window_duration_secs, 0);
                    let time_conf = dec!(0.30) + dec!(0.70) * (Decimal::ONE - time_ratio); // default params
                    let dist_conf = (dec!(0.20) + dec!(0.50) * atr_mult).min(Decimal::ONE); // default params

                    // Calculate EV and min_edge for logging (matches position.rs logic)
                    let favorable_price = opp.favorable_price();
                    let max_edge_factor = self.engines_config.directional.max_edge_factor;
                    let (ev, min_edge) = if max_edge_factor > Decimal::ZERO {
                        // EV-based: EV = confidence - price, trade if EV >= min_edge
                        let time_factor = Decimal::new(seconds_remaining, 0) / Decimal::new(window_duration_secs, 0);
                        let edge = max_edge_factor * time_factor;
                        (pm_conf - favorable_price, edge)
                    } else {
                        // Legacy phase-based thresholds
                        (Decimal::ZERO, Decimal::ZERO)
                    };

                    trace!(
                        "📈 Confidence {} {}: dist${:.2} atr${:.2} atr_mult={:.2} | time_conf={:.2} dist_conf={:.2} | conf={:.2} EV={:.3} min_edge={:.3} ({})",
                        event_id, asset, distance_dollars.abs(), atr, atr_mult,
                        time_conf, dist_conf, pm_conf, ev, min_edge,
                        if max_edge_factor > Decimal::ZERO { "EV-based" } else { "phase-based" },
                    );

                    // Sync confidence data to GlobalState for dashboard display
                    let would_trade = ev >= min_edge;
                    let snapshot = crate::state::ConfidenceSnapshot::new(
                        event_id.clone(),
                        pm_conf,
                        time_conf,
                        dist_conf,
                        min_edge,
                        ev,
                        would_trade,
                        distance_dollars.abs(),
                        atr_mult,
                        favorable_price,
                    );
                    self.state.market_data.update_confidence(&event_id, snapshot);

                    // Check if we should trade based on EV-based threshold and budget
                    let trade_decision = market.should_trade(
                        distance_dollars,
                        seconds_remaining,
                        favorable_price,
                        max_edge_factor,
                        window_duration_secs,
                    );

                    let (total_size, pm_confidence, phase) = match trade_decision {
                        position::TradeDecision::Skip(reason) => {
                            // Log skips at info level for diagnosis
                            info!(
                                "⏭️ SKIP {} {} {:?}: signal={:?} dist=${:.2} atr_mult={:.2} conf={:.2} EV={:.3} min_edge={:.3} phase={} price={}",
                                event_id, asset, reason, opp.signal, distance_dollars.abs(), atr_mult,
                                pm_conf, ev, min_edge, phase, favorable_price
                            );
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

                    // Verify size is meaningful - account for the split ratio
                    // The dominant leg (larger ratio) must reach minimum order size of $1.00
                    let dominant_ratio = opp.up_ratio.max(opp.down_ratio);
                    let dominant_leg_size = total_size * dominant_ratio;
                    if dominant_leg_size < Decimal::ONE {
                        // Need at least $1.00 / dominant_ratio to place an order
                        let min_needed = Decimal::ONE / dominant_ratio;
                        debug!(
                            "Directional size too small for {}: ${:.2} (dominant leg ${:.2} < $1, need ${:.2} total)",
                            event_id, total_size, dominant_leg_size, min_needed
                        );
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
                    info!("🚀 EXECUTE {} {}: signal={:?} size=${:.2} phase={} conf={:.2} up_ratio={} down_ratio={}",
                        event_id, asset, opp.signal, total_size, phase, pm_confidence, opp.up_ratio, opp.down_ratio);

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
        if (yes_filled_size > Decimal::ZERO || no_filled_size > Decimal::ZERO)
            && let Some(market) = self.markets.get_mut(&opportunity.event_id)
        {
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

        // Update volume metrics
        let volume_cents = (sizing.expected_cost * Decimal::new(100, 0))
            .try_into()
            .unwrap_or(0u64);
        self.state.metrics.add_volume_cents(volume_cents);

        Ok(TradeAction::Execute)
    }

    /// Execute a directional trade based on signal.
    ///
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

        // Linear hedge sizing based on distance from strike
        // Instead of discrete buckets (Strong=82%, Lean=65%), use continuous function
        //
        // directional_confidence ranges from -1.0 (100% DOWN) to +1.0 (100% UP)
        // At 2x the strong threshold, we're at max confidence for that time bracket
        let thresholds = get_thresholds(opportunity.minutes_remaining);
        let max_distance = thresholds.strong * dec!(2); // 2x strong threshold = max confidence
        let directional_confidence = if max_distance > Decimal::ZERO {
            (opportunity.distance / max_distance).max(dec!(-1)).min(dec!(1))
        } else {
            Decimal::ZERO
        };

        // Linear ratio: 5% to 95% based on confidence
        // up_ratio = 0.5 + confidence * 0.45
        let up_ratio = dec!(0.5) + directional_confidence * dec!(0.45);
        let down_ratio = Decimal::ONE - up_ratio;

        let mut up_size = total_size * up_ratio;
        let mut down_size = total_size * down_ratio;

        // Calculate share counts to check for position flip
        let up_shares = if up_price > Decimal::ZERO { up_size / up_price } else { Decimal::ZERO };
        let down_shares = if down_price > Decimal::ZERO { down_size / down_price } else { Decimal::ZERO };

        // Prevent position flip: hedge shares must not exceed main shares
        if up_ratio > down_ratio {
            // Main direction is UP - ensure down_shares < up_shares
            if down_shares > up_shares && up_shares > Decimal::ZERO {
                let max_hedge_dollars = up_shares * down_price;
                down_size = max_hedge_dollars.min(down_size);
                debug!(
                    "Capped DOWN hedge to prevent flip: {} shares -> {} dollars",
                    up_shares, down_size
                );
            }
        } else if down_ratio > up_ratio {
            // Main direction is DOWN - ensure up_shares < down_shares
            if up_shares > down_shares && down_shares > Decimal::ZERO {
                let max_hedge_dollars = down_shares * up_price;
                up_size = max_hedge_dollars.min(up_size);
                debug!(
                    "Capped UP hedge to prevent flip: {} shares -> {} dollars",
                    down_shares, up_size
                );
            }
        }

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

        // Determine order type based on execution mode
        // Chasing is disabled in backtest mode (simulated executor fills immediately)
        let (order_type, use_chase) = match self.config.execution.execution_mode {
            crate::config::ExecutionMode::Limit => {
                let chase = self.config.execution.chase_enabled && !self.config.is_backtest;
                (OrderType::Gtc, chase)
            }
            crate::config::ExecutionMode::Market => (OrderType::Ioc, false),
        };

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
                order_type,
                timeout_ms: None,
                timestamp: Utc::now(),
            };

            // Use chaser for limit orders if chase is enabled
            // The other_leg_price is the NO price (for arb ceiling calculation)
            let other_leg_price = down_price;
            let up_result = if use_chase {
                match self.chaser.chase_order(&mut self.executor, up_order, other_leg_price).await {
                    Ok(chase_result) => {
                        if chase_result.success || chase_result.filled_size > Decimal::ZERO {
                            Ok(OrderResult::Filled(crate::executor::OrderFill {
                                request_id: format!("dir-{}-up", req_id_base),
                                order_id: "chased".to_string(),
                                size: chase_result.filled_size,
                                price: chase_result.avg_price,
                                fee: chase_result.total_fee,
                                timestamp: Utc::now(),
                            }))
                        } else {
                            Ok(OrderResult::Pending(crate::executor::PendingOrder {
                                request_id: format!("dir-{}-up", req_id_base),
                                order_id: "chase_failed".to_string(),
                                timestamp: Utc::now(),
                            }))
                        }
                    }
                    Err(e) => Err(e),
                }
            } else {
                self.executor.place_order(up_order).await
            };
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
                order_type,
                timeout_ms: None,
                timestamp: Utc::now(),
            };

            // Use chaser for limit orders if chase is enabled
            // The other_leg_price is the YES price (for arb ceiling calculation)
            let other_leg_price = up_price;
            let down_result = if use_chase {
                match self.chaser.chase_order(&mut self.executor, down_order, other_leg_price).await {
                    Ok(chase_result) => {
                        if chase_result.success || chase_result.filled_size > Decimal::ZERO {
                            Ok(OrderResult::Filled(crate::executor::OrderFill {
                                request_id: format!("dir-{}-down", req_id_base),
                                order_id: "chased".to_string(),
                                size: chase_result.filled_size,
                                price: chase_result.avg_price,
                                fee: chase_result.total_fee,
                                timestamp: Utc::now(),
                            }))
                        } else {
                            Ok(OrderResult::Pending(crate::executor::PendingOrder {
                                request_id: format!("dir-{}-down", req_id_base),
                                order_id: "chase_failed".to_string(),
                                timestamp: Utc::now(),
                            }))
                        }
                    }
                    Err(e) => Err(e),
                }
            } else {
                self.executor.place_order(down_order).await
            };
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
        if (up_filled_size > Decimal::ZERO || down_filled_size > Decimal::ZERO)
            && let Some(market) = self.markets.get_mut(&event_id)
        {
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

        // Record the trade in confidence sizer
        if total_volume > Decimal::ZERO {
            self.confidence_sizer.record_trade(total_volume);

            // Update volume metrics
            let volume_cents = (total_volume * Decimal::new(100, 0))
                .try_into()
                .unwrap_or(0u64);
            self.state.metrics.add_volume_cents(volume_cents);

            // Record successful trade for metrics tracking
            self.state.record_success();
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

    /// Get executor's available balance.
    pub fn available_balance(&self) -> Decimal {
        self.executor.available_balance()
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

        let market = TrackedMarket::new(&event, dec!(100), &StrategyConfig::default());
        assert!(market.active_maker_orders.is_empty());
    }
}
