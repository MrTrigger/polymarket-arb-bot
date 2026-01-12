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

pub mod config;
pub mod data_source;
pub mod state;
pub mod types;

pub use config::{BotConfig, ObservabilityConfig};
pub use data_source::{
    BookDeltaEvent, BookSnapshotEvent, DataSource, DataSourceError, FillEvent, MarketEvent,
    SpotPriceEvent, WindowCloseEvent, WindowOpenEvent,
};
pub use data_source::live::{ActiveMarket, LiveDataSource, LiveDataSourceConfig};
pub use data_source::replay::{ReplayConfig, ReplayDataSource};
pub use state::{
    ActiveWindow, ControlFlags, GlobalState, InventoryPosition, InventoryState, LiveOrderBook,
    MetricsCounters, MetricsSnapshot, SharedMarketData, ShadowOrderState, WindowPhase,
};
pub use types::{Inventory, MarketState, OrderBook, PriceLevel};
