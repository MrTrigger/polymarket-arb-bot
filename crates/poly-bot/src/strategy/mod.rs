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
pub mod directional;
pub mod maker;
pub mod pivots;
pub mod position;
pub mod pricing;
pub mod regime;
pub mod signal;
pub mod sizing;
pub mod toxic;
pub mod decision_log;
pub mod strike;
pub mod trades_log;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

use poly_common::types::{CryptoAsset, Outcome, Side};
use crate::executor::chase::ChaseConfig;
use crate::order_manager::{
    AsyncOrderManager, AsyncOrderManagerConfig, OrderIntent, OrderManagerTask, OrderUpdateEvent,
};

// ============================================================================
// Price Buffer — recent prices for exact T=0 lookups at window open
// ============================================================================

/// Ring buffer of recent (timestamp_ms, price) pairs per asset.
/// Used to look up the exact price at window open time (T=0) instead of
/// using whatever the current price happens to be when we process the event.
///
/// Capacity: 120 entries ≈ 2 minutes at 1/sec update rate.
struct PriceBuffer {
    entries: HashMap<CryptoAsset, VecDeque<(i64, Decimal)>>,
}

impl PriceBuffer {
    const CAPACITY: usize = 120;

    fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    /// Record a price update.
    fn record(&mut self, asset: CryptoAsset, timestamp_ms: i64, price: Decimal) {
        let buf = self.entries.entry(asset).or_insert_with(VecDeque::new);
        buf.push_back((timestamp_ms, price));
        while buf.len() > Self::CAPACITY {
            buf.pop_front();
        }
    }

    /// Max acceptable distance from target timestamp (2 seconds).
    /// Beyond this, the price is too stale to be considered "at T=0".
    const MAX_DELTA_MS: i64 = 2000;

    /// Look up the price closest to `target_ms`.
    /// Returns None if buffer is empty or closest entry is >2s from target.
    fn lookup(&self, asset: CryptoAsset, target_ms: i64) -> Option<Decimal> {
        let buf = self.entries.get(&asset)?;
        let (ts, price) = buf.iter()
            .min_by_key(|(ts, _)| (ts - target_ms).unsigned_abs())?;
        if (ts - target_ms).unsigned_abs() <= Self::MAX_DELTA_MS as u64 {
            Some(*price)
        } else {
            None
        }
    }
}

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
use crate::executor::chase::{ChaseStopReason, PriceChaser};
use crate::order_manager::{PendingOrderTracker, PendingOrderType};
use crate::state::{ActiveWindow, GlobalState};
use crate::types::{EngineType, Inventory, MarketState};

pub use arb::{ArbDetector, ArbOpportunity, ArbRejection, ArbThresholds};
pub use confidence::{
    Confidence, ConfidenceCalculator, ConfidenceFactors, MAX_MULTIPLIER, MIN_MULTIPLIER,
};
pub use signal::{
    calculate_distance, distance_bps, get_signal, get_thresholds, Signal, SignalThresholds,
};
pub use sizing::{
    create_sizer, PositionSizer, SizingAdjustments, SizingConfig, SizingInput,
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

use rust_decimal_macros::dec;

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
    /// Condition ID (for CTF contract interactions like redeeming).
    condition_id: String,
    /// Asset.
    asset: CryptoAsset,
    /// YES token ID.
    yes_token_id: String,
    /// NO token ID.
    no_token_id: String,
    /// Strike price.
    strike_price: Decimal,
    /// Window start time (for constructing Polymarket page URL at settlement).
    window_start: DateTime<Utc>,
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
    /// Last simulated timestamp when we placed an order (for cooldown).
    /// Uses DateTime<Utc> to work correctly in both live and backtest modes.
    last_order_timestamp: Option<DateTime<Utc>>,
    /// Phase-based position manager for this market.
    /// Controls when/how much to trade based on confidence and budget.
    position_manager: position::PositionManager,
    /// Minimum order size in shares (from CLOB API).
    /// For backtest, uses BACKTEST_MIN_ORDER_SIZE (5 shares).
    min_order_size: Decimal,
    /// Last signal at time of most recent trade (for trades log).
    last_signal: String,
    /// Last confidence at time of most recent trade (for trades log).
    last_confidence: Decimal,
    /// Peak confidence seen since first fill (for reactive hedge tiers).
    peak_confidence: Decimal,
    /// Which hedge tiers have been triggered (indexed 0-2).
    hedge_tiers_triggered: [bool; 3],
    /// Direction of the main position (true=UP/YES, false=DOWN/NO). Set on first fill.
    position_direction: Option<bool>,
    /// Chainlink strike price at window open (ground truth for Polymarket resolution).
    chainlink_strike: Option<Decimal>,
    /// Binance spot price at window open (reference for lead calculation).
    binance_at_open: Option<Decimal>,
}

impl TrackedMarket {
    /// Create a new TrackedMarket with specified min_order_size.
    fn new(
        event: &WindowOpenEvent,
        market_budget: Decimal,
        strategy_config: &StrategyConfig,
        min_order_size: Decimal,
    ) -> Self {
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
            base_order_size: strategy_config.sizing_config.base_order_size,
            atr,
            // Confidence calculation params
            time_conf_floor: strategy_config.time_conf_floor,
            dist_conf_floor: strategy_config.dist_conf_floor,
            dist_conf_per_atr: strategy_config.dist_conf_per_atr,
            ..Default::default()
        };

