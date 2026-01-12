//! Shared types and utilities for the Polymarket arbitrage bot.
//!
//! This crate contains:
//! - Common types (CryptoAsset, Side, OrderBookLevel, MarketWindow)
//! - ClickHouse client wrapper
//! - Schema definitions

pub mod clickhouse;
pub mod types;

pub use clickhouse::{ClickHouseClient, ClickHouseConfig, ClickHouseError};
pub use types::*;
