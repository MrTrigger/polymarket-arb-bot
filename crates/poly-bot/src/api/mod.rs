//! Polymarket API client modules.
//!
//! This module provides HTTP clients for Polymarket's CLOB API endpoints
//! that are not covered by the official SDK or require caching behavior.
//!
//! ## Modules
//!
//! - `fees`: Fee rate fetching with per-session caching
//! - `markets`: Market data including min_order_size from orderbook
//! - `rewards`: Maker rebate rewards fetching and tracking

pub mod fees;
pub mod markets;
pub mod rewards;

pub use fees::{FeeRateClient, FeeRateError, FeeRateResponse};
pub use markets::{MarketClient, MarketError, MarketResponse, OrderBookResponse, OrderLevel};
pub use rewards::{
    CurrentRewardResponse, RewardsClient, RewardsConfig, RewardsError, TotalUserEarningResponse,
    UserEarning,
};
