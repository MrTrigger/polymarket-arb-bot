//! Trading modes for poly-bot.
//!
//! This module contains the different trading modes:
//!
//! - **Live**: Real trading with real money
//! - **Paper**: Real data, simulated execution
//! - **Shadow**: Real data, log-only (validates feeds without execution)
//! - **Backtest**: Historical data replay (coming soon)
//!
//! Each mode wires together the appropriate `DataSource` and `Executor`
//! implementations with the strategy loop.

pub mod live;
pub mod paper;
pub mod shadow;

pub use live::{LiveMode, LiveModeConfig};
pub use paper::{PaperMode, PaperModeConfig};
pub use shadow::{ShadowMode, ShadowModeConfig};
