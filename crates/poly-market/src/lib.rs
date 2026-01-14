//! Polymarket integration library.
//!
//! Provides shared functionality for interacting with Polymarket:
//! - Market discovery via Gamma API
//! - CLOB WebSocket client for orderbook data
//! - Orderbook state management
//!
//! Used by both `poly-collect` (data collection) and `poly-bot` (trading).

pub mod clob;
pub mod discovery;
pub mod orderbook;
pub mod types;

// Re-export main types and clients
pub use clob::{ClobClient, ClobConfig, ClobError, ClobEvent};
pub use discovery::{DiscoveredMarket, DiscoveryConfig, DiscoveryError, MarketDiscovery};
pub use orderbook::{parse_timestamp, OrderBookState};
pub use types::{
    BookMessage, GammaEvent, GammaMarket, GammaTag, OrderSummary, PriceChange, PriceChangeMessage,
    TokenIds,
};