        Self {
            event_id: event.event_id.clone(),
            condition_id: event.condition_id.clone(),
            asset: event.asset,
            yes_token_id: event.yes_token_id.clone(),
            no_token_id: event.no_token_id.clone(),
            strike_price: event.strike_price,
            window_start: event.window_start,
            window_end: event.window_end,
            state,
            inventory: Inventory::new(event.event_id.clone()),
            yes_toxic_warning: None,
            no_toxic_warning: None,
            active_maker_orders: HashMap::new(),
            last_order_timestamp: None,
            position_manager: position::PositionManager::new(position_config),
            min_order_size,
            last_signal: String::new(),
            last_confidence: Decimal::ZERO,
            peak_confidence: Decimal::ZERO,
            hedge_tiers_triggered: [false; 3],
            position_direction: None,
            chainlink_strike: None,
            binance_at_open: None,
        }
    }

    /// Create a new TrackedMarket with default min_order_size for backtest.
    /// Uses 5 shares as the default (most common on Polymarket).
    fn new_for_backtest(
        event: &WindowOpenEvent,
        market_budget: Decimal,
        strategy_config: &StrategyConfig,
    ) -> Self {
        Self::new(event, market_budget, strategy_config, dec!(5))
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
    #[allow(dead_code)]
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

    /// Reserve budget before placing an order.
    fn reserve_budget(&mut self, size: Decimal, seconds_remaining: i64) {
        self.position_manager.reserve_budget(size, seconds_remaining);
    }

    /// Release a reservation when an order fails.
    fn release_reservation(&mut self, size: Decimal, seconds_remaining: i64) {
        self.position_manager.release_reservation(size, seconds_remaining);
    }

    /// Commit a reservation when an order fills.
    fn commit_reservation(
        &mut self,
        reserved_size: Decimal,
        filled_size: Decimal,
        up_amount: Decimal,
        down_amount: Decimal,
        seconds_remaining: i64,
    ) {
        self.position_manager.commit_reservation(reserved_size, filled_size, up_amount, down_amount, seconds_remaining);
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

    // Confidence calculation params
    /// Minimum time confidence at window start (default 0.30).
    /// Formula: time_conf = floor + (1 - floor) * (1 - time_ratio)
    pub time_conf_floor: Decimal,
    /// Minimum distance confidence for tiny moves (default 0.20).
    pub dist_conf_floor: Decimal,
    /// Confidence gained per ATR of movement (default 0.50).
    /// Formula: dist_conf = clamp(floor + per_atr * atr_multiple, floor, 1.0)
    pub dist_conf_per_atr: Decimal,

    // Edge requirement (quality filter)
    /// Maximum edge required at window start (decays to 0 at window end).
    /// Formula: min_edge = max_edge_factor * (time_remaining / window_duration)
    /// Trade if: EV = (confidence - price) >= min_edge
    pub max_edge_factor: Decimal,

    /// Execution configuration (order mode, chase settings).
    pub execution: crate::config::ExecutionConfig,

    /// Whether running in backtest mode (disables price chasing).
    pub is_backtest: bool,

    /// Whether we can fetch live data from Polymarket's web pages.
    /// True for live and paper modes, false for backtest.
    pub live_oracle: bool,

    /// Chainlink price staleness threshold in seconds.
    pub oracle_stale_threshold_secs: u64,
    /// Enable Binance lead confidence adjustment.
    pub binance_lead_enabled: bool,
    /// Confidence boost when Binance lead confirms signal direction.
    pub lead_confirm_boost: Decimal,
    /// Confidence cut when Binance lead opposes signal direction.
    pub lead_oppose_cut: Decimal,
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
            // Confidence calculation params
            time_conf_floor: dec!(0.30),     // 30% confidence at window start
            dist_conf_floor: dec!(0.15),     // 15% minimum for tiny moves (sweep optimal)
            dist_conf_per_atr: dec!(0.30),   // +30% per ATR of movement (sweep optimal)
            // Edge requirement (quality filter)
            max_edge_factor: dec!(0.20),     // 20% EV required at window start (decays to 0)
            execution: crate::config::ExecutionConfig::default(),
            is_backtest: false,
            live_oracle: true,
            oracle_stale_threshold_secs: 30,
            binance_lead_enabled: true,
            lead_confirm_boost: dec!(0.15),
            lead_oppose_cut: dec!(0.25),
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
            // Confidence calculation params
            time_conf_floor: dec!(0.30),
            dist_conf_floor: dec!(0.15),
            dist_conf_per_atr: dec!(0.30),
            // Edge requirement (quality filter)
            max_edge_factor: dec!(0.20),
            execution: crate::config::ExecutionConfig::default(),
            is_backtest: false,
            live_oracle: true,
            oracle_stale_threshold_secs: 30,
            binance_lead_enabled: true,
            lead_confirm_boost: dec!(0.15),
            lead_oppose_cut: dec!(0.25),
        }
    }

    /// Set backtest mode (disables price chasing, disables live oracle by default).
    pub fn with_backtest(mut self, is_backtest: bool) -> Self {
        self.is_backtest = is_backtest;
        if is_backtest {
            self.live_oracle = false;
        }
        self
    }

    /// Set whether to fetch strike/settlement prices from Polymarket's page.
    pub fn with_live_oracle(mut self, live_oracle: bool) -> Self {
        self.live_oracle = live_oracle;
        self
    }

    /// Set the window duration on an existing config.
    pub fn with_window_duration(mut self, window_duration_secs: i64) -> Self {
        self.window_duration_secs = window_duration_secs;
        self
    }

    /// Returns the slug for Polymarket page URLs (e.g., "15m").
    pub fn duration_slug(&self) -> &'static str {
        match self.window_duration_secs {
            300 => "5m",
            900 => "15m",
            3600 => "1h",
            _ => "15m",
        }
    }

    /// Set the execution configuration.
    pub fn with_execution(mut self, execution: crate::config::ExecutionConfig) -> Self {
        self.execution = execution;
        self
    }

    /// Apply oracle configuration.
    pub fn with_oracle(mut self, oracle: &crate::config::OracleConfig) -> Self {
        self.oracle_stale_threshold_secs = oracle.chainlink_stale_threshold_secs;
        self.binance_lead_enabled = oracle.binance_lead_enabled;
        self.lead_confirm_boost = oracle.lead_confirm_boost;
        self.lead_oppose_cut = oracle.lead_oppose_cut;
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
    /// Tracked markets by event_id.
    markets: HashMap<String, TrackedMarket>,
    /// Token ID to event ID mapping.
    token_to_event: HashMap<String, String>,
    /// Latest spot prices by asset.
    spot_prices: HashMap<CryptoAsset, Decimal>,
    /// Latest Chainlink oracle prices by asset.
    chainlink_prices: HashMap<CryptoAsset, Decimal>,
    /// Last Chainlink price update timestamp by asset.
    chainlink_last_update: HashMap<CryptoAsset, DateTime<Utc>>,
    /// Recent Chainlink prices for exact T=0 lookups at window open.
    chainlink_buffer: PriceBuffer,
    /// Recent Binance spot prices for exact T=0 lookups at window open.
    binance_buffer: PriceBuffer,
    /// Windows waiting for Chainlink price before opening.
    /// Decision counter for unique IDs.
    decision_counter: u64,
    /// Observability channel sender (optional).
    obs_sender: Option<mpsc::Sender<TradeDecision>>,
    /// Multi-engine observability channel sender (optional).
    multi_obs_sender: Option<mpsc::Sender<MultiEngineDecision>>,
    /// Last status log time for periodic logging.
    last_status_log: std::time::Instant,
    /// Last skip log time for rate-limited skip logging.
    last_skip_log: std::time::Instant,
    /// Dynamic ATR tracker for volatility-adjusted position sizing.
    atr_tracker: atr::AtrTracker,
    /// Market regime detector for adaptive parameters.
    regime_detector: regime::RegimeDetector,
    /// Pivot tracker for support/resistance based confidence adjustment.
    pivot_tracker: pivots::PivotTracker,
    /// Simulated time for backtest mode (uses event timestamps instead of wall-clock time).
    simulated_time: DateTime<Utc>,
    /// Price chaser for maker execution with fill polling (legacy - used when async order manager is not enabled).
    chaser: PriceChaser,
    /// Centralized pending order tracker (replaces per-market boolean flags).
    pending_tracker: PendingOrderTracker,
    /// Async order manager for non-blocking order execution (optional).
    /// When Some, directional orders are submitted via channels and chasing runs in background.
    async_order_manager: Option<AsyncOrderManager>,
    /// Receiver for order updates from async order manager.
    order_update_rx: Option<mpsc::Receiver<OrderUpdateEvent>>,
    /// Handle to the order manager background task.
    order_manager_task: Option<tokio::task::JoinHandle<()>>,
    /// Decision logger for comparing live vs backtest behavior.
    decision_logger: Option<decision_log::DecisionLogger>,
    /// Trades logger for recording all fills and settlements.
    trades_logger: Option<trades_log::TradesLogger>,
    /// First event timestamp seen (for backtest reporting).
    first_event_time: Option<DateTime<Utc>>,
    /// Last event timestamp seen (for backtest reporting).
    last_event_time: Option<DateTime<Utc>>,
    /// Market client for fetching min_order_size from CLOB API (live only).
    market_client: Option<crate::api::MarketClient>,
    /// HTTP client for fetching strike prices from Polymarket pages.
    http_client: reqwest::Client,
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
        // Create directional detector with config from engines_config and strategy config
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

        // Extract chase config values before moving config into struct
        let chase_enabled = config.execution.chase_enabled && !config.is_backtest;
        let chase_step_size = config.execution.chase_step_size;
        let chase_check_interval_ms = config.execution.chase_check_interval_ms;
        let max_chase_time_ms = config.execution.max_chase_time_ms;

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
            markets: HashMap::new(),
            token_to_event: HashMap::new(),
            spot_prices: HashMap::new(),
            chainlink_prices: HashMap::new(),
            chainlink_last_update: HashMap::new(),
            chainlink_buffer: PriceBuffer::new(),
            binance_buffer: PriceBuffer::new(),
            decision_counter: 0,
            obs_sender: None,
            multi_obs_sender: None,
            last_status_log: std::time::Instant::now(),
            last_skip_log: std::time::Instant::now(),
            atr_tracker: atr::AtrTracker::default(),
            regime_detector: regime::RegimeDetector::default(),
            pivot_tracker: pivots::PivotTracker::default(),
            simulated_time: Utc::now(),
            chaser: PriceChaser::new(ChaseConfig::from_execution_config(
                chase_enabled,
                chase_step_size,
                chase_check_interval_ms,
                max_chase_time_ms,
                dec!(0.005), // 0.5% minimum margin
            )),
            pending_tracker: PendingOrderTracker::new(),
            async_order_manager: None,
            order_update_rx: None,
            order_manager_task: None,
            decision_logger: None,
            trades_logger: None,
            first_event_time: None,
            last_event_time: None,
            market_client: None,
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
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

    /// Set market client for fetching min_order_size from CLOB API (live trading).
    pub fn with_market_client(mut self, client: crate::api::MarketClient) -> Self {
        self.market_client = Some(client);
        self
    }

    /// Enable decision logging for comparing live vs backtest behavior.
    ///
    /// Writes all trading decisions (skip and execute) to a CSV file.
    pub fn with_decision_logger(mut self, logger: decision_log::DecisionLogger) -> Self {
        self.decision_logger = Some(logger);
        self
    }

    /// Enable trades logging for recording all fills and settlements.
    pub fn with_trades_logger(mut self, logger: trades_log::TradesLogger) -> Self {
        self.trades_logger = Some(logger);
        self
    }

    /// Enable async order management with a separate executor for the order manager task.
    ///
    /// When enabled, directional orders are submitted to a background task that handles
    /// price chasing without blocking the main event loop. Fill notifications are received
    /// via channel and processed in the main loop.
    ///
    /// **Important**: The `order_executor` should be a separate instance from the strategy's
    /// executor. For live trading, create two LiveExecutor instances. For paper/backtest mode,
    /// async order management is typically not needed.
    ///
    /// # Arguments
    /// * `order_executor` - Executor instance for the order manager task to use
    /// * `config` - Configuration for the async order manager
    pub fn with_async_order_manager<OE: Executor + Send + Sync + 'static>(
        mut self,
        order_executor: OE,
        config: AsyncOrderManagerConfig,
    ) -> Self {
        let (order_manager, order_update_rx, task_handle) = OrderManagerTask::spawn(
            order_executor,
            config,
            self.state.clone(),
        );

        self.async_order_manager = Some(order_manager);
        self.order_update_rx = Some(order_update_rx);
        self.order_manager_task = Some(task_handle);

        info!("Async order management enabled - chasing will run in background task");
        self
    }

    /// Check if async order management is enabled.
    pub fn has_async_order_manager(&self) -> bool {
        self.async_order_manager.is_some()
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
    ///
    /// When async order management is enabled, this loop uses `tokio::select!` to
    /// concurrently process market data events and order fill notifications.
    pub async fn run(&mut self) -> Result<(), StrategyError> {
        info!("Strategy loop starting (async_order_manager={})", self.async_order_manager.is_some());

        // Create a shutdown check interval (100ms) to ensure Ctrl+C responsiveness
        let mut shutdown_check = tokio::time::interval(std::time::Duration::from_millis(100));
        shutdown_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // Check for shutdown
            if self.state.control.is_shutdown_requested() {
                info!("Shutdown requested, stopping strategy loop");
                // Shutdown async order manager if present
                if let Some(ref manager) = self.async_order_manager {
                    manager.shutdown().await;
                }
                return Err(StrategyError::Shutdown);
            }

            // Use select! to handle both data events and order updates concurrently
            tokio::select! {
                biased;

                // Highest priority: Check for shutdown signal periodically
                _ = shutdown_check.tick() => {
                    // Just loop back to check is_shutdown_requested() at top of loop
                    continue;
                }

                // High priority: Process order updates from async order manager
                order_update = async {
                    match &mut self.order_update_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<OrderUpdateEvent>>().await,
                    }
                } => {
                    if let Some(update) = order_update {
                        self.handle_order_update(update);
                    } else {
                        // Order manager channel closed - this shouldn't happen normally
                        warn!("Order update channel closed unexpectedly");
                        self.order_update_rx = None;
                    }
                }

                // Process market data events
                event_result = self.data_source.next_event() => {
                    match event_result {
                        Ok(Some(event)) => {
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
                        Ok(None) => {
                            debug!("Data source exhausted");
                            break;
                        }
                        Err(DataSourceError::Shutdown) => {
                            info!("Data source shutdown");
                            if let Some(ref manager) = self.async_order_manager {
                                manager.shutdown().await;
                            }
                            return Err(StrategyError::Shutdown);
                        }
                        Err(e) => {
                            warn!("Data source error: {}", e);
                            continue;
                        }
                    }
                }
            }
        }

        // Shutdown async order manager cleanly
        if let Some(ref manager) = self.async_order_manager {
            manager.shutdown().await;
        }

        Ok(())
    }

    /// Handle an order update from the async order manager.
    fn handle_order_update(&mut self, update: OrderUpdateEvent) {
        match update {
            OrderUpdateEvent::Fill {
                handle_id,
                market_id,
                token_id,
                outcome,
                side,
                filled_size,
                avg_price,
                fee,
                is_complete,
            } => {
                info!(
                    handle_id = %handle_id,
                    market_id = %market_id,
                    token_id = %token_id,
                    outcome = ?outcome,
                    side = ?side,
                    filled_size = %filled_size,
                    avg_price = %avg_price,
                    fee = %fee,
                    complete = is_complete,
                    "Order filled (async)"
                );

                // Update inventory in global state
                if let Some(mut inv) = self.state.market_data.inventory.get_mut(&market_id) {
                    let position_delta = match (outcome, side) {
                        (Outcome::Yes, Side::Buy) => filled_size,
                        (Outcome::Yes, Side::Sell) => -filled_size,
                        (Outcome::No, Side::Buy) => filled_size,
                        (Outcome::No, Side::Sell) => -filled_size,
                    };
                    let cost_delta = avg_price * filled_size + fee;
                    match outcome {
                        Outcome::Yes => {
                            inv.yes_shares += position_delta;
                            inv.yes_cost_basis += cost_delta;
                        }
                        Outcome::No => {
                            inv.no_shares += position_delta;
                            inv.no_cost_basis += cost_delta;
                        }
                    }
                }

                // Clear pending tracker if complete
                if is_complete {
                    self.pending_tracker.clear(&market_id, PendingOrderType::Directional);
                }
            }

            OrderUpdateEvent::Rejected { handle_id, market_id, reason } => {
                warn!(
                    handle_id = %handle_id,
                    market_id = %market_id,
                    reason = %reason,
                    "Order rejected (async)"
                );
                self.pending_tracker.clear(&market_id, PendingOrderType::Directional);
            }

            OrderUpdateEvent::Cancelled { handle_id, market_id } => {
                debug!(
                    handle_id = %handle_id,
                    market_id = %market_id,
                    "Order cancelled (async)"
                );
                self.pending_tracker.clear(&market_id, PendingOrderType::Directional);
            }

            OrderUpdateEvent::ChaseEnded { handle_id, market_id, reason, filled_size, avg_price } => {
                info!(
                    handle_id = %handle_id,
                    market_id = %market_id,
                    reason = ?reason,
                    filled_size = %filled_size,
                    avg_price = %avg_price,
                    "Chase ended (async)"
                );
                self.pending_tracker.clear(&market_id, PendingOrderType::Directional);
            }
        }
    }

    /// Process a single market event.
    async fn process_event(&mut self, event: MarketEvent) -> Result<(), StrategyError> {
        let event_time = std::time::Instant::now();

        // Update simulated time from event timestamp (for backtest mode)
        self.simulated_time = event.timestamp();

        // Advance executor's simulated clock and process any deferred fills that have matured
        self.executor.set_simulated_time(self.simulated_time);
        let deferred_fills = self.executor.process_pending_fills().await;
        for fill in deferred_fills {
            self.handle_fill(fill).await?;
        }

        // Track event time range for backtest reporting
        let ts = event.timestamp();
        if self.first_event_time.is_none() {
            self.first_event_time = Some(ts);
        }
        self.last_event_time = Some(ts);

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
            MarketEvent::BookSnapshotPair(e) => {
                // Process both YES and NO snapshots atomically
                let event_id = e.event_id.clone();
                self.handle_book_snapshot(e.yes_snapshot).await;
                self.handle_book_snapshot(e.no_snapshot).await;
                vec![event_id]
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
                // Strike comes from Gamma API — open immediately, no deferral needed.
                // Chainlink is used for settlement (at window end) and signal detection.
                let event_id = e.event_id.clone();
                self.handle_window_open(e).await;
                vec![event_id]
            }
            MarketEvent::WindowClose(e) => {
                self.handle_window_close(e).await;
                vec![] // Market is closed, no opportunities
            }
            MarketEvent::ChainlinkPrice(e) => {
                let asset = e.asset;
                self.handle_chainlink_price(e);

                // Return affected tracked markets for opportunity checks
                let affected: Vec<String> = self.markets.iter()
                    .filter(|(_, m)| m.asset == asset)
                    .map(|(id, _)| id.clone())
                    .collect();
                affected
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
        debug!("📈 Spot price: {} @ {}", event.asset, event.price);

        // Update local cache
        self.spot_prices.insert(event.asset, event.price);

        // Record in buffer for exact T=0 lookups at window open
        self.binance_buffer.record(event.asset, event.timestamp.timestamp_millis(), event.price);

        // Record price for dynamic ATR calculation (use event timestamp, not wall clock)
        self.atr_tracker.record_price(event.asset, event.price, event.timestamp.timestamp_millis());

        // Update pivot tracker and regime detector with current ATR
        let current_atr = self.atr_tracker.get_atr(event.asset);
        self.pivot_tracker.set_atr(event.asset, current_atr);
        self.pivot_tracker.record_price(event.asset, event.price, event.timestamp.timestamp());
        self.regime_detector.update_atr(event.asset, current_atr);

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

                // strike=0 should never happen — handle_window_open rejects it
                if market.state.strike_price.is_zero() {
                    error!("BUG: market {} has strike=0 but was not rejected at window open",
                        market.event_id);
                    continue;
                }

                // Sync confidence data for dashboard display
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

                    // Use best ask as favorable price estimate (conservative)
                    let favorable_price = if event.price > market.strike_price {
                        // Price above strike - YES is favorable
                        market.state.yes_book.best_ask().unwrap_or(dec!(0.50))
                    } else {
                        // Price below strike - NO is favorable
                        market.state.no_book.best_ask().unwrap_or(dec!(0.50))
                    };

                    // EV-based decision with time-decaying edge requirement
                    let ev = pm_conf - favorable_price;
                    let time_factor = Decimal::new(seconds_remaining.max(0), 0) / Decimal::new(window_duration_secs, 0);
                    let min_edge = self.config.max_edge_factor * time_factor;
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

    /// Handle a Chainlink oracle price update from Polymarket RTDS.
    fn handle_chainlink_price(&mut self, event: crate::data_source::ChainlinkPriceEvent) {
        trace!("🔗 Chainlink price: {} @ {}", event.asset, event.price);

        // Update local cache
        self.chainlink_prices.insert(event.asset, event.price);
        self.chainlink_last_update.insert(event.asset, event.timestamp);

        // Record in buffer for exact T=0 lookups at window open
        self.chainlink_buffer.record(event.asset, event.timestamp.timestamp_millis(), event.price);

        // Update all markets for this asset
        for market in self.markets.values_mut() {
            if market.asset == event.asset {
                market.state.chainlink_price = Some(event.price);
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
        let event_id = self.token_to_event.get(&event.token_id)?.clone();

        let market = self.markets.get_mut(&event_id)?;

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

            // Log fill to trades log
            if let Some(ref logger) = self.trades_logger {
                let spot = self.spot_prices.get(&market.asset).copied().unwrap_or(Decimal::ZERO);
                let outcome_str = match event.outcome { Outcome::Yes => "YES", Outcome::No => "NO" };
                logger.log_fill(
                    event.timestamp,
                    &event.event_id, market.asset, outcome_str,
                    event.size, event.price, event.fee, cost,
                    market.strike_price, spot,
                    &market.last_signal, market.last_confidence,
                    self.executor.available_balance(),
                    market.inventory.yes_shares, market.inventory.no_shares,
                    market.inventory.yes_cost_basis + market.inventory.no_cost_basis,
                );
            }

            // CRITICAL: Update position manager budget to prevent over-trading.
            // Without this, fills via WebSocket don't reduce the budget, allowing
            // unlimited orders until the account is empty.
            let seconds_remaining = market.state.seconds_remaining;
            let (up_cost, down_cost) = match event.outcome {
                Outcome::Yes => (cost, Decimal::ZERO),
                Outcome::No => (Decimal::ZERO, cost),
            };
            market.record_trade(cost, up_cost, down_cost, seconds_remaining);

            // Clear pending order tracking on fill
            // Once a fill arrives, has_position() will also return true,
            // but clear these for completeness
            self.pending_tracker.clear_all(&event.event_id);

            // Update global state inventory
            let pos = crate::state::InventoryPosition {
                event_id: event.event_id.clone(),
                condition_id: market.condition_id.clone(),
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
    ///
    /// Strike priority:
    /// 1. Chainlink RTDS price (what Polymarket actually uses as strike)
    /// 2. Binance historical price from discovery (approximate, ~0.2% off)
    ///
    /// 15M market titles are "Bitcoin Up or Down - February 10, 12:00PM-12:15PM ET"
    /// with no dollar amount, so parse_strike_price() always returns 0.
    /// Discovery fills in strike from Binance when the window becomes active.
    /// We override with Chainlink here since that's the actual oracle.
    async fn handle_window_open(&mut self, mut event: WindowOpenEvent) {
        let window_start_ms = event.window_start.timestamp_millis();

        // Look up prices at exactly T=0 (window start) from ring buffers.
        // Returns None if we don't have data near T=0 (e.g., bot just started).
        let chainlink_at_t0 = self.chainlink_buffer.lookup(event.asset, window_start_ms);
        let binance_at_t0 = self.binance_buffer.lookup(event.asset, window_start_ms);

        // Strike source priority:
        // 1. Chainlink RTDS at T=0 (identical to Polymarket's openPrice — confirmed empirically)
        // 2. Fetch from Polymarket page (exact "price to beat", but flaky SSR cache)
        // 3. Backtest: replay data
        if self.config.live_oracle {
            if let Some(cl_strike) = chainlink_at_t0 {
                event.strike_price = cl_strike;
                // Verify against page if possible (background check, non-blocking for now)
                info!("Window open: {} {} strike={} (chainlink T=0)",
                    event.event_id, event.asset, event.strike_price);
            } else if let Some(pm_strike) = strike::fetch_polymarket_strike(
                &self.http_client, event.asset, event.window_start, self.config.duration_slug()
            ).await {
                event.strike_price = pm_strike;
                info!("Window open: {} {} strike={} (polymarket page fallback)",
                    event.event_id, event.asset, event.strike_price);
            } else {
                warn!("Rejecting window open {} {}: no strike source available",
                    event.event_id, event.asset);
                return;
            }
        } else {
            // Backtest: use strike from replay data
            if event.strike_price.is_zero() {
                warn!("Rejecting window open {} {}: strike is zero in replay data",
                    event.event_id, event.asset);
                return;
            }
            info!("Window open: {} {} strike={} (replay)",
                event.event_id, event.asset, event.strike_price);
        }

        // Calculate budget per market from sizing config
        let market_budget = self.config.sizing_config.max_market_exposure;

        // Create tracked market with phase-based position management
        // Pass strategy config so confidence params flow through to PositionManager
        let market = if self.config.is_backtest {
            TrackedMarket::new_for_backtest(&event, market_budget, &self.config)
        } else {
            // Fetch min_order_size from CLOB API (falls back to 5 on error)
            let min_order_size = if let Some(ref client) = self.market_client {
                match client.get_min_order_size(&event.condition_id).await {
                    Ok(size) => {
                        info!("Fetched min_order_size={} for {}", size, event.event_id);
                        size
                    }
                    Err(e) => {
                        warn!("Failed to fetch min_order_size for {}: {}, using default 5", event.event_id, e);
                        dec!(5)
                    }
                }
            } else {
                dec!(5)
            };
            TrackedMarket::new(&event, market_budget, &self.config, min_order_size)
        };

        // Record Chainlink and Binance snapshots at T=0 for lead calculation.
        // chainlink_strike = Chainlink price at window open (ground truth for settlement)
        // binance_at_open = Binance price at window open (baseline for lead = binance_delta - chainlink_delta)
        let mut market = market;
        market.chainlink_strike = chainlink_at_t0;
        market.state.chainlink_strike = chainlink_at_t0;
        market.binance_at_open = binance_at_t0;
        market.state.binance_at_open = binance_at_t0;

        // Update token mapping
        self.token_to_event.insert(event.yes_token_id.clone(), event.event_id.clone());
        self.token_to_event.insert(event.no_token_id.clone(), event.event_id.clone());

        // Add to tracked markets
        self.markets.insert(event.event_id.clone(), market);

        // Sync to global state for dashboard display
        let active_window = ActiveWindow {
            event_id: event.event_id.clone(),
            condition_id: event.condition_id,
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
            let asset = market.asset;
            let strike_price = market.strike_price;
            let yes_shares = market.inventory.yes_shares;
            let no_shares = market.inventory.no_shares;
            let cost_basis = market.inventory.yes_cost_basis + market.inventory.no_cost_basis;

            // Settlement: use Chainlink RTDS price (same oracle Polymarket uses).
            // Page closePrice is never populated in time (~2+ min delay), so we use
            // the latest Chainlink price which is sampled from the same Data Streams feed.
            let final_spot = self.chainlink_prices.get(&asset).copied()
                .or_else(|| self.spot_prices.get(&asset).copied())
                .unwrap_or(Decimal::ZERO);
            let yes_wins = final_spot >= strike_price;

            let payout = if yes_wins { yes_shares } else { no_shares };
            let realized_pnl = payout - cost_basis;

            let _ = self.executor.settle_market(&event.event_id, yes_wins).await;

            if cost_basis != Decimal::ZERO {
                let pnl_cents = (realized_pnl * Decimal::new(100, 0)).to_i64().unwrap_or(0);
                self.state.metrics.add_pnl_cents(pnl_cents);

                info!(
                    event_id = %event.event_id,
                    asset = ?asset,
                    yes_wins = %yes_wins,
                    final_spot = %final_spot,
                    payout = %payout,
                    cost_basis = %cost_basis,
                    realized_pnl = %realized_pnl,
                    "Market settled at window close"
                );
            }

            if let Some(ref logger) = self.trades_logger {
                logger.log_settlement(
                    Utc::now(), &event.event_id, asset, yes_wins,
                    strike_price, final_spot,
                    yes_shares, no_shares, cost_basis,
                    payout, realized_pnl,
                    self.executor.available_balance(),
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

    /// Collect expired market data for settlement.
    /// Returns raw data; settlement decision is made in `settle_expired_markets`.
    #[allow(clippy::type_complexity)]
    fn collect_expired_markets(&mut self, current_time: DateTime<Utc>) -> Vec<(String, CryptoAsset, Decimal, Decimal, Decimal, Decimal)> {
        let expired: Vec<_> = self.markets
            .iter()
            .filter(|(_, m)| m.is_expired(current_time))
            .map(|(id, m)| (
                id.clone(),
                m.asset,
                m.strike_price,
                m.inventory.yes_shares,
                m.inventory.no_shares,
                m.inventory.yes_cost_basis + m.inventory.no_cost_basis,
            ))
            .collect();

        // Remove expired markets from tracking
        for (id, _, _, _, _, _) in &expired {
            if let Some(m) = self.markets.remove(id) {
                self.token_to_event.remove(&m.yes_token_id);
                self.token_to_event.remove(&m.no_token_id);
            }
            self.state.market_data.active_windows.remove(id);
            self.state.market_data.remove_inventory(id);
        }

        expired
    }

    /// Settle expired markets and update PnL.
    async fn settle_expired_markets(&mut self) {
        let expired = self.collect_expired_markets(self.simulated_time);

        for (event_id, asset, strike_price, yes_shares, no_shares, cost_basis) in expired {
            // Settlement: use Chainlink RTDS price (same oracle Polymarket uses).
            let final_spot = self.chainlink_prices.get(&asset).copied()
                .or_else(|| self.spot_prices.get(&asset).copied())
                .unwrap_or(Decimal::ZERO);
            let yes_wins = final_spot >= strike_price;

            let payout = if yes_wins { yes_shares } else { no_shares };
            let realized_pnl = payout - cost_basis;

            let _ = self.executor.settle_market(&event_id, yes_wins).await;

            if cost_basis != Decimal::ZERO {
                let pnl_cents = (realized_pnl * Decimal::new(100, 0)).to_i64().unwrap_or(0);
                self.state.metrics.add_pnl_cents(pnl_cents);

                info!(
                    event_id = %event_id,
                    asset = ?asset,
                    yes_wins = %yes_wins,
                    payout = %payout,
                    cost_basis = %cost_basis,
                    realized_pnl = %realized_pnl,
                    "Market settled"
                );
            } else {
                debug!("Removed expired market: {} (no position)", event_id);
            }

            if let Some(ref logger) = self.trades_logger {
                logger.log_settlement(
                    self.simulated_time, &event_id, asset, yes_wins,
                    strike_price, final_spot,
                    yes_shares, no_shares, cost_basis,
                    payout, realized_pnl,
                    self.executor.available_balance(),
                );
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

        // Calculate total exposure from executor (single source of truth)
        let total_exposure = self.executor.total_exposure();

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

            // Update Chainlink price in market state if we have it
            if let Some(&cl_price) = self.chainlink_prices.get(&market.state.asset) {
                market.state.chainlink_price = Some(cl_price);
            }
            // Propagate Chainlink strike from TrackedMarket to MarketState
            market.state.chainlink_strike = market.chainlink_strike;

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
                            "🎯 Signal {} {}: {:?} dist={:.4}% | thresholds: lean={:.4}% strong={:.4}% | yes_ask={} no_ask={}",
                            event_id, asset, opp.signal, opp.distance * dec!(100),
                            thresholds.lean * dec!(100), thresholds.strong * dec!(100),
                            opp.yes_ask, opp.no_ask
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
                debug!(
                    "⚪ No signal for {}: dist too small or no clear direction",
                    event_id
                );
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

                    // For arb: check if we already have a position or pending order.
                    // Arb locks in profit regardless of outcome, so we shouldn't
                    // keep adding once we're already positioned or have an order in flight.
                    if market.has_position() {
                        trace!(
                            "Already have arb position for {}, skipping",
                            event_id
                        );
                        continue; // Silent skip - already have position
                    }
                    if self.pending_tracker.is_pending(&event_id, PendingOrderType::Arbitrage) {
                        trace!(
                            "Already have pending arb order for {}, skipping",
                            event_id
                        );
                        continue; // Silent skip - order already in flight
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

                    // Check if we already have a pending directional order
                    if self.pending_tracker.is_pending(&event_id, PendingOrderType::Directional) {
                        trace!(
                            "Already have pending directional order for {}, skipping",
                            event_id
                        );
                        continue; // Silent skip - order already in flight
                    }

                    // Cooldown check: don't spam orders (wait between attempts)
                    // Uses simulated time (window_end - seconds_remaining) to work in both live and backtest.
                    // Applied for ALL execution modes - in live, network latency naturally throttles,
                    // but in backtest the simulated executor fills instantly, so without a cooldown
                    // the backtest fires 20+ trades per event per phase (unrealistic).
                    {
                        const ORDER_COOLDOWN_SECS: i64 = 5;
                        let current_time = market.window_end - chrono::Duration::seconds(market.state.seconds_remaining);
                        if let Some(last_timestamp) = market.last_order_timestamp {
                            let elapsed = (current_time - last_timestamp).num_seconds();
                            if elapsed < ORDER_COOLDOWN_SECS {
                                trace!(
                                    "Cooldown active for {}: {}s since last order (need {}s)",
                                    event_id,
                                    elapsed,
                                    ORDER_COOLDOWN_SECS
                                );
                                continue;
                            }
                        }
                    }

                    // Calculate distance in dollars for position manager
                    let distance_dollars = opp.distance * market.strike_price;
                    let seconds_remaining = market.state.seconds_remaining;
                    let window_duration_secs = self.config.window_duration_secs;

                    // Debug: log confidence calculation for position manager
                    let phase = position::Phase::from_seconds(seconds_remaining);
                    let base_conf = market.position_manager.calculate_confidence(
                        distance_dollars,
                        seconds_remaining,
                        window_duration_secs,
                    );
                    let atr = market.position_manager.atr();
                    let atr_mult = if atr > Decimal::ZERO { distance_dollars.abs() / atr } else { Decimal::ZERO };

                    // Apply pivot-based confidence adjustment
                    // Determines if current price is near S/R levels and adjusts confidence accordingly
                    let favor_up = matches!(opp.signal, signal::Signal::StrongUp | signal::Signal::LeanUp);
                    let spot_price = market.state.spot_price.unwrap_or(market.strike_price);
                    let pivot_conf = self.pivot_tracker.calculate_confidence(asset, spot_price, favor_up);
                    let pm_conf = pivot_conf.apply(base_conf);

                    // Log pivot adjustment if significant
                    if (pivot_conf.multiplier - Decimal::ONE).abs() > dec!(0.05) {
                        trace!(
                            "🎯 Pivot adj {} {}: regime={} S={} R={} pos={:.0}% mult={:.2} base_conf={:.2} -> {:.2}",
                            event_id, asset, pivot_conf.regime,
                            pivot_conf.support.map(|s| format!("{:.0}", s)).unwrap_or("-".into()),
                            pivot_conf.resistance.map(|r| format!("{:.0}", r)).unwrap_or("-".into()),
                            pivot_conf.range_position.unwrap_or(dec!(-1)) * dec!(100),
                            pivot_conf.multiplier, base_conf, pm_conf
                        );
                    }

                    // Binance lead confidence adjustment
                    // When Binance has moved further than Chainlink in the signal direction,
                    // Chainlink will likely catch up → boost confidence
                    let pm_conf = if self.config.binance_lead_enabled {
                        if let Some(lead) = opp.binance_lead {
                            let signal_is_up = matches!(opp.signal, signal::Signal::StrongUp | signal::Signal::LeanUp);
                            let lead_confirms = (signal_is_up && lead > Decimal::ZERO) || (!signal_is_up && lead < Decimal::ZERO);
                            if lead_confirms {
                                let boosted = pm_conf * (Decimal::ONE + self.config.lead_confirm_boost);
                                trace!("🔗 Binance lead={:.4} confirms {} → boost conf {:.3} → {:.3}",
                                    lead, opp.signal, pm_conf, boosted);
                                boosted.min(Decimal::ONE)
                            } else {
                                let cut = pm_conf * (Decimal::ONE - self.config.lead_oppose_cut);
                                trace!("🔗 Binance lead={:.4} opposes {} → cut conf {:.3} → {:.3}",
                                    lead, opp.signal, pm_conf, cut);
                                cut.max(Decimal::ZERO)
                            }
                        } else {
                            pm_conf
                        }
                    } else {
                        pm_conf
                    };

                    // Calculate time and distance confidence components for detailed logging (trace level)
                    // These now use the parameterized formulas from PositionManager
                    let time_ratio = Decimal::new(seconds_remaining, 0) / Decimal::new(window_duration_secs, 0);
                    let time_conf = dec!(0.30) + dec!(0.70) * (Decimal::ONE - time_ratio); // default params
                    let dist_conf = (dec!(0.20) + dec!(0.50) * atr_mult).min(Decimal::ONE); // default params

                    // Calculate EV for logging (matches position.rs logic)
                    // EV-based with time-decaying edge requirement
                    let favorable_price = opp.favorable_price();
                    let ev = pm_conf - favorable_price;
                    let min_edge = self.config.max_edge_factor * time_ratio;

                    trace!(
                        "📈 Confidence {} {}: dist${:.2} atr${:.2} atr_mult={:.2} | time_conf={:.2} dist_conf={:.2} | conf={:.2} EV={:.3}",
                        event_id, asset, distance_dollars.abs(), atr, atr_mult,
                        time_conf, dist_conf, pm_conf, ev,
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

                    // Check reactive hedges for markets we already hold a position in.
                    // Extract position_direction before calling check_reactive_hedge
                    // to avoid borrow conflict (market borrows self.markets immutably).
                    let has_position = market.position_direction.is_some();

                    // End the immutable borrow on `market` so we can call &mut self method
                    let _ = market;

                    if has_position {
                        self.check_reactive_hedge(&event_id, pm_conf, &opp.signal).await?;
                    }

                    // Re-borrow market for should_trade
                    let market = self.markets.get(&event_id).unwrap();

                    // Check if we should trade based on EV threshold and budget
                    let trade_decision = market.should_trade(
                        distance_dollars,
                        seconds_remaining,
                        favorable_price,
                        self.config.max_edge_factor,
                        window_duration_secs,
                    );

                    let (total_size, pm_confidence, phase) = match trade_decision {
                        position::TradeDecision::Skip(reason) => {
                            // Rate-limited skip logging (max once per second)
                            if self.last_skip_log.elapsed() >= std::time::Duration::from_secs(1) {
                                self.last_skip_log = std::time::Instant::now();
                                info!(
                                    "⏭️ SKIP {} {} {:?}: signal={:?} dist=${:.2} atr_mult={:.2} conf={:.2} EV={:.3} min_edge={:.3} phase={} price={}",
                                    event_id, asset, reason, opp.signal, distance_dollars.abs(), atr_mult,
                                    pm_conf, ev, min_edge, phase, favorable_price
                                );
                            }
                            // Only record decision for meaningful skips, not repetitive steady-state ones
                            if !matches!(reason,
                                position::SkipReason::LowConfidence
                                | position::SkipReason::PhaseBudgetExhausted
                                | position::SkipReason::TotalBudgetExhausted
                            ) {
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
                                // Decision log for live vs backtest comparison
                                if let Some(ref logger) = self.decision_logger {
                                    logger.log_skip(
                                        self.simulated_time,
                                        &event_id,
                                        asset,
                                        spot_price,
                                        opp.strike_price,
                                        opp.distance,
                                        atr,
                                        seconds_remaining,
                                        &format!("{}", phase),
                                        opp.signal.clone(),
                                        opp.yes_ask,
                                        opp.no_ask,
                                        base_conf,
                                        pivot_conf.multiplier,
                                        pm_conf,
                                        ev,
                                        min_edge,
                                        favorable_price,
                                        reason,
                                    );
                                }
                            }
                            continue;
                        }
                        position::TradeDecision::Trade { size, confidence, phase } => {
                            (size, confidence, phase)
                        }
                    };

                    // CRITICAL: Check global exposure limit before trading
                    // The PositionManager only checks per-market budget, not global exposure
                    let remaining_global_capacity = self.config.sizing_config.max_total_exposure - total_exposure;
                    if remaining_global_capacity <= Decimal::ZERO {
                        debug!(
                            "Global exposure limit reached for {}: total={} max={}",
                            event_id, total_exposure, self.config.sizing_config.max_total_exposure
                        );
                        self.state.metrics.inc_skipped();
                        continue;
                    }

                    // Cap size to remaining global capacity
                    let total_size = if total_size > remaining_global_capacity {
                        info!(
                            "Capping directional size for {} from ${:.2} to ${:.2} (global exposure limit)",
                            event_id, total_size, remaining_global_capacity
                        );
                        remaining_global_capacity
                    } else {
                        total_size
                    };

                    // Verify size is meaningful — single leg, must reach $1.00
                    if total_size < Decimal::ONE {
                        debug!(
                            "Directional size too small for {}: ${:.2} < $1",
                            event_id, total_size
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

                    // Execute directional trade (single leg, no default hedge)
                    info!("🚀 EXECUTE {} {}: signal={:?} size=${:.2} phase={} conf={:.2}",
                        event_id, asset, opp.signal, total_size, phase, pm_confidence);

                    // Decision log for live vs backtest comparison
                    if let Some(ref logger) = self.decision_logger {
                        let log_up_ratio = if matches!(opp.signal, signal::Signal::StrongUp | signal::Signal::LeanUp) {
                            Decimal::ONE
                        } else {
                            Decimal::ZERO
                        };
                        logger.log_execute(
                            self.simulated_time,
                            &event_id,
                            asset,
                            spot_price,
                            opp.strike_price,
                            opp.distance,
                            atr,
                            seconds_remaining,
                            &format!("{}", phase),
                            opp.signal.clone(),
                            opp.yes_ask,
                            opp.no_ask,
                            base_conf,
                            pivot_conf.multiplier,
                            pm_confidence,
                            ev,
                            min_edge,
                            favorable_price,
                            total_size,
                            log_up_ratio,
                        );
                    }

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
                    // CRITICAL: Check global exposure limit before maker orders
                    let remaining_global_capacity = self.config.sizing_config.max_total_exposure - total_exposure;
                    if remaining_global_capacity <= Decimal::ZERO {
                        debug!(
                            "Global exposure limit reached for maker {}: total={} max={}",
                            event_id, total_exposure, self.config.sizing_config.max_total_exposure
                        );
                        self.state.metrics.inc_skipped();
                        continue;
                    }

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
        // Mark pending BEFORE placing orders to prevent duplicate submissions
        self.pending_tracker.register(&opportunity.event_id, PendingOrderType::Arbitrage);

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
        let mut yes_filled_fee = Decimal::ZERO;
        let mut no_filled_size = Decimal::ZERO;
        let mut no_filled_cost = Decimal::ZERO;
        let mut no_filled_fee = Decimal::ZERO;

        // Submit YES order
        let yes_result = self.executor.place_order(yes_order).await;
        match &yes_result {
            Ok(result) if result.is_filled() => {
                yes_filled_size = result.filled_size();
                yes_filled_cost = result.filled_cost();
                yes_filled_fee = result.filled_fee();
                debug!("YES order filled: {} shares, cost={}", yes_filled_size, yes_filled_cost);
            }
            Ok(result) => {
                debug!("YES order not filled: {:?}", result);
            }
            Err(e) => {
                warn!("YES order failed: {}", e);
                // Clear pending tracking on failure
                self.pending_tracker.clear(&opportunity.event_id, PendingOrderType::Arbitrage);
                // Only count system errors (connection, timeout, internal) for circuit breaker
                // FOK rejections and market conditions are normal and should NOT trip it
                if e.is_system_error() && self.state.record_failure(self.config.max_consecutive_failures) {
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
                no_filled_fee = result.filled_fee();
                debug!("NO order filled: {} shares, cost={}", no_filled_size, no_filled_cost);
            }
            Ok(result) => {
                debug!("NO order not filled: {:?}", result);
            }
            Err(e) => {
                warn!("{} order failed: {}", if opportunity.yes_ask < opportunity.no_ask { "DOWN" } else { "DOWN" }, e);
                // Only count system errors for circuit breaker, not FOK rejections
                if e.is_system_error() && self.state.record_failure(self.config.max_consecutive_failures) {
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

            // Log fills to trades log
            if let Some(ref logger) = self.trades_logger {
                let spot = self.spot_prices.get(&market.asset).copied().unwrap_or(Decimal::ZERO);
                let timestamp = market.window_end - chrono::Duration::seconds(market.state.seconds_remaining);
                let balance = self.executor.available_balance();
                if yes_filled_size > Decimal::ZERO {
                    let price = if yes_filled_size > Decimal::ZERO { yes_filled_cost / yes_filled_size } else { Decimal::ZERO };
                    logger.log_fill(
                        timestamp, &opportunity.event_id, market.asset, "YES",
                        yes_filled_size, price, yes_filled_fee, yes_filled_cost,
                        market.strike_price, spot,
                        "arb", Decimal::ZERO, balance,
                        market.inventory.yes_shares, market.inventory.no_shares,
                        market.inventory.yes_cost_basis + market.inventory.no_cost_basis,
                    );
                }
                if no_filled_size > Decimal::ZERO {
                    let price = if no_filled_size > Decimal::ZERO { no_filled_cost / no_filled_size } else { Decimal::ZERO };
                    logger.log_fill(
                        timestamp, &opportunity.event_id, market.asset, "NO",
                        no_filled_size, price, no_filled_fee, no_filled_cost,
                        market.strike_price, spot,
                        "arb", Decimal::ZERO, balance,
                        market.inventory.yes_shares, market.inventory.no_shares,
                        market.inventory.yes_cost_basis + market.inventory.no_cost_basis,
                    );
                }
            }

            // Update global state inventory for sizing calculations
            let pos = crate::state::InventoryPosition {
                event_id: opportunity.event_id.clone(),
                condition_id: market.condition_id.clone(),
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

        // Clear pending flag only if orders are NOT pending (i.e., filled or rejected)
        // If orders are pending on the exchange, keep the flag true to prevent duplicates
        // until we receive fill callbacks
        let yes_pending = yes_result.as_ref().map(|r| r.is_pending()).unwrap_or(false);
        let no_pending = no_result.as_ref().map(|r| r.is_pending()).unwrap_or(false);

        if !yes_pending && !no_pending {
            self.pending_tracker.clear(&opportunity.event_id, PendingOrderType::Arbitrage);
        }

        Ok(TradeAction::Execute)
    }

    /// Check whether reactive hedge tiers should fire for a market we already hold.
    ///
    /// Tracks peak confidence per market. When confidence drops from peak, triggers
    /// graduated hedge tiers, each gated by a max hedge price:
    /// - Tier 0: 30% drop from peak → hedge 10% of directional shares, max 25¢
    /// - Tier 1: 50% drop from peak → hedge 15% of directional shares, max 35¢
    /// - Tier 2: Signal flips direction → hedge 25% of directional shares, max 45¢
    ///
    /// Each tier fires at most once per market.
    async fn check_reactive_hedge(
        &mut self,
        event_id: &str,
        current_confidence: Decimal,
        signal: &signal::Signal,
    ) -> Result<(), StrategyError> {
        // Hedge tier configuration (hardcoded for now)
        const NUM_TIERS: usize = 3;
        let tier_drop_pcts: [Decimal; NUM_TIERS] = [dec!(0.30), dec!(0.50), dec!(1.0)]; // 1.0 = signal flip
        let tier_size_pcts: [Decimal; NUM_TIERS] = [dec!(0.10), dec!(0.15), dec!(0.25)];
        let tier_max_prices: [Decimal; NUM_TIERS] = [dec!(0.25), dec!(0.35), dec!(0.45)];

        // Get market state — need position_direction to determine hedge side
        let (position_direction, peak_confidence, directional_shares,
             hedge_token_id, hedge_outcome, hedge_side_label, min_order_size,
             tiers_triggered, event_id_owned, asset,
             strike_price, seconds_remaining) = {
            let market = match self.markets.get_mut(event_id) {
                Some(m) => m,
                None => return Ok(()),
            };

            // No position yet — nothing to hedge
            let pos_dir = match market.position_direction {
                Some(d) => d,
                None => return Ok(()),
            };

            // Update peak confidence
            market.peak_confidence = market.peak_confidence.max(current_confidence);

            // Determine which side has shares and which is the hedge side
            let (dir_shares, hedge_token, hedge_out, hedge_label) = if pos_dir {
                // Position is UP/YES — hedge with NO
                (market.inventory.yes_shares, market.no_token_id.clone(), Outcome::No, "NO")
            } else {
                // Position is DOWN/NO — hedge with YES
                (market.inventory.no_shares, market.yes_token_id.clone(), Outcome::Yes, "YES")
            };

            // No shares to hedge against
            if dir_shares <= Decimal::ZERO {
                return Ok(());
            }

            (pos_dir, market.peak_confidence, dir_shares,
             hedge_token, hedge_out, hedge_label, market.min_order_size,
             market.hedge_tiers_triggered, market.event_id.clone(), market.asset,
             market.strike_price, market.state.seconds_remaining)
        };

        // Calculate confidence drop percentage
        let drop_pct = if peak_confidence > Decimal::ZERO {
            (peak_confidence - current_confidence) / peak_confidence
        } else {
            Decimal::ZERO
        };

        // Check if signal has flipped relative to position direction
        let signal_is_up = matches!(signal, signal::Signal::StrongUp | signal::Signal::LeanUp);
        let signal_flipped = position_direction != signal_is_up && signal.is_directional();

        // Check each untriggered tier
        for tier in 0..NUM_TIERS {
            if tiers_triggered[tier] {
                continue;
            }

            // Tier 2 triggers on signal flip, tiers 0-1 trigger on confidence drop
            let should_trigger = if tier == 2 {
                signal_flipped
            } else {
                drop_pct >= tier_drop_pcts[tier]
            };

            if !should_trigger {
                continue;
            }

            // Get hedge side ask price
            let hedge_price = match self.state.market_data.order_books.get(&hedge_token_id) {
                Some(book) => book.best_ask,
                None => continue, // No orderbook, skip
            };

            // Price gate — skip if hedge side is too expensive
            if hedge_price > tier_max_prices[tier] || hedge_price <= Decimal::ZERO {
                info!(
                    "Hedge tier {} skipped for {} {}: price {} > max {}",
                    tier, event_id_owned, asset, hedge_price, tier_max_prices[tier]
                );
                // Log price-gated skip to decision log
                let spot = self.spot_prices.get(&asset).copied().unwrap_or(Decimal::ZERO);
                if let Some(ref logger) = self.decision_logger {
                    logger.log_hedge(
                        Utc::now(), &event_id_owned, asset, spot, strike_price,
                        seconds_remaining, *signal, current_confidence, peak_confidence,
                        tier, drop_pct, signal_flipped, hedge_side_label,
                        hedge_price, tier_max_prices[tier], Decimal::ZERO,
                        "HEDGE_SKIP_PRICE",
                    );
                }
                continue;
            }

            // Calculate hedge shares
            let mut hedge_shares = (directional_shares * tier_size_pcts[tier]).floor();
            if hedge_shares < min_order_size {
                hedge_shares = min_order_size;
            }

            info!(
                "🛡️ Hedge tier {} triggered for {} {}: drop={:.1}% flip={} shares={} price={} (max {})",
                tier, event_id_owned, asset,
                drop_pct * dec!(100), signal_flipped,
                hedge_shares, hedge_price, tier_max_prices[tier]
            );

            // Place hedge order (IOC/market order — we want immediate fill)
            let req_id = self.decision_counter;
            self.decision_counter += 1;
            let order = OrderRequest {
                request_id: format!("hedge-{}-t{}", req_id, tier),
                event_id: event_id_owned.clone(),
                token_id: hedge_token_id.clone(),
                outcome: hedge_outcome,
                side: Side::Buy,
                size: hedge_shares,
                price: Some(hedge_price),
                order_type: OrderType::Ioc, // Always IOC for hedges — want immediate fill
                timeout_ms: None,
                timestamp: Utc::now(),
            };

            let result = self.executor.place_order(order).await;

            // Mark tier as triggered regardless of fill outcome
            if let Some(market) = self.markets.get_mut(event_id) {
                market.hedge_tiers_triggered[tier] = true;
            }

            match result {
                Ok(result) if result.is_filled() => {
                    let filled_size = result.filled_size();
                    let filled_cost = result.filled_cost();
                    let filled_fee = result.filled_fee();

                    if let Some(market) = self.markets.get_mut(event_id) {
                        market.inventory.record_fill(hedge_outcome, filled_size, filled_cost);

                        // Log to trades log
                        if let Some(ref logger) = self.trades_logger {
                            let spot = self.spot_prices.get(&asset).copied().unwrap_or(Decimal::ZERO);
                            let timestamp = market.window_end - chrono::Duration::seconds(market.state.seconds_remaining);
                            let balance = self.executor.available_balance();
                            let price = if filled_size > Decimal::ZERO { filled_cost / filled_size } else { Decimal::ZERO };
                            let side_label = if hedge_outcome == Outcome::Yes { "YES" } else { "NO" };
                            logger.log_fill(
                                timestamp, event_id, asset, side_label,
                                filled_size, price, filled_fee, filled_cost,
                                market.strike_price, spot,
                                &format!("hedge_t{}", tier), current_confidence, balance,
                                market.inventory.yes_shares, market.inventory.no_shares,
                                market.inventory.yes_cost_basis + market.inventory.no_cost_basis,
                            );
                        }

                        // Update global state
                        let pos = crate::state::InventoryPosition {
                            event_id: event_id.to_string(),
                            condition_id: market.condition_id.clone(),
                            yes_shares: market.inventory.yes_shares,
                            no_shares: market.inventory.no_shares,
                            yes_cost_basis: market.inventory.yes_cost_basis,
                            no_cost_basis: market.inventory.no_cost_basis,
                            realized_pnl: market.inventory.realized_pnl,
                        };
                        self.state.market_data.update_inventory(event_id, pos);
                    }

                    let volume_cents = (filled_cost * Decimal::new(100, 0))
                        .try_into()
                        .unwrap_or(0u64);
                    self.state.metrics.add_volume_cents(volume_cents);

                    info!(
                        "🛡️ Hedge tier {} filled for {} {}: {} shares at {}",
                        tier, event_id_owned, asset, filled_size, hedge_price
                    );

                    // Log filled hedge to decision log
                    if let Some(ref logger) = self.decision_logger {
                        let spot = self.spot_prices.get(&asset).copied().unwrap_or(Decimal::ZERO);
                        logger.log_hedge(
                            Utc::now(), &event_id_owned, asset, spot, strike_price,
                            seconds_remaining, *signal, current_confidence, peak_confidence,
                            tier, drop_pct, signal_flipped, hedge_side_label,
                            hedge_price, tier_max_prices[tier], filled_size,
                            "HEDGE_FILL",
                        );
                    }
                }
                Ok(_) => {
                    debug!("Hedge tier {} order not filled for {}", tier, event_id_owned);
                    // Log unfilled hedge to decision log
                    if let Some(ref logger) = self.decision_logger {
                        let spot = self.spot_prices.get(&asset).copied().unwrap_or(Decimal::ZERO);
                        logger.log_hedge(
                            Utc::now(), &event_id_owned, asset, spot, strike_price,
                            seconds_remaining, *signal, current_confidence, peak_confidence,
                            tier, drop_pct, signal_flipped, hedge_side_label,
                            hedge_price, tier_max_prices[tier], hedge_shares,
                            "HEDGE_SKIP_NOFILL",
                        );
                    }
                }
                Err(e) => {
                    warn!("Hedge tier {} order failed for {}: {}", tier, event_id_owned, e);
                    // Log failed hedge to decision log
                    if let Some(ref logger) = self.decision_logger {
                        let spot = self.spot_prices.get(&asset).copied().unwrap_or(Decimal::ZERO);
                        logger.log_hedge(
                            Utc::now(), &event_id_owned, asset, spot, strike_price,
                            seconds_remaining, *signal, current_confidence, peak_confidence,
                            tier, drop_pct, signal_flipped, hedge_side_label,
                            hedge_price, tier_max_prices[tier], hedge_shares,
                            "HEDGE_FAIL",
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Execute a directional trade based on signal — single leg only (no default hedge).
    ///
    /// Goes 100% directional on entry. Hedges are added reactively via `check_reactive_hedge()`
    /// when confidence deteriorates, buying the opposite side while it's still cheap.
    ///
    /// # Arguments
    ///
    /// * `opportunity` - The detected directional opportunity with signal
    /// * `total_size` - Total USDC to deploy on the directional leg
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

        // Get seconds_remaining for budget reservation
        let seconds_remaining = self.markets
            .get(&event_id)
            .map(|m| m.state.seconds_remaining)
            .unwrap_or(0);

        // Mark pending, set cooldown timer, and reserve budget BEFORE placing orders
        self.pending_tracker.register(&event_id, PendingOrderType::Directional);
        if let Some(market) = self.markets.get_mut(&event_id) {
            // Use simulated time for cooldown (works in both live and backtest)
            let current_time = market.window_end - chrono::Duration::seconds(market.state.seconds_remaining);
            market.last_order_timestamp = Some(current_time);
            market.reserve_budget(total_size, seconds_remaining);
            // Store signal/confidence for trades log
            market.last_signal = format!("{:?}", opportunity.signal);
            market.last_confidence = opportunity.confidence.total_multiplier();
        }

        // Get the tracked market data (only what we need, no stale orderbook clones)
        let (yes_token_id, no_token_id, min_order_size) = match self.markets.get(&event_id) {
            Some(m) => (
                m.yes_token_id.clone(),
                m.no_token_id.clone(),
                m.min_order_size,
            ),
            None => {
                warn!("No tracked market for directional: {}", event_id);
                self.pending_tracker.clear(&event_id, PendingOrderType::Directional);
                if let Some(market) = self.markets.get_mut(&event_id) {
                    market.release_reservation(total_size, seconds_remaining);
                }
                return Ok(TradeAction::SkipSizing);
            }
        };

        // Determine direction from signal — single leg, no split
        let is_up = matches!(opportunity.signal, signal::Signal::StrongUp | signal::Signal::LeanUp);
        let (token_id, outcome, other_token_id) = if is_up {
            (yes_token_id.clone(), Outcome::Yes, no_token_id.clone())
        } else {
            (no_token_id.clone(), Outcome::No, yes_token_id.clone())
        };
        let side_label = if is_up { "YES" } else { "NO" };

        // Get directional side price from WebSocket orderbook cache
        let dir_price = match self.state.market_data.order_books.get(&token_id) {
            Some(live_book) => live_book.best_ask,
            None => {
                warn!(token_id = %token_id, "No orderbook for {} token, skipping", side_label);
                self.pending_tracker.clear(&event_id, PendingOrderType::Directional);
                if let Some(market) = self.markets.get_mut(&event_id) {
                    market.release_reservation(total_size, seconds_remaining);
                }
                return Ok(TradeAction::SkipSizing);
            }
        };
        // Get other side price for chase market-awareness (not for placing orders)
        let other_price = self.state.market_data.order_books.get(&other_token_id)
            .map(|b| b.best_ask)
            .unwrap_or(Decimal::ZERO);

        if dir_price <= Decimal::ZERO {
            debug!("Invalid {} price {} for {}", side_label, dir_price, event_id);
            self.pending_tracker.clear(&event_id, PendingOrderType::Directional);
            if let Some(market) = self.markets.get_mut(&event_id) {
                market.release_reservation(total_size, seconds_remaining);
            }
            return Ok(TradeAction::SkipSizing);
        }

        // Convert USDC to shares for single directional leg
        let mut shares = (total_size / dir_price).floor();
        if shares < min_order_size {
            shares = min_order_size;
        }

        info!(
            "Directional trade: {} signal={:?} side={} distance={:.4}% conf={:.2}x shares={} price={} min_order_size={}",
            event_id,
            opportunity.signal,
            side_label,
            opportunity.distance * Decimal::ONE_HUNDRED,
            opportunity.confidence.total_multiplier(),
            shares,
            dir_price,
            min_order_size
        );

        // Generate request ID
        let req_id_base = self.decision_counter;
        self.decision_counter += 1;

        // Track volume and fills
        let mut total_volume = Decimal::ZERO;
        let mut filled_size = Decimal::ZERO;
        let mut filled_cost = Decimal::ZERO;
        let mut filled_fee = Decimal::ZERO;
        let mut order_pending = false;

        // Determine order type based on execution mode
        let (order_type, use_chase) = match self.config.execution.execution_mode {
            crate::config::ExecutionMode::Limit => {
                let chase = self.config.execution.chase_enabled && !self.config.is_backtest;
                (OrderType::Gtc, chase)
            }
            crate::config::ExecutionMode::Market => (OrderType::Ioc, false),
        };

        // Place single directional order
        let order = OrderRequest {
            request_id: format!("dir-{}-{}", req_id_base, side_label.to_lowercase()),
            event_id: event_id.clone(),
            token_id: token_id.clone(),
            outcome,
            side: Side::Buy,
            size: shares,
            price: Some(dir_price),
            order_type,
            timeout_ms: None,
            timestamp: Utc::now(),
        };

        // Clean up any orphaned orders for this token
        match self.executor.cancel_orders_for_token(&token_id).await {
            Ok(count) if count > 0 => {
                info!(count = count, token_id = %token_id, "Cleaned up orphan orders for {} token", side_label);
            }
            Err(e) => {
                warn!(error = %e, token_id = %token_id, "Failed to cleanup orphan orders for {} token", side_label);
            }
            _ => {}
        }

        // Submit order — async order manager, chase, or direct
        let order_result = if let Some(ref order_manager) = self.async_order_manager {
            let intent = OrderIntent::new(
                event_id.clone(),
                token_id.clone(),
                outcome,
                Side::Buy,
                shares,
                dir_price,
            );
            match order_manager.submit(intent, other_price).await {
                Ok(handle) => {
                    debug!(handle_id = %handle.id, "{} order submitted to async order manager", side_label);
                    order_pending = true;
                    Ok(OrderResult::Pending(crate::executor::PendingOrder {
                        request_id: format!("dir-{}-{}", req_id_base, side_label.to_lowercase()),
                        order_id: handle.id.0.clone(),
                        timestamp: Utc::now(),
                    }))
                }
                Err(rejected) => {
                    warn!(reason = %rejected.reason, "{} order rejected by order manager", side_label);
                    Ok(OrderResult::Rejected(crate::executor::OrderRejection {
                        request_id: format!("dir-{}-{}", req_id_base, side_label.to_lowercase()),
                        reason: rejected.reason.to_string(),
                        timestamp: Utc::now(),
                    }))
                }
            }
        } else if use_chase {
            const MAX_POST_ONLY_RETRIES: u32 = 3;
            let mut current_order = order;
            let mut post_only_retries = 0u32;
            let state_clone = self.state.clone();
            let token_id_clone = token_id.clone();
            loop {
                let get_best_price = || {
                    state_clone.market_data.order_books.get(&token_id_clone)
                        .map(|book| book.best_bid)
                };
                match self.chaser.chase_order_with_market(&mut self.executor, current_order.clone(), other_price, get_best_price).await {
                    Ok(chase_result) => {
                        if chase_result.stop_reason == Some(ChaseStopReason::MarketClosed) {
                            warn!(event_id = %event_id, "Market closed during {} order chase", side_label);
                            self.markets.remove(&event_id);
                            self.state.market_data.active_windows.remove(&event_id);
                            break Ok(OrderResult::Rejected(crate::executor::OrderRejection {
                                request_id: format!("dir-{}-{}", req_id_base, side_label.to_lowercase()),
                                reason: "Market closed".to_string(),
                                timestamp: Utc::now(),
                            }));
                        }
                        if chase_result.stop_reason == Some(ChaseStopReason::PostOnlyRejected) {
                            post_only_retries += 1;
                            if post_only_retries >= MAX_POST_ONLY_RETRIES {
                                break Ok(OrderResult::Rejected(crate::executor::OrderRejection {
                                    request_id: format!("dir-{}-{}", req_id_base, side_label.to_lowercase()),
                                    reason: "POST_ONLY max retries reached".to_string(),
                                    timestamp: Utc::now(),
                                }));
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            if let Some(live_book) = self.state.market_data.order_books.get(&token_id) {
                                let fresh_price = live_book.best_bid;
                                if fresh_price > Decimal::ZERO {
                                    current_order.price = Some(fresh_price);
                                    continue;
                                }
                            }
                            break Ok(OrderResult::Rejected(crate::executor::OrderRejection {
                                request_id: format!("dir-{}-{}", req_id_base, side_label.to_lowercase()),
                                reason: "No fresh price available".to_string(),
                                timestamp: Utc::now(),
                            }));
                        }
                        if chase_result.success || chase_result.filled_size > Decimal::ZERO {
                            break Ok(OrderResult::Filled(crate::executor::OrderFill {
                                request_id: format!("dir-{}-{}", req_id_base, side_label.to_lowercase()),
                                order_id: "chased".to_string(),
                                size: chase_result.filled_size,
                                price: chase_result.avg_price,
                                fee: chase_result.total_fee,
                                timestamp: Utc::now(),
                            }));
                        } else {
                            let reason = chase_result.stop_reason
                                .map(|r| format!("Chase stopped: {:?}", r))
                                .unwrap_or_else(|| "Chase failed".to_string());
                            break Ok(OrderResult::Rejected(crate::executor::OrderRejection {
                                request_id: format!("dir-{}-{}", req_id_base, side_label.to_lowercase()),
                                reason,
                                timestamp: Utc::now(),
                            }));
                        }
                    }
                    Err(e) => break Err(e),
                }
            }
        } else {
            self.executor.place_order(order).await
        };

        match order_result {
            Ok(result) if result.is_filled() => {
                filled_size = result.filled_size();
                filled_cost = result.filled_cost();
                filled_fee = result.filled_fee();
                total_volume += filled_cost;
                debug!("{} order filled: {} shares, cost={}", side_label, filled_size, filled_cost);
            }
            Ok(result) => {
                order_pending = result.is_pending();
                debug!("{} order not filled: {:?}", side_label, result);
            }
            Err(e) => {
                warn!("{} order failed: {}", side_label, e);
                if e.is_system_error() && self.state.record_failure(self.config.max_consecutive_failures) {
                    self.state.trip_circuit_breaker();
                    warn!("Circuit breaker tripped after consecutive failures");
                }
            }
        }

        // Update inventory after fill
        if filled_size > Decimal::ZERO
            && let Some(market) = self.markets.get_mut(&event_id)
        {
            market.inventory.record_fill(outcome, filled_size, filled_cost);

            // Set position direction and peak confidence for reactive hedge tracking
            market.position_direction = Some(is_up);
            market.peak_confidence = market.peak_confidence.max(opportunity.confidence.total_multiplier());

            // Log fill to trades log
            if let Some(ref logger) = self.trades_logger {
                let spot = self.spot_prices.get(&market.asset).copied().unwrap_or(Decimal::ZERO);
                let timestamp = market.window_end - chrono::Duration::seconds(market.state.seconds_remaining);
                let balance = self.executor.available_balance();
                let price = if filled_size > Decimal::ZERO { filled_cost / filled_size } else { Decimal::ZERO };
                logger.log_fill(
                    timestamp, &event_id, market.asset, side_label,
                    filled_size, price, filled_fee, filled_cost,
                    market.strike_price, spot,
                    &market.last_signal, market.last_confidence, balance,
                    market.inventory.yes_shares, market.inventory.no_shares,
                    market.inventory.yes_cost_basis + market.inventory.no_cost_basis,
                );
            }

            // Update global state inventory
            let pos = crate::state::InventoryPosition {
                event_id: event_id.clone(),
                condition_id: market.condition_id.clone(),
                yes_shares: market.inventory.yes_shares,
                no_shares: market.inventory.no_shares,
                yes_cost_basis: market.inventory.yes_cost_basis,
                no_cost_basis: market.inventory.no_cost_basis,
                realized_pnl: market.inventory.realized_pnl,
            };
            self.state.market_data.update_inventory(&event_id, pos);

            // Commit reservation
            let seconds_remaining = market.state.seconds_remaining;
            let up_cost = if is_up { filled_cost } else { Decimal::ZERO };
            let down_cost = if is_up { Decimal::ZERO } else { filled_cost };
            market.commit_reservation(total_size, total_volume, up_cost, down_cost, seconds_remaining);

            debug!(
                "Inventory updated for {}: YES={} NO={} total_exposure={}",
                event_id,
                market.inventory.yes_shares,
                market.inventory.no_shares,
                market.inventory.total_exposure()
            );
        }

        // Update metrics
        if total_volume > Decimal::ZERO {
            let volume_cents = (total_volume * Decimal::new(100, 0))
                .try_into()
                .unwrap_or(0u64);
            self.state.metrics.add_volume_cents(volume_cents);
            self.state.record_success();
        }

        // Clear pending tracking if order is NOT pending
        if !order_pending {
            self.pending_tracker.clear(&event_id, PendingOrderType::Directional);

            if total_volume == Decimal::ZERO && self.markets.contains_key(&event_id) {
                debug!(
                    "Trade attempt for {} completed without fills, budget reservation released",
                    event_id
                );
            }
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
                // Only count system errors for circuit breaker, not FOK rejections
                if e.is_system_error() && self.state.record_failure(self.config.max_consecutive_failures) {
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

        // Write session summary before shutting down
        if let Some(ref logger) = self.trades_logger {
            let start = self.first_event_time.unwrap_or_else(Utc::now);
            let end = self.last_event_time.unwrap_or_else(Utc::now);
            logger.write_session_summary(start, end);
        }

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

    /// Get simulation statistics from the executor (if available).
    ///
    /// Returns Some for simulated/backtest executors, None for live.
    pub fn simulation_stats(&self) -> Option<crate::executor::simulated::SimulatedStats> {
        self.executor.simulation_stats()
    }

    /// Get the actual event time range from processed events.
    /// Returns (first_event_time, last_event_time) if any events were processed.
    pub fn event_time_range(&self) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
        (self.first_event_time, self.last_event_time)
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

        fn market_exposure(&self, _event_id: &str) -> Decimal {
            Decimal::ZERO
        }

        fn total_exposure(&self) -> Decimal {
            Decimal::ZERO
        }

        fn remaining_capacity(&self) -> Decimal {
            Decimal::MAX
        }

        fn get_position(&self, _event_id: &str) -> Option<crate::executor::PositionSnapshot> {
            None
        }

        async fn shutdown(&mut self) {}
    }

    fn create_chainlink_price_event(asset: CryptoAsset, price: Decimal) -> MarketEvent {
        MarketEvent::ChainlinkPrice(crate::data_source::ChainlinkPriceEvent {
            asset,
            price,
            timestamp: Utc::now(),
        })
    }

    fn create_window_open_event(event_id: &str, asset: CryptoAsset) -> MarketEvent {
        MarketEvent::WindowOpen(WindowOpenEvent {
            event_id: event_id.to_string(),
            condition_id: format!("{}-cond", event_id),
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
            create_chainlink_price_event(CryptoAsset::Btc, dec!(100000)),
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
            create_chainlink_price_event(CryptoAsset::Btc, dec!(100000)),
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
        // Explicitly enable arb for this test (disabled by default due to fees)
        let mut engines_config = EnginesConfig::default();
        engines_config.arbitrage.enabled = true;

        let mut strategy = StrategyLoop::with_engines(data_source, executor, state.clone(), config, engines_config);
        state.enable_trading();

        let _ = strategy.run().await;

        // Should have detected opportunity and executed
        assert!(state.metrics.opportunities_detected.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn test_trading_disabled_skips_opportunities() {
        let events = vec![
            create_chainlink_price_event(CryptoAsset::Btc, dec!(100000)),
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
            create_chainlink_price_event(CryptoAsset::Btc, dec!(100000)),
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
            condition_id: "test-cond".to_string(),
            asset: CryptoAsset::Btc,
            yes_token_id: "yes-token".to_string(),
            no_token_id: "no-token".to_string(),
            strike_price: dec!(100000),
            window_start: Utc::now(),
            window_end: Utc::now() + chrono::Duration::minutes(15),
            timestamp: Utc::now(),
        };

        let market = TrackedMarket::new_for_backtest(&event, dec!(100), &StrategyConfig::default());
        assert!(market.active_maker_orders.is_empty());
    }
}
