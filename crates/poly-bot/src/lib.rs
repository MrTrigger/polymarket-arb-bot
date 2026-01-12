//! Polymarket 15-minute arbitrage trading bot.
//!
//! This crate implements the core trading logic for exploiting mispricings
//! when YES + NO shares sum to less than $1.00 on Polymarket's 15-minute
//! up/down markets.
//!
//! ## Architecture
//!
//! - **Lock-free hot path**: DashMap and atomics for shared state
//! - **Pre-hashed signing**: Shadow bids fire <2ms after primary fill
//! - **Fire-and-forget observability**: <10ns overhead on hot path
//!
//! ## Modules
//!
//! - `config`: Configuration loading and validation
//! - `state`: Global shared state with lock-free access
//! - `types`: Order book, market state, and inventory types
//! - `data_source`: Data source abstraction (live WebSocket, replay from ClickHouse)
//! - `executor`: Order execution abstraction (live, paper, backtest)

pub mod config;
pub mod data_source;
pub mod executor;
pub mod observability;
pub mod risk;
pub mod state;
pub mod strategy;
pub mod types;

pub use config::{BotConfig, ObservabilityConfig};
pub use data_source::{
    BookDeltaEvent, BookSnapshotEvent, DataSource, DataSourceError, FillEvent, MarketEvent,
    SpotPriceEvent, WindowCloseEvent, WindowOpenEvent,
};
pub use data_source::live::{ActiveMarket, LiveDataSource, LiveDataSourceConfig};
pub use data_source::replay::{ReplayConfig, ReplayDataSource};
pub use executor::{
    Executor, ExecutorError, OrderCancellation, OrderFill, OrderRejection, OrderRequest,
    OrderResult, OrderType, PartialOrderFill, PendingOrder,
};
pub use executor::backtest::{BacktestExecutor, BacktestExecutorConfig, BacktestPosition, BacktestStats};
pub use executor::live::{LiveExecutor, LiveExecutorConfig};
pub use executor::paper::{PaperExecutor, PaperExecutorConfig, PaperPosition};
pub use executor::shadow::{
    PrehashedOrder, ShadowError, ShadowFireResult, ShadowManager, ShadowOrder, ShadowStatus,
    SharedShadowManager,
};
pub use executor::chase::{
    ChaseConfig, ChaseFill, ChaseResult, ChaseStopReason, PriceChaser,
};
pub use executor::fill::{
    FillData, FillHandler, FillHandlerConfig, FillResult, InventorySnapshot, PartialFillState,
};
pub use state::{
    ActiveWindow, ControlFlags, GlobalState, InventoryPosition, InventoryState, LiveOrderBook,
    MetricsCounters, MetricsSnapshot, SharedMarketData, ShadowOrderState, WindowPhase,
};
pub use strategy::{ArbDetector, ArbOpportunity, ArbRejection, ArbThresholds};
pub use risk::{PreTradeCheck, PreTradeRejection, RiskCheckConfig, RiskCheckResult, RiskChecker};
pub use risk::{
    ChaseReason, CloseReason, LegRiskAction, LegRiskAssessment, LegRiskConfig, LegRiskManager,
    LegState, LegStatus,
};
pub use types::{Inventory, MarketState, OrderBook, PriceLevel};
pub use observability::{
    ActionType, Counterfactual, DecisionContext, DecisionSnapshot, ObservabilityEvent,
    OutcomeType, SnapshotBuilder,
};
pub use observability::{
    create_capture_channel, create_shared_capture, CaptureConfig, CaptureReceiver, CaptureSender,
    CaptureStats, CaptureStatsSnapshot, ObservabilityCapture, SharedCapture,
    DEFAULT_CHANNEL_CAPACITY,
};
pub use observability::{
    create_shared_processor, hash_string, spawn_processor, DecisionRecord, IdLookup,
    InMemoryIdLookup, ObservabilityProcessor, ProcessorConfig, ProcessorStats,
    ProcessorStatsSnapshot, SharedProcessor,
};
pub use observability::{
    create_shared_analyzer, CounterfactualAnalyzer, CounterfactualConfig, CounterfactualStats,
    CounterfactualStatsSnapshot, PendingDecision, Settlement, SharedCounterfactualAnalyzer,
};
pub use observability::{
    create_shared_detector, create_shared_detector_with_capture, Anomaly, AnomalyConfig,
    AnomalyDetector, AnomalySeverity, AnomalyStats, AnomalyStatsSnapshot, AnomalyType,
    SharedAnomalyDetector,
};
