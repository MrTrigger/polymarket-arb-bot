//! Dashboard module for the React trading dashboard.
//!
//! This module provides event capture and processing for the real-time
//! dashboard that displays trading activity, P&L, and market state.

pub mod capture;
pub mod processor;
pub mod session;
pub mod state;
pub mod types;

pub use capture::*;
pub use processor::*;
pub use session::*;
pub use state::*;
pub use types::*;
