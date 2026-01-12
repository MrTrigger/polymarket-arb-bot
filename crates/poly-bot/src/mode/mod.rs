//! Trading modes for poly-bot.
//!
//! This module contains the different trading modes:
//!
//! - **Live**: Real trading with real money
//! - **Paper**: Real data, simulated execution (coming soon)
//! - **Shadow**: Real data, log-only (coming soon)
//! - **Backtest**: Historical data replay (coming soon)
//!
//! Each mode wires together the appropriate `DataSource` and `Executor`
//! implementations with the strategy loop.

pub mod live;

pub use live::{LiveMode, LiveModeConfig};
