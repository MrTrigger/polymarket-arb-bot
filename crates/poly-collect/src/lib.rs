//! Poly-collect: Real-time data collection for Polymarket arbitrage bot.
//!
//! This crate provides:
//! - Market discovery via Gamma API (using shared `poly-market` crate)
//! - Binance WebSocket capture for spot prices
//! - Polymarket CLOB WebSocket capture for orderbooks (using shared `poly-market` types)
//!
//! Core Polymarket types and functionality come from the `poly-market` crate.

pub mod binance;
pub mod clob;
pub mod config;
pub mod csv_writer;
pub mod data_writer;
pub mod discovery;
pub mod history;
pub mod rtds;
