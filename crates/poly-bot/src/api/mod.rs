//! Polymarket API client modules.
//!
//! This module provides HTTP clients for Polymarket's CLOB API endpoints
//! that are not covered by the official SDK or require caching behavior.
//!
//! ## Modules
//!
//! - `fees`: Fee rate fetching with per-session caching

pub mod fees;

pub use fees::{FeeRateClient, FeeRateError, FeeRateResponse};
