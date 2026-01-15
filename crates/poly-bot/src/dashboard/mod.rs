//! Dashboard module for the React trading dashboard.
//!
//! This module provides event capture and processing for the real-time
//! dashboard that displays trading activity, P&L, and market state.

pub mod api;
pub mod capture;
pub mod integration;
pub mod processor;
pub mod server;
pub mod session;
pub mod state;
pub mod types;

pub use api::*;
pub use capture::*;
pub use integration::*;
pub use processor::*;
pub use server::*;
pub use session::*;
pub use state::*;
pub use types::*;
