//! Configuration for poly-bot.
//!
//! Supports loading from TOML file with environment variable overrides.
//! All trading parameters from the spec are defined here.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use poly_common::{ClickHouseConfig, WindowDuration};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::types::EngineType;

/// Top-level configuration for poly-bot.
#[derive(Debug, Clone)]
pub struct BotConfig {
    /// Trading mode: live, paper, shadow, backtest.
    pub mode: TradingMode,

    /// Assets to trade.
    pub assets: Vec<String>,

    /// Market window duration (15min or 1h).
    pub window_duration: WindowDuration,

    /// Console logging level.
    pub console_log_level: String,

    /// File logging level.
    pub file_log_level: String,

    /// ClickHouse configuration.
    pub clickhouse: ClickHouseConfig,

    /// Trading parameters.
    pub trading: TradingConfig,

    /// Risk management parameters.
    pub risk: RiskConfig,

    /// Shadow bid parameters.
    pub shadow: ShadowConfig,

    /// Execution parameters.
    pub execution: ExecutionConfig,

    /// Observability configuration.
    pub observability: ObservabilityConfig,

    /// Dashboard configuration (React trading dashboard).
    pub dashboard: DashboardConfig,

    /// Wallet configuration (for live trading).
    pub wallet: WalletConfig,

    /// Backtest-specific configuration.
    pub backtest: BacktestConfig,

    /// Live mode-specific configuration.
    pub live: LiveConfig,

    /// Engine configuration (arbitrage, directional, maker).
    pub engines: EnginesConfig,

    /// Phase-based strategy configuration.
    pub phases: PhaseConfig,
}

/// Trading mode determines data source and executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradingMode {
    /// Real trading with real money.
    Live,
    /// Real data, simulated execution.
    Paper,
    /// Real data, log-only (no execution).
    Shadow,
    /// Historical data replay.
    Backtest,
}

impl TradingMode {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "live" => Some(TradingMode::Live),
            "paper" => Some(TradingMode::Paper),
            "shadow" => Some(TradingMode::Shadow),
            "backtest" => Some(TradingMode::Backtest),
            _ => None,
        }
    }
}

impl std::fmt::Display for TradingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TradingMode::Live => write!(f, "live"),
            TradingMode::Paper => write!(f, "paper"),
            TradingMode::Shadow => write!(f, "shadow"),
            TradingMode::Backtest => write!(f, "backtest"),
        }
    }
}

/// Trading parameters for arbitrage detection and sizing.
#[derive(Debug, Clone)]
pub struct TradingConfig {
    /// Minimum arbitrage margin to trade (early window, >5 min remaining).
    pub min_margin_early: Decimal,

    /// Minimum arbitrage margin to trade (mid window, 2-5 min remaining).
    pub min_margin_mid: Decimal,

    /// Minimum arbitrage margin to trade (late window, <2 min remaining).
    pub min_margin_late: Decimal,

    /// Minimum time remaining in window to trade (seconds).
    pub min_time_remaining_secs: u64,

    /// Maximum exposure per market (USDC). Hard cap on position size per market.
    pub max_market_exposure: Decimal,

    /// Maximum total exposure across all markets (USDC).
    pub max_total_exposure: Decimal,

    /// Base order size (USDC). Target size for each order (both arb and directional).
    pub base_order_size: Decimal,

    /// Minimum order size (USDC). Polymarket minimum is ~$1.
    pub min_order_size: Decimal,

    /// Total available balance for trading (USDC). Used for allowance management.
    pub available_balance: Decimal,

    /// Window time boundary for "early" phase (seconds).
    pub early_threshold_secs: u64,

    /// Window time boundary for "mid" phase (seconds).
    pub mid_threshold_secs: u64,
}

impl Default for TradingConfig {
    fn default() -> Self {
        // Default to 15-minute config for backwards compatibility
        Self::for_fifteen_min()
    }
}

impl TradingConfig {
    /// Config preset for 5-minute markets (fee-free).
    ///
    /// Lower thresholds since no fees. Faster phase transitions.
    /// - Early: >90s remaining (>1.5 min)
    /// - Mid: 30-90s remaining (0.5-1.5 min)
    /// - Late: <30s remaining
    pub fn for_five_min() -> Self {
        Self {
            // Lower thresholds - no fees to overcome
            min_margin_early: Decimal::new(10, 3),    // 1.0%
            min_margin_mid: Decimal::new(5, 3),       // 0.5%
            min_margin_late: Decimal::new(25, 4),     // 0.25%
            min_time_remaining_secs: 10,              // 10 seconds minimum
            max_market_exposure: Decimal::new(1000, 0),
            max_total_exposure: Decimal::new(5000, 0),
            base_order_size: Decimal::new(50, 0),
            min_order_size: Decimal::ONE,
            available_balance: Decimal::new(5000, 0),
            // 5-minute window phases (300 seconds total)
            early_threshold_secs: 90,  // >1.5 min = early
            mid_threshold_secs: 30,    // 30s-1.5min = mid, <30s = late
        }
    }

    /// Config preset for 15-minute markets (with fees/rebates).
    ///
    /// Higher thresholds to overcome ~2-3% fee drag.
    /// - Early: >300s remaining (>5 min)
    /// - Mid: 120-300s remaining (2-5 min)
    /// - Late: <120s remaining (<2 min)
    pub fn for_fifteen_min() -> Self {
        Self {
            // Higher thresholds - need to beat ~2-3% taker fees
            min_margin_early: Decimal::new(40, 3),    // 4.0% (conservative for fees)
            min_margin_mid: Decimal::new(30, 3),      // 3.0%
            min_margin_late: Decimal::new(20, 3),     // 2.0% (still above fee drag)
            min_time_remaining_secs: 30,
            max_market_exposure: Decimal::new(1000, 0),
            max_total_exposure: Decimal::new(5000, 0),
            base_order_size: Decimal::new(50, 0),
            min_order_size: Decimal::ONE,
            available_balance: Decimal::new(5000, 0),
            // 15-minute window phases (900 seconds total)
            early_threshold_secs: 300, // >5 min = early
            mid_threshold_secs: 120,   // 2-5 min = mid, <2 min = late
        }
    }

    /// Config preset for 1-hour markets (fee-free).
    ///
    /// Lower thresholds since no fees. Longer phase transitions.
    /// - Early: >1200s remaining (>20 min)
    /// - Mid: 600-1200s remaining (10-20 min)
    /// - Late: <600s remaining (<10 min)
    pub fn for_one_hour() -> Self {
        Self {
            // Lower thresholds - no fees to overcome
            min_margin_early: Decimal::new(15, 3),    // 1.5%
            min_margin_mid: Decimal::new(10, 3),      // 1.0%
            min_margin_late: Decimal::new(5, 3),      // 0.5%
            min_time_remaining_secs: 60,              // 60 seconds minimum
            max_market_exposure: Decimal::new(1000, 0),
            max_total_exposure: Decimal::new(5000, 0),
            base_order_size: Decimal::new(50, 0),
            min_order_size: Decimal::ONE,
            available_balance: Decimal::new(5000, 0),
            // 1-hour window phases (3600 seconds total)
            early_threshold_secs: 1200,  // >20 min = early
            mid_threshold_secs: 600,     // 10-20 min = mid, <10 min = late
        }
    }

    /// Get the appropriate preset for a window duration.
    pub fn for_window_duration(duration: poly_common::WindowDuration) -> Self {
        match duration {
            poly_common::WindowDuration::FiveMin => Self::for_five_min(),
            poly_common::WindowDuration::FifteenMin => Self::for_fifteen_min(),
            poly_common::WindowDuration::OneHour => Self::for_one_hour(),
        }
    }
}

/// Risk management parameters.
#[derive(Debug, Clone)]
pub struct RiskConfig {
    /// Risk management mode: "circuit_breaker", "daily_pnl", or "both".
    ///
    /// - `circuit_breaker`: Lock-free atomic checks for consecutive failures (fastest).
    /// - `daily_pnl`: P&L-based checks (daily loss, consecutive losses, hedge ratio).
    /// - `both`: Combines both strategies (recommended for production).
    pub risk_mode: String,

    /// Maximum consecutive failures before circuit breaker trips.
    pub max_consecutive_failures: u32,

    /// Cooldown after circuit breaker trip (seconds).
    pub circuit_breaker_cooldown_secs: u64,

    /// Maximum daily loss before stopping (USDC).
    pub max_daily_loss: Decimal,

    /// Maximum inventory imbalance ratio (0.0-1.0).
    pub max_imbalance_ratio: Decimal,

    /// Toxic flow severity threshold to skip trade (0-100).
    pub toxic_flow_threshold: u8,

    /// Emergency close threshold for leg risk (USDC).
    pub emergency_close_threshold: Decimal,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            risk_mode: "both".to_string(),
            max_consecutive_failures: 3,
            circuit_breaker_cooldown_secs: 300, // 5 minutes
            max_daily_loss: Decimal::new(500, 0), // $500
            max_imbalance_ratio: Decimal::new(7, 1), // 0.7
            toxic_flow_threshold: 80,
            emergency_close_threshold: Decimal::new(200, 0), // $200
        }
    }
}

/// Shadow bid configuration.
#[derive(Debug, Clone)]
pub struct ShadowConfig {
    /// Enable shadow bidding.
    pub enabled: bool,

    /// Shadow bid price offset from primary (basis points).
    pub price_offset_bps: u32,

    /// Maximum time to wait for shadow fill (milliseconds).
    pub max_wait_ms: u64,
}

impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            price_offset_bps: 50, // 0.5%
            max_wait_ms: 2000,    // 2 seconds
        }
    }
}

/// Order execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    /// Use limit orders with price chasing (maker-first, then chase to fill).
    /// Places GTC order, waits for fill, bumps price incrementally if needed.
    #[default]
    Limit,

    /// Use market orders (taker, crosses spread immediately).
    /// Faster fills but pays taker fees.
    Market,
}

impl ExecutionMode {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "limit" | "maker" | "chase" => Some(ExecutionMode::Limit),
            "market" | "taker" | "ioc" => Some(ExecutionMode::Market),
            _ => None,
        }
    }
}

impl std::fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutionMode::Limit => write!(f, "limit"),
            ExecutionMode::Market => write!(f, "market"),
        }
    }
}

/// Execution configuration.
#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    /// Order execution mode: "limit" (maker with chase) or "market" (taker).
    pub execution_mode: ExecutionMode,

    /// Enable price chasing (only applies to Limit mode).
    pub chase_enabled: bool,

    /// Chase step size (price increment per iteration).
    pub chase_step_size: Decimal,

    /// Chase check interval (milliseconds).
    pub chase_check_interval_ms: u64,

    /// Maximum chase time (milliseconds).
    pub max_chase_time_ms: u64,

    /// Simulated fill latency for paper trading (milliseconds).
    pub paper_fill_latency_ms: u64,

    /// Order timeout (milliseconds).
    pub order_timeout_ms: u64,

    /// Fire a test trade on startup to verify order flow (live mode only).
    pub test_trade_on_startup: bool,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            execution_mode: ExecutionMode::Limit,
            chase_enabled: true,
            chase_step_size: Decimal::new(1, 3), // 0.001
            chase_check_interval_ms: 100,
            max_chase_time_ms: 5000,
            paper_fill_latency_ms: 50,
            order_timeout_ms: 10000,
            test_trade_on_startup: false,
        }
    }
}

/// Observability configuration with enable/disable flags.
#[derive(Debug, Clone)]
pub struct ObservabilityConfig {
    /// Enable decision capture.
    pub capture_decisions: bool,

    /// Enable counterfactual analysis.
    pub capture_counterfactuals: bool,

    /// Enable anomaly detection.
    pub detect_anomalies: bool,

    /// Channel buffer size for fire-and-forget capture.
    pub channel_buffer_size: usize,

    /// Batch size for ClickHouse writes.
    pub batch_size: usize,

    /// Flush interval (seconds).
    pub flush_interval_secs: u64,

    /// Enable alert webhooks.
    pub alerts_enabled: bool,

    /// Webhook URL for alerts (optional).
    pub alert_webhook_url: Option<String>,
}

/// Dashboard configuration for the React trading dashboard.
///
/// Controls the capture channel, background processor, and WebSocket server
/// for real-time dashboard updates.
#[derive(Debug, Clone)]
pub struct DashboardConfig {
    /// Enable dashboard capture and streaming.
    pub enabled: bool,

    /// Channel capacity for fire-and-forget capture.
    /// Events are dropped when channel is full.
    pub channel_capacity: usize,

    /// Whether to log dropped events.
    pub log_drops: bool,

    /// Minimum drop count before logging (to avoid spam).
    pub drop_log_threshold: u64,

    /// Batch size for ClickHouse writes.
    pub batch_size: usize,

    /// Flush interval in seconds.
    pub flush_interval_secs: u64,

    /// Maximum buffer size before dropping oldest events.
    pub max_buffer_size: usize,

    /// WebSocket server port for real-time streaming.
    pub websocket_port: u16,

    /// REST API port for historical queries.
    pub api_port: u16,

    /// State broadcast interval in milliseconds.
    pub broadcast_interval_ms: u64,

    /// P&L snapshot interval in seconds.
    pub pnl_snapshot_interval_secs: u64,

    /// Path to the frontend dist/ directory for static file serving.
    /// If set, the API server will serve the React dashboard at the root path.
    /// If None, only the REST API endpoints will be available.
    pub static_dir: Option<String>,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            channel_capacity: 1024,
            log_drops: true,
            drop_log_threshold: 100,
            batch_size: 100,
            flush_interval_secs: 5,
            max_buffer_size: 10000,
            websocket_port: 3001,
            api_port: 3002,
            broadcast_interval_ms: 500,
            pnl_snapshot_interval_secs: 60,
            static_dir: None,
        }
    }
}

impl DashboardConfig {
    /// Create a disabled dashboard config.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Default::default()
        }
    }

    /// Convert to capture config.
    pub fn capture_config(&self) -> crate::dashboard::DashboardCaptureConfig {
        crate::dashboard::DashboardCaptureConfig {
            enabled: self.enabled,
            channel_capacity: self.channel_capacity,
            log_drops: self.log_drops,
            drop_log_threshold: self.drop_log_threshold,
        }
    }

    /// Convert to processor config.
    pub fn processor_config(&self) -> crate::dashboard::DashboardProcessorConfig {
        crate::dashboard::DashboardProcessorConfig {
            batch_size: self.batch_size,
            flush_interval: Duration::from_secs(self.flush_interval_secs),
            max_buffer_size: self.max_buffer_size,
        }
    }
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            capture_decisions: true,
            capture_counterfactuals: true,
            detect_anomalies: true,
            channel_buffer_size: 10000,
            batch_size: 1000,
            flush_interval_secs: 5,
            alerts_enabled: false,
            alert_webhook_url: None,
        }
    }
}

/// Wallet configuration for live trading.
#[derive(Debug, Clone, Default)]
pub struct WalletConfig {
    /// Private key (loaded from env var, never in config file).
    pub private_key: Option<String>,

    /// Polymarket API key.
    pub api_key: Option<String>,

    /// Polymarket API secret.
    pub api_secret: Option<String>,

    /// Polymarket API passphrase.
    pub api_passphrase: Option<String>,
}

/// Backtest configuration.
#[derive(Debug, Clone)]
pub struct BacktestConfig {
    /// Start date for backtest (YYYY-MM-DD).
    pub start_date: Option<String>,

    /// End date for backtest (YYYY-MM-DD).
    pub end_date: Option<String>,

    /// Duration limit for backtest (e.g., "2h", "30m", "1d").
    /// Can be used alone (first N of data) or with start_date.
    pub duration: Option<String>,

    /// Playback speed multiplier (1.0 = real-time, 0 = max speed).
    pub speed: f64,

    /// Enable parameter sweep mode.
    pub sweep_enabled: bool,

    /// Data directory for CSV replay (if set, uses CSV instead of ClickHouse).
    pub data_dir: Option<String>,

    /// Number of parallel workers for sweep mode (0 or None = number of CPU cores).
    pub sweep_parallel_workers: Option<usize>,

    /// Optional trading config specific to backtest mode.
    /// If present, backtest uses these instead of the main [trading] config.
    /// This allows keeping stable backtest parameters for comparing commits.
    pub trading: Option<TradingConfig>,

    /// Path to write decision log CSV (for comparing live vs backtest).
    pub decision_log_path: Option<String>,

    /// Sweep parameters (from [backtest.sweep] in config TOML).
    pub sweep_params: SweepConfig,

    /// Minimum interval between orderbook snapshots per event (ms).
    /// Downsamples L2 data for faster backtesting. E.g. 1000 = 1 snapshot/sec.
    pub book_interval_ms: Option<u64>,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            start_date: None,
            end_date: None,
            duration: None,
            speed: 0.0, // Max speed by default
            sweep_enabled: false,
            data_dir: None,
            sweep_parallel_workers: None,
            trading: None,
            decision_log_path: None,
            sweep_params: SweepConfig::default(),
            book_interval_ms: None,
        }
    }
}

/// Live mode configuration.
#[derive(Debug, Clone)]
pub struct LiveConfig {
    /// Interval in minutes between auto-claim attempts for resolved positions.
    /// Set to 0 to disable auto-claiming.
    pub auto_claim_interval: u64,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            auto_claim_interval: 30, // Check every 30 minutes by default
        }
    }
}

/// Phase-based budget allocation configuration.
///
/// Controls budget allocation per market phase.
#[derive(Debug, Clone)]
pub struct PhaseConfig {
    /// Budget allocation for early phase (0.0-1.0).
    pub early_budget: Decimal,
    /// Budget allocation for build phase (0.0-1.0).
    pub build_budget: Decimal,
    /// Budget allocation for core phase (0.0-1.0).
    pub core_budget: Decimal,
    /// Budget allocation for final phase (0.0-1.0).
    pub final_budget: Decimal,
}

impl Default for PhaseConfig {
    fn default() -> Self {
        Self {
            // Standard budget allocation
            early_budget: Decimal::new(15, 2),     // 0.15 = 15%
            build_budget: Decimal::new(25, 2),     // 0.25 = 25%
            core_budget: Decimal::new(30, 2),      // 0.30 = 30%
            final_budget: Decimal::new(30, 2),     // 0.30 = 30%
        }
    }
}

impl PhaseConfig {
    /// Get budget allocation for a given phase.
    pub fn budget_for_phase(&self, phase: &str) -> Decimal {
        match phase {
            "early" => self.early_budget,
            "build" => self.build_budget,
            "core" => self.core_budget,
            "final" => self.final_budget,
            _ => Decimal::ZERO,
        }
    }
}

// ============================================================================
// Engine Configuration
// ============================================================================

/// Configuration for all trading engines.
///
/// The bot supports three trading engines that can run in parallel:
/// 1. **Arbitrage**: Buy both YES+NO when combined cost < $1 (risk-free profit)
/// 2. **Directional**: Signal-based trading with asymmetric allocations
/// 3. **Maker Rebates**: Passive limit orders to earn market maker rebates
///
/// Engines are evaluated in priority order, with higher-priority engines
/// taking precedence when conflicts occur (e.g., arb opportunity during directional).
#[derive(Debug, Clone)]
pub struct EnginesConfig {
    /// Arbitrage engine configuration.
    pub arbitrage: ArbitrageEngineConfig,

    /// Directional trading engine configuration.
    pub directional: DirectionalEngineConfig,

    /// Maker rebates engine configuration.
    pub maker: MakerEngineConfig,

    /// Engine priority for conflict resolution.
    /// Lower index = higher priority. First engine wins on conflicts.
    /// Default: [Arbitrage, Directional, MakerRebates]
    pub priority: Vec<EngineType>,
}

impl Default for EnginesConfig {
    fn default() -> Self {
        Self {
            arbitrage: ArbitrageEngineConfig::default(),
            directional: DirectionalEngineConfig::default(),
            maker: MakerEngineConfig::default(),
            // Arbitrage has highest priority (risk-free), then directional, then maker
            priority: vec![
                EngineType::Arbitrage,
                EngineType::Directional,
                EngineType::MakerRebates,
            ],
        }
    }
}

impl EnginesConfig {
    /// Create a config with only arbitrage enabled (backward compatible).
    pub fn arbitrage_only() -> Self {
        Self {
            arbitrage: ArbitrageEngineConfig {
                enabled: true, // Explicitly enable for arb-only mode
                ..Default::default()
            },
            directional: DirectionalEngineConfig {
                enabled: false,
                ..Default::default()
            },
            maker: MakerEngineConfig {
                enabled: false,
                ..Default::default()
            },
            priority: vec![EngineType::Arbitrage],
        }
    }

    /// Check if any engine is enabled.
    pub fn any_enabled(&self) -> bool {
        self.arbitrage.enabled || self.directional.enabled || self.maker.enabled
    }

    /// Get list of enabled engines in priority order.
    pub fn enabled_engines(&self) -> Vec<EngineType> {
        self.priority
            .iter()
            .filter(|e| match e {
                EngineType::Arbitrage => self.arbitrage.enabled,
                EngineType::Directional => self.directional.enabled,
                EngineType::MakerRebates => self.maker.enabled,
            })
            .copied()
            .collect()
    }

    /// Get the priority rank of an engine (0 = highest).
    /// Returns None if engine is not in priority list.
    pub fn get_priority(&self, engine: EngineType) -> Option<usize> {
        self.priority.iter().position(|e| *e == engine)
    }
}

/// Configuration for the arbitrage engine.
///
/// The arbitrage engine detects risk-free profit opportunities when
/// YES + NO shares can be bought for less than $1.00 combined.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbitrageEngineConfig {
    /// Whether the arbitrage engine is enabled.
    pub enabled: bool,

    /// Minimum margin (bps) for early phase (>5 min remaining).
    /// Higher margins required early due to uncertainty.
    pub min_margin_early_bps: u32,

    /// Minimum margin (bps) for mid phase (2-5 min remaining).
    pub min_margin_mid_bps: u32,

    /// Minimum margin (bps) for late phase (<2 min remaining).
    /// Lower margins acceptable as outcome becomes certain.
    pub min_margin_late_bps: u32,

    /// Minimum confidence score (0-100) to accept opportunity.
    pub min_confidence: u8,

    /// Time boundary (seconds) between early and mid phase.
    pub early_threshold_secs: u64,

    /// Time boundary (seconds) between mid and late phase.
    pub mid_threshold_secs: u64,
}

impl Default for ArbitrageEngineConfig {
    fn default() -> Self {
        Self {
            enabled: false, // Disabled: taker fees (~3%) exceed typical arb margins (~2.5%)
            min_margin_early_bps: 250, // 2.5%
            min_margin_mid_bps: 150,   // 1.5%
            min_margin_late_bps: 50,   // 0.5%
            min_confidence: 70,
            early_threshold_secs: 300, // 5 minutes
            mid_threshold_secs: 120,   // 2 minutes
        }
    }
}

/// Configuration for the directional trading engine.
///
/// The directional engine uses price signals (spot vs strike) to make
/// asymmetric bets while still buying both sides for guaranteed payout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectionalEngineConfig {
    /// Whether the directional engine is enabled.
    pub enabled: bool,

    /// Minimum time remaining (seconds) to consider directional trades.
    pub min_seconds_remaining: u64,

    /// Maximum combined cost (as ratio of $1.00) to accept trade.
    /// E.g., 0.995 means combined YES+NO cost must be < $0.995.
    pub max_combined_cost: Decimal,

    /// Maximum spread (bps) as ratio of price to accept.
    /// E.g., 1000 = 10% maximum spread.
    pub max_spread_bps: u32,

    /// Minimum depth (USDC) required on favorable side.
    pub min_favorable_depth: Decimal,

    // --- Signal Thresholds ---
    // The following thresholds control when signals are generated.
    // These are distance from strike as percentage of strike price.

    /// Strong signal threshold for late window (<1 min).
    /// Default: 0.03% (30 bps)
    pub strong_threshold_late_bps: u32,

    /// Lean signal threshold for late window (<1 min).
    /// Default: 0.01% (10 bps)
    pub lean_threshold_late_bps: u32,

    // --- Allocation Ratios ---
    // Ratios for UP allocation based on signal strength.

    /// UP allocation for StrongUp signal (as ratio 0-1).
    /// Default: 0.78 (78% UP, 22% DOWN)
    pub strong_up_ratio: Decimal,

    /// UP allocation for LeanUp signal (as ratio 0-1).
    /// Default: 0.60 (60% UP, 40% DOWN)
    pub lean_up_ratio: Decimal,

    /// UP allocation for Neutral signal (as ratio 0-1).
    /// Default: 0.50 (50% UP, 50% DOWN)
    pub neutral_ratio: Decimal,

    // --- Confidence / Edge Params ---
    // Optional overrides for strategy-level confidence params.
    // If None, code defaults are used.

    /// Minimum time confidence at window start.
    pub time_conf_floor: Option<Decimal>,
    /// Minimum distance confidence for tiny moves.
    pub dist_conf_floor: Option<Decimal>,
    /// Confidence gained per ATR of movement.
    pub dist_conf_per_atr: Option<Decimal>,
    /// Maximum edge required at window start (decays to 0).
    pub max_edge_factor: Option<Decimal>,
}

impl Default for DirectionalEngineConfig {
    fn default() -> Self {
        Self {
            enabled: false, // Disabled by default (opt-in)
            min_seconds_remaining: 60,
            max_combined_cost: Decimal::new(995, 3), // 0.995
            max_spread_bps: 1000, // 10%
            min_favorable_depth: Decimal::new(100, 0), // $100
            // Signal thresholds (in bps of strike)
            strong_threshold_late_bps: 30, // 0.03%
            lean_threshold_late_bps: 10,   // 0.01%
            // Allocation ratios
            strong_up_ratio: Decimal::new(78, 2),  // 0.78
            lean_up_ratio: Decimal::new(60, 2),    // 0.60
            neutral_ratio: Decimal::new(50, 2),    // 0.50
            time_conf_floor: None,
            dist_conf_floor: None,
            dist_conf_per_atr: None,
            max_edge_factor: None,
        }
    }
}

impl DirectionalEngineConfig {
    /// Get the DOWN ratio for a given UP ratio.
    #[inline]
    pub fn down_ratio_for(&self, up_ratio: Decimal) -> Decimal {
        Decimal::ONE - up_ratio
    }

    /// Calculate the required confidence for a trade at the given price.
    ///
    /// Since EV = confidence - price, we need confidence > price to have positive EV.
    ///
    /// # Arguments
    /// * `favorable_price` - The price of the side we're buying (0-1)
    ///
    /// # Returns
    /// The minimum confidence required to trade (equal to the price)
    #[inline]
    pub fn required_confidence(&self, favorable_price: Decimal) -> Decimal {
        favorable_price
    }

    /// Get allocation ratios for StrongUp signal.
    #[inline]
    pub fn strong_up_allocation(&self) -> (Decimal, Decimal) {
        (self.strong_up_ratio, self.down_ratio_for(self.strong_up_ratio))
    }

    /// Get allocation ratios for LeanUp signal.
    #[inline]
    pub fn lean_up_allocation(&self) -> (Decimal, Decimal) {
        (self.lean_up_ratio, self.down_ratio_for(self.lean_up_ratio))
    }

    /// Get allocation ratios for StrongDown signal.
    /// Inverts the StrongUp ratios.
    #[inline]
    pub fn strong_down_allocation(&self) -> (Decimal, Decimal) {
        let (up, down) = self.strong_up_allocation();
        (down, up)
    }

    /// Get allocation ratios for LeanDown signal.
    /// Inverts the LeanUp ratios.
    #[inline]
    pub fn lean_down_allocation(&self) -> (Decimal, Decimal) {
        let (up, down) = self.lean_up_allocation();
        (down, up)
    }
}

/// Configuration for the maker rebates engine.
///
/// The maker engine places passive limit orders to earn rebates from
/// Polymarket's maker rewards program.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MakerEngineConfig {
    /// Whether the maker engine is enabled.
    pub enabled: bool,

    /// Minimum time remaining (seconds) to place maker orders.
    /// Orders need time to fill before window closes.
    pub min_seconds_remaining: u64,

    /// Minimum spread (bps) to place maker orders.
    /// Below this, there's no room for profitable maker placement.
    pub min_spread_bps: u32,

    /// Maximum spread (bps) for maker strategy.
    /// Above this, there's too much uncertainty.
    pub max_spread_bps: u32,

    /// Default order size (USDC) for maker orders.
    pub default_order_size: Decimal,

    /// Maximum book imbalance ratio (0-1) before skipping.
    /// E.g., 0.80 means skip if one side is >80% of total.
    pub max_imbalance_ratio: Decimal,

    /// How far inside spread to place maker orders (bps from BBO).
    /// E.g., 10 means place 10 bps inside the current bid/ask.
    pub spread_inside_bps: u32,

    /// Stale order threshold (milliseconds).
    /// Orders older than this will be refreshed.
    pub stale_threshold_ms: u64,

    /// Price change threshold (bps) to refresh order.
    /// If optimal price moved more than this, refresh the order.
    pub price_refresh_threshold_bps: u32,

    /// Maximum concurrent maker orders per market.
    pub max_orders_per_market: u32,
}

impl Default for MakerEngineConfig {
    fn default() -> Self {
        Self {
            enabled: false, // Disabled by default (opt-in)
            min_seconds_remaining: 120, // 2 minutes
            min_spread_bps: 50,         // 0.5%
            max_spread_bps: 1000,       // 10%
            default_order_size: Decimal::new(50, 0), // $50
            max_imbalance_ratio: Decimal::new(80, 2), // 0.80
            spread_inside_bps: 10,       // 0.1% inside spread
            stale_threshold_ms: 5000,    // 5 seconds
            price_refresh_threshold_bps: 20, // 0.2%
            max_orders_per_market: 4,
        }
    }
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            mode: TradingMode::Shadow,
            assets: vec!["BTC".to_string(), "ETH".to_string(), "SOL".to_string()],
            window_duration: WindowDuration::OneHour, // Default to 1h since 15min not available
            console_log_level: "info".to_string(),
            file_log_level: "debug".to_string(),
            clickhouse: ClickHouseConfig::default(),
            trading: TradingConfig::default(),
            risk: RiskConfig::default(),
            shadow: ShadowConfig::default(),
            execution: ExecutionConfig::default(),
            observability: ObservabilityConfig::default(),
            dashboard: DashboardConfig::default(),
            wallet: WalletConfig::default(),
            backtest: BacktestConfig::default(),
            live: LiveConfig::default(),
            engines: EnginesConfig::default(),
            phases: PhaseConfig::default(),
        }
    }
}

impl BotConfig {
    /// Load configuration from a TOML file.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read config file: {:?}", path.as_ref()))?;
        Self::from_toml_str(&content)
    }

    /// Parse configuration from TOML string.
    pub fn from_toml_str(content: &str) -> Result<Self> {
        let file: TomlConfig = toml::from_str(content).context("Failed to parse TOML config")?;
        Self::try_from_toml(file)
    }

    /// Returns the effective trading config for the current mode.
    ///
    /// In backtest mode, returns `backtest.trading` if configured, otherwise
    /// falls back to the main `trading` config. This allows keeping stable
    /// backtest parameters for comparing across commits.
    pub fn effective_trading_config(&self) -> &TradingConfig {
        if self.mode == TradingMode::Backtest {
            self.backtest.trading.as_ref().unwrap_or(&self.trading)
        } else {
            &self.trading
        }
    }

    /// Apply environment variable overrides for sensitive values.
    pub fn apply_env_overrides(&mut self) {
        // Wallet credentials from environment
        if let Ok(key) = std::env::var("POLY_PRIVATE_KEY") {
            self.wallet.private_key = Some(key);
        }
        if let Ok(key) = std::env::var("POLY_API_KEY") {
            self.wallet.api_key = Some(key);
        }
        if let Ok(secret) = std::env::var("POLY_API_SECRET") {
            self.wallet.api_secret = Some(secret);
        }
        if let Ok(pass) = std::env::var("POLY_API_PASSPHRASE") {
            self.wallet.api_passphrase = Some(pass);
        }

        // ClickHouse credentials
        // Support both full URL and separate host/port
        if let Ok(url) = std::env::var("CLICKHOUSE_URL") {
            // If URL already has protocol and port, use as-is
            if url.starts_with("http://") || url.starts_with("https://") {
                self.clickhouse.url = url;
            } else {
                // Construct URL from host and HTTP port
                // Note: CLICKHOUSE_HTTP_PORT is for HTTP interface (default 8123)
                let port = std::env::var("CLICKHOUSE_HTTP_PORT").unwrap_or_else(|_| "8123".to_string());
                self.clickhouse.url = format!("http://{}:{}", url, port);
            }
        }
        if let Ok(user) = std::env::var("CLICKHOUSE_USER") {
            self.clickhouse.user = Some(user);
        }
        if let Ok(pass) = std::env::var("CLICKHOUSE_PASSWORD") {
            self.clickhouse.password = Some(pass);
        }
        if let Ok(database) = std::env::var("CLICKHOUSE_DATABASE") {
            self.clickhouse.database = database;
        }

        // Alert webhook
        if let Ok(url) = std::env::var("ALERT_WEBHOOK_URL") {
            self.observability.alert_webhook_url = Some(url);
            self.observability.alerts_enabled = true;
        }
    }

    /// Apply CLI argument overrides.
    pub fn apply_cli_overrides(
        &mut self,
        mode: Option<String>,
        assets: Option<Vec<String>>,
        clickhouse_url: Option<String>,
    ) {
        if let Some(mode_str) = mode
            && let Some(m) = TradingMode::from_str(&mode_str)
        {
            self.mode = m;
        }

        if let Some(asset_list) = assets
            && !asset_list.is_empty()
        {
            self.assets = asset_list;
        }

        if let Some(url) = clickhouse_url {
            self.clickhouse.url = url;
        }
    }

    /// Validate configuration and return errors for invalid values.
    pub fn validate(&self) -> Result<()> {
        // Mode-specific validation
        // Note: API credentials (api_key, api_secret, api_passphrase) are optional for live mode.
        // If not provided, they will be derived from the private key at startup.
        if self.mode == TradingMode::Live
            && self.wallet.private_key.is_none()
        {
            bail!("Live mode requires POLY_PRIVATE_KEY environment variable");
        }

        // Note: start_date and end_date are optional for backtest mode
        // If not specified, all data in the data_dir will be used

        // Trading config validation
        if self.trading.min_margin_early <= Decimal::ZERO {
            bail!("min_margin_early must be positive");
        }
        if self.trading.max_market_exposure <= Decimal::ZERO {
            bail!("max_market_exposure must be positive");
        }
        if self.trading.max_total_exposure <= Decimal::ZERO {
            bail!("max_total_exposure must be positive");
        }
        if self.trading.max_market_exposure > self.trading.max_total_exposure {
            bail!("max_market_exposure cannot exceed max_total_exposure");
        }

        // Risk config validation
        if self.risk.max_consecutive_failures == 0 {
            bail!("max_consecutive_failures must be at least 1");
        }
        if self.risk.max_imbalance_ratio <= Decimal::ZERO
            || self.risk.max_imbalance_ratio > Decimal::ONE
        {
            bail!("max_imbalance_ratio must be between 0 and 1");
        }

        // Assets validation
        if self.assets.is_empty() {
            bail!("At least one asset must be configured");
        }

        Ok(())
    }
}

// ============================================================================
// Strategy Configuration (from strategy.toml)
// ============================================================================

/// Sweep configuration loaded from strategy.toml.
#[derive(Debug, Clone, Default)]
pub struct SweepConfig {
    /// Base order size values to sweep (key parameter for trade frequency).
    pub base_order_sizes: Vec<f64>,
    /// Strong UP ratio values to sweep.
    pub strong_ratios: Vec<f64>,
    /// Lean UP ratio values to sweep.
    pub lean_ratios: Vec<f64>,
    /// Time confidence floor values to sweep.
    /// This is the minimum confidence from time at window start.
    pub time_conf_floors: Vec<f64>,
    /// Distance confidence floor values to sweep.
    /// This is the minimum confidence for tiny moves.
    pub dist_conf_floors: Vec<f64>,
    /// Distance confidence per ATR values to sweep.
    /// This is the confidence gained per ATR of movement.
    pub dist_conf_per_atrs: Vec<f64>,
    /// Edge factor values to sweep (quality filter).
    /// This is the minimum EV required at window start.
    pub edge_factors: Vec<f64>,
}

impl SweepConfig {
    /// Load sweep configuration from strategy.toml.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read strategy config: {:?}", path.as_ref()))?;
        Self::from_toml_str(&content)
    }

    /// Parse sweep configuration from TOML string.
    pub fn from_toml_str(content: &str) -> Result<Self> {
        let toml: StrategyToml = toml::from_str(content)
            .context("Failed to parse strategy.toml")?;
        Ok(Self {
            base_order_sizes: toml.sweep.base_order_sizes,
            strong_ratios: toml.sweep.strong_ratios,
            lean_ratios: toml.sweep.lean_ratios,
            time_conf_floors: toml.sweep.time_conf_floors,
            dist_conf_floors: toml.sweep.dist_conf_floors,
            dist_conf_per_atrs: toml.sweep.dist_conf_per_atrs,
            edge_factors: toml.sweep.edge_factors,
        })
    }

    /// Convert to SweepParameter list for backtest mode.
    pub fn to_sweep_parameters(&self) -> Vec<crate::mode::backtest::SweepParameter> {
        use crate::mode::backtest::SweepParameter;
        let mut params = Vec::new();

        // Helper to create sweep param from values
        fn make_param(name: &str, values: &[f64]) -> Option<SweepParameter> {
            if values.len() > 1 {
                let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
                let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let step = (max - min) / (values.len() - 1) as f64;
                Some(SweepParameter::new(name, min, max, step))
            } else {
                None
            }
        }

        // Base order size (key parameter for trade frequency)
        if let Some(p) = make_param("base_order_size", &self.base_order_sizes) {
            params.push(p);
        }

        // Allocation ratios
        if let Some(p) = make_param("strong_up_ratio", &self.strong_ratios) {
            params.push(p);
        }
        if let Some(p) = make_param("lean_up_ratio", &self.lean_ratios) {
            params.push(p);
        }

        // Confidence calculation params
        if let Some(p) = make_param("time_conf_floor", &self.time_conf_floors) {
            params.push(p);
        }
        if let Some(p) = make_param("dist_conf_floor", &self.dist_conf_floors) {
            params.push(p);
        }
        if let Some(p) = make_param("dist_conf_per_atr", &self.dist_conf_per_atrs) {
            params.push(p);
        }

        // Edge factor (quality filter)
        if let Some(p) = make_param("max_edge_factor", &self.edge_factors) {
            params.push(p);
        }

        params
    }
}

/// TOML structure for strategy.toml.
#[derive(Debug, Deserialize)]
struct StrategyToml {
    #[serde(default)]
    sweep: SweepToml,
}

/// Sweep section in strategy.toml.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct SweepToml {
    base_order_sizes: Vec<f64>,
    strong_ratios: Vec<f64>,
    lean_ratios: Vec<f64>,
    time_conf_floors: Vec<f64>,
    dist_conf_floors: Vec<f64>,
    dist_conf_per_atrs: Vec<f64>,
    edge_factors: Vec<f64>,
}

impl Default for SweepToml {
    fn default() -> Self {
        Self {
            base_order_sizes: Vec::new(),
            strong_ratios: Vec::new(),
            lean_ratios: Vec::new(),
            time_conf_floors: Vec::new(),
            dist_conf_floors: Vec::new(),
            dist_conf_per_atrs: Vec::new(),
            edge_factors: Vec::new(),
        }
    }
}

// ============================================================================
// TOML deserialization structures
// ============================================================================

#[derive(Debug, Deserialize)]
struct TomlConfig {
    #[serde(default)]
    general: GeneralToml,
    #[serde(default)]
    clickhouse: ClickHouseToml,
    #[serde(default)]
    trading: TradingToml,
    #[serde(default)]
    risk: RiskToml,
    #[serde(default)]
    shadow: ShadowToml,
    #[serde(default)]
    execution: ExecutionToml,
    #[serde(default)]
    observability: ObservabilityToml,
    #[serde(default)]
    dashboard: DashboardToml,
    #[serde(default)]
    backtest: BacktestToml,
    #[serde(default)]
    live: LiveToml,
    #[serde(default)]
    engines: EnginesToml,
    #[serde(default)]
    phases: PhasesToml,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct GeneralToml {
    // ===== REQUIRED FIELDS =====
    /// REQUIRED: Trading mode (live, paper, shadow, backtest)
    mode: Option<String>,
    /// REQUIRED: Assets to trade (e.g., ["BTC", "ETH", "SOL"])
    assets: Option<Vec<String>>,
    /// REQUIRED: Market window duration: "15min" or "1h"
    window_duration: Option<String>,

    // ===== OPTIONAL FIELDS =====
    /// Console logging level (default: "info")
    console_log_level: String,
    /// File logging level (default: "debug")
    file_log_level: String,
    /// Legacy: kept for backwards compatibility, maps to console_log_level
    #[serde(default)]
    log_level: Option<String>,
}

impl Default for GeneralToml {
    fn default() -> Self {
        Self {
            // Required (None = must be provided)
            mode: None,
            assets: None,
            window_duration: None,
            // Optional with defaults
            console_log_level: "info".to_string(),
            file_log_level: "debug".to_string(),
            log_level: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ClickHouseToml {
    url: String,
    database: String,
    max_rows: u64,
    max_bytes: u64,
    period_secs: u64,
}

impl Default for ClickHouseToml {
    fn default() -> Self {
        Self {
            url: "http://localhost:8123".to_string(),
            database: "polymarket".to_string(),
            max_rows: 10000,
            max_bytes: 10 * 1024 * 1024,
            period_secs: 5,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct TradingToml {
    // ===== REQUIRED FIELDS =====
    /// REQUIRED: Maximum exposure per market in USDC
    max_market_exposure: Option<f64>,
    /// REQUIRED: Maximum total exposure across all markets in USDC
    max_total_exposure: Option<f64>,
    /// REQUIRED: Base order size in USDC
    base_order_size: Option<f64>,
    /// REQUIRED: Available balance for trading in USDC
    available_balance: Option<f64>,

    // ===== OPTIONAL FIELDS =====
    /// Minimum order size in USDC (default: $1)
    min_order_size: f64,
    min_margin_early_pct: f64,
    min_margin_mid_pct: f64,
    min_margin_late_pct: f64,
    min_time_remaining_secs: u64,
    early_threshold_secs: u64,
    mid_threshold_secs: u64,

    // Legacy field name (deprecated, use max_market_exposure)
    #[serde(default)]
    max_position_per_market: Option<f64>,
}

impl Default for TradingToml {
    fn default() -> Self {
        Self {
            // Required (None = must be provided)
            max_market_exposure: None,
            max_total_exposure: None,
            base_order_size: None,
            available_balance: None,
            // Optional with defaults (for backwards compatibility)
            min_order_size: 1.0,
            min_margin_early_pct: 2.5,
            min_margin_mid_pct: 1.5,
            min_margin_late_pct: 0.5,
            min_time_remaining_secs: 30,
            early_threshold_secs: 300,
            mid_threshold_secs: 120,
            // Legacy
            max_position_per_market: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RiskToml {
    // ===== REQUIRED FIELDS =====
    /// REQUIRED: Maximum daily loss before trading stops (USDC)
    max_daily_loss: Option<f64>,
    /// REQUIRED: Emergency close threshold (ratio, e.g., 0.15 = 15%)
    emergency_close_threshold: Option<f64>,

    // ===== OPTIONAL FIELDS =====
    risk_mode: String,
    max_consecutive_failures: u32,
    circuit_breaker_cooldown_secs: u64,
    max_imbalance_ratio: f64,
    toxic_flow_threshold: u8,
}

impl Default for RiskToml {
    fn default() -> Self {
        Self {
            // Required (None = must be provided)
            max_daily_loss: None,
            emergency_close_threshold: None,
            // Optional with defaults
            risk_mode: "both".to_string(),
            max_consecutive_failures: 3,
            circuit_breaker_cooldown_secs: 300,
            max_imbalance_ratio: 0.7,
            toxic_flow_threshold: 80,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ShadowToml {
    enabled: bool,
    price_offset_bps: u32,
    max_wait_ms: u64,
}

impl Default for ShadowToml {
    fn default() -> Self {
        Self {
            enabled: true,
            price_offset_bps: 50,
            max_wait_ms: 2000,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ExecutionToml {
    // ===== REQUIRED FIELDS =====
    /// REQUIRED: Order execution mode: "limit" or "market"
    execution_mode: Option<String>,

    // ===== OPTIONAL FIELDS =====
    chase_enabled: bool,
    chase_step_size: f64,
    chase_check_interval_ms: u64,
    max_chase_time_ms: u64,
    paper_fill_latency_ms: u64,
    order_timeout_ms: u64,
    test_trade_on_startup: bool,
}

impl Default for ExecutionToml {
    fn default() -> Self {
        Self {
            // Required (None = must be provided)
            execution_mode: None,
            // Optional with defaults
            chase_enabled: true,
            chase_step_size: 0.001,
            chase_check_interval_ms: 100,
            max_chase_time_ms: 5000,
            paper_fill_latency_ms: 50,
            order_timeout_ms: 10000,
            test_trade_on_startup: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ObservabilityToml {
    capture_decisions: bool,
    capture_counterfactuals: bool,
    detect_anomalies: bool,
    channel_buffer_size: usize,
    batch_size: usize,
    flush_interval_secs: u64,
    alerts_enabled: bool,
}

impl Default for ObservabilityToml {
    fn default() -> Self {
        Self {
            capture_decisions: true,
            capture_counterfactuals: true,
            detect_anomalies: true,
            channel_buffer_size: 10000,
            batch_size: 1000,
            flush_interval_secs: 5,
            alerts_enabled: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct DashboardToml {
    enabled: bool,
    channel_capacity: usize,
    log_drops: bool,
    drop_log_threshold: u64,
    batch_size: usize,
    flush_interval_secs: u64,
    max_buffer_size: usize,
    websocket_port: u16,
    api_port: u16,
    broadcast_interval_ms: u64,
    pnl_snapshot_interval_secs: u64,
    static_dir: Option<String>,
}

impl Default for DashboardToml {
    fn default() -> Self {
        Self {
            enabled: true,
            channel_capacity: 1024,
            log_drops: true,
            drop_log_threshold: 100,
            batch_size: 100,
            flush_interval_secs: 5,
            max_buffer_size: 10000,
            websocket_port: 3001,
            api_port: 3002,
            broadcast_interval_ms: 500,
            pnl_snapshot_interval_secs: 60,
            static_dir: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct BacktestToml {
    start_date: Option<String>,
    end_date: Option<String>,
    /// Duration limit (e.g., "2h", "30m", "1d").
    duration: Option<String>,
    speed: f64,
    sweep_enabled: bool,
    data_dir: Option<String>,
    sweep_parallel_workers: Option<usize>,
    /// Optional trading config specific to backtest mode.
    /// If present, backtest uses these instead of [trading].
    trading: Option<TradingToml>,
    /// Path to write decision log CSV.
    decision_log_path: Option<String>,
    /// Sweep parameters (inline in this config).
    #[serde(default)]
    sweep: SweepToml,
    /// Minimum interval between orderbook snapshots per event (ms).
    book_interval_ms: Option<u64>,
}

impl Default for BacktestToml {
    fn default() -> Self {
        Self {
            start_date: None,
            end_date: None,
            duration: None,
            speed: 0.0,
            sweep_enabled: false,
            data_dir: None,
            sweep_parallel_workers: None,
            trading: None,
            decision_log_path: None,
            sweep: SweepToml::default(),
            book_interval_ms: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct LiveToml {
    /// Interval in minutes between auto-claim attempts.
    auto_claim_interval: u64,
}

impl Default for LiveToml {
    fn default() -> Self {
        Self {
            auto_claim_interval: 30,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct PhasesToml {
    early_budget: f64,
    build_budget: f64,
    core_budget: f64,
    final_budget: f64,
}

impl Default for PhasesToml {
    fn default() -> Self {
        Self {
            // Standard budget allocation
            early_budget: 0.15,
            build_budget: 0.25,
            core_budget: 0.30,
            final_budget: 0.30,
        }
    }
}

// --- Engine TOML structures ---

#[derive(Debug, Deserialize)]
#[serde(default)]
struct EnginesToml {
    arbitrage: ArbitrageEngineToml,
    directional: DirectionalEngineToml,
    maker: MakerEngineToml,
    /// Priority order: ["arbitrage", "directional", "maker"]
    priority: Vec<String>,
}

impl Default for EnginesToml {
    fn default() -> Self {
        Self {
            arbitrage: ArbitrageEngineToml::default(),
            directional: DirectionalEngineToml::default(),
            maker: MakerEngineToml::default(),
            priority: vec![
                "arbitrage".to_string(),
                "directional".to_string(),
                "maker".to_string(),
            ],
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ArbitrageEngineToml {
    enabled: bool,
    min_margin_early_bps: u32,
    min_margin_mid_bps: u32,
    min_margin_late_bps: u32,
    min_confidence: u8,
    early_threshold_secs: u64,
    mid_threshold_secs: u64,
}

impl Default for ArbitrageEngineToml {
    fn default() -> Self {
        Self {
            enabled: false, // Disabled: taker fees exceed typical arb margins
            min_margin_early_bps: 250,
            min_margin_mid_bps: 150,
            min_margin_late_bps: 50,
            min_confidence: 70,
            early_threshold_secs: 300,
            mid_threshold_secs: 120,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct DirectionalEngineToml {
    enabled: bool,
    min_seconds_remaining: u64,
    max_combined_cost: f64,
    max_spread_bps: u32,
    min_favorable_depth: f64,
    strong_threshold_late_bps: u32,
    lean_threshold_late_bps: u32,
    strong_up_ratio: f64,
    lean_up_ratio: f64,
    neutral_ratio: f64,
    // Confidence / edge params (optional, use code defaults if not set)
    time_conf_floor: Option<f64>,
    dist_conf_floor: Option<f64>,
    dist_conf_per_atr: Option<f64>,
    max_edge_factor: Option<f64>,
}

impl Default for DirectionalEngineToml {
    fn default() -> Self {
        Self {
            enabled: false,
            min_seconds_remaining: 60,
            max_combined_cost: 0.995,
            max_spread_bps: 1000,
            min_favorable_depth: 100.0,
            strong_threshold_late_bps: 30,
            lean_threshold_late_bps: 10,
            strong_up_ratio: 0.78,
            lean_up_ratio: 0.60,
            neutral_ratio: 0.50,
            time_conf_floor: None,
            dist_conf_floor: None,
            dist_conf_per_atr: None,
            max_edge_factor: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct MakerEngineToml {
    enabled: bool,
    min_seconds_remaining: u64,
    min_spread_bps: u32,
    max_spread_bps: u32,
    default_order_size: f64,
    max_imbalance_ratio: f64,
    spread_inside_bps: u32,
    stale_threshold_ms: u64,
    price_refresh_threshold_bps: u32,
    max_orders_per_market: u32,
}

impl Default for MakerEngineToml {
    fn default() -> Self {
        Self {
            enabled: false,
            min_seconds_remaining: 120,
            min_spread_bps: 50,
            max_spread_bps: 1000,
            default_order_size: 50.0,
            max_imbalance_ratio: 0.80,
            spread_inside_bps: 10,
            stale_threshold_ms: 5000,
            price_refresh_threshold_bps: 20,
            max_orders_per_market: 4,
        }
    }
}

/// Parse engine priority from string list.
fn parse_priority(priority: &[String]) -> Vec<EngineType> {
    priority
        .iter()
        .filter_map(|s| match s.to_lowercase().as_str() {
            "arbitrage" | "arb" => Some(EngineType::Arbitrage),
            "directional" | "dir" => Some(EngineType::Directional),
            "maker" | "maker_rebates" | "rebates" => Some(EngineType::MakerRebates),
            _ => None,
        })
        .collect()
}

/// Convert f64 percentage to Decimal ratio (e.g., 2.5 -> 0.025).
fn pct_to_decimal(pct: f64) -> Decimal {
    Decimal::try_from(pct / 100.0).unwrap_or(Decimal::ZERO)
}

/// Convert f64 to Decimal.
fn f64_to_decimal(val: f64) -> Decimal {
    Decimal::try_from(val).unwrap_or(Decimal::ZERO)
}

impl BotConfig {
    /// Convert from parsed TOML config with validation.
    fn try_from_toml(toml: TomlConfig) -> Result<Self> {
        // Validate required general fields
        let mode = toml.general.mode
            .ok_or_else(|| anyhow::anyhow!("Missing required config: [general] mode"))?;
        let assets = toml.general.assets
            .ok_or_else(|| anyhow::anyhow!("Missing required config: [general] assets"))?;
        let window_duration_str = toml.general.window_duration
            .ok_or_else(|| anyhow::anyhow!("Missing required config: [general] window_duration"))?;
        let window_duration: WindowDuration = window_duration_str.parse()
            .map_err(|_| anyhow::anyhow!("Invalid window_duration '{}': must be '15min' or '1h'", window_duration_str))?;

        // Validate required trading fields
        // Support both new name (max_market_exposure) and legacy name (max_position_per_market)
        let max_market_exposure = toml.trading.max_market_exposure
            .or(toml.trading.max_position_per_market)
            .ok_or_else(|| anyhow::anyhow!("Missing required config: [trading] max_market_exposure"))?;
        let max_total_exposure = toml.trading.max_total_exposure
            .ok_or_else(|| anyhow::anyhow!("Missing required config: [trading] max_total_exposure"))?;
        let base_order_size = toml.trading.base_order_size
            .ok_or_else(|| anyhow::anyhow!("Missing required config: [trading] base_order_size"))?;
        let available_balance = toml.trading.available_balance
            .ok_or_else(|| anyhow::anyhow!("Missing required config: [trading] available_balance"))?;

        // Validate required risk fields
        let max_daily_loss = toml.risk.max_daily_loss
            .ok_or_else(|| anyhow::anyhow!("Missing required config: [risk] max_daily_loss"))?;
        let emergency_close_threshold = toml.risk.emergency_close_threshold
            .ok_or_else(|| anyhow::anyhow!("Missing required config: [risk] emergency_close_threshold"))?;

        // Validate required execution fields
        let execution_mode = toml.execution.execution_mode
            .ok_or_else(|| anyhow::anyhow!("Missing required config: [execution] execution_mode"))?;

        // Handle backwards compatibility: if old log_level is set, use it for console
        let console_log_level = toml.general.log_level
            .unwrap_or(toml.general.console_log_level);

        Ok(Self {
            mode: TradingMode::from_str(&mode).unwrap_or(TradingMode::Shadow),
            assets,
            window_duration,
            console_log_level,
            file_log_level: toml.general.file_log_level,
            clickhouse: ClickHouseConfig {
                url: toml.clickhouse.url,
                database: toml.clickhouse.database,
                user: None,
                password: None,
                max_rows: toml.clickhouse.max_rows,
                max_bytes: toml.clickhouse.max_bytes,
                commit_period: Duration::from_secs(toml.clickhouse.period_secs),
            },
            trading: TradingConfig {
                min_margin_early: pct_to_decimal(toml.trading.min_margin_early_pct),
                min_margin_mid: pct_to_decimal(toml.trading.min_margin_mid_pct),
                min_margin_late: pct_to_decimal(toml.trading.min_margin_late_pct),
                min_time_remaining_secs: toml.trading.min_time_remaining_secs,
                max_market_exposure: f64_to_decimal(max_market_exposure),
                max_total_exposure: f64_to_decimal(max_total_exposure),
                base_order_size: f64_to_decimal(base_order_size),
                min_order_size: f64_to_decimal(toml.trading.min_order_size),
                available_balance: f64_to_decimal(available_balance),
                early_threshold_secs: toml.trading.early_threshold_secs,
                mid_threshold_secs: toml.trading.mid_threshold_secs,
            },
            risk: RiskConfig {
                risk_mode: toml.risk.risk_mode,
                max_consecutive_failures: toml.risk.max_consecutive_failures,
                circuit_breaker_cooldown_secs: toml.risk.circuit_breaker_cooldown_secs,
                max_daily_loss: f64_to_decimal(max_daily_loss),
                max_imbalance_ratio: f64_to_decimal(toml.risk.max_imbalance_ratio),
                toxic_flow_threshold: toml.risk.toxic_flow_threshold,
                emergency_close_threshold: f64_to_decimal(emergency_close_threshold),
            },
            shadow: ShadowConfig {
                enabled: toml.shadow.enabled,
                price_offset_bps: toml.shadow.price_offset_bps,
                max_wait_ms: toml.shadow.max_wait_ms,
            },
            execution: ExecutionConfig {
                execution_mode: ExecutionMode::from_str(&execution_mode)
                    .unwrap_or(ExecutionMode::Limit),
                chase_enabled: toml.execution.chase_enabled,
                chase_step_size: f64_to_decimal(toml.execution.chase_step_size),
                chase_check_interval_ms: toml.execution.chase_check_interval_ms,
                max_chase_time_ms: toml.execution.max_chase_time_ms,
                paper_fill_latency_ms: toml.execution.paper_fill_latency_ms,
                order_timeout_ms: toml.execution.order_timeout_ms,
                test_trade_on_startup: toml.execution.test_trade_on_startup,
            },
            observability: ObservabilityConfig {
                capture_decisions: toml.observability.capture_decisions,
                capture_counterfactuals: toml.observability.capture_counterfactuals,
                detect_anomalies: toml.observability.detect_anomalies,
                channel_buffer_size: toml.observability.channel_buffer_size,
                batch_size: toml.observability.batch_size,
                flush_interval_secs: toml.observability.flush_interval_secs,
                alerts_enabled: toml.observability.alerts_enabled,
                alert_webhook_url: None, // Set via env var
            },
            dashboard: DashboardConfig {
                enabled: toml.dashboard.enabled,
                channel_capacity: toml.dashboard.channel_capacity,
                log_drops: toml.dashboard.log_drops,
                drop_log_threshold: toml.dashboard.drop_log_threshold,
                batch_size: toml.dashboard.batch_size,
                flush_interval_secs: toml.dashboard.flush_interval_secs,
                max_buffer_size: toml.dashboard.max_buffer_size,
                websocket_port: toml.dashboard.websocket_port,
                api_port: toml.dashboard.api_port,
                broadcast_interval_ms: toml.dashboard.broadcast_interval_ms,
                pnl_snapshot_interval_secs: toml.dashboard.pnl_snapshot_interval_secs,
                // Environment variable overrides config file
                static_dir: std::env::var("POLY_DASHBOARD_STATIC_DIR")
                    .ok()
                    .or_else(|| toml.dashboard.static_dir.clone()),
            },
            wallet: WalletConfig::default(), // Always from env vars
            backtest: BacktestConfig {
                start_date: toml.backtest.start_date,
                end_date: toml.backtest.end_date,
                duration: toml.backtest.duration,
                speed: toml.backtest.speed,
                sweep_enabled: toml.backtest.sweep_enabled,
                data_dir: toml.backtest.data_dir,
                sweep_parallel_workers: toml.backtest.sweep_parallel_workers,
                trading: match toml.backtest.trading {
                    Some(t) => Some(TradingConfig {
                        min_margin_early: pct_to_decimal(t.min_margin_early_pct),
                        min_margin_mid: pct_to_decimal(t.min_margin_mid_pct),
                        min_margin_late: pct_to_decimal(t.min_margin_late_pct),
                        min_time_remaining_secs: t.min_time_remaining_secs,
                        max_market_exposure: f64_to_decimal(t.max_market_exposure
                            .or(t.max_position_per_market)
                            .ok_or_else(|| anyhow::anyhow!("Missing required config: [backtest.trading] max_market_exposure"))?),
                        max_total_exposure: f64_to_decimal(t.max_total_exposure
                            .ok_or_else(|| anyhow::anyhow!("Missing required config: [backtest.trading] max_total_exposure"))?),
                        base_order_size: f64_to_decimal(t.base_order_size
                            .ok_or_else(|| anyhow::anyhow!("Missing required config: [backtest.trading] base_order_size"))?),
                        min_order_size: f64_to_decimal(t.min_order_size),
                        available_balance: f64_to_decimal(t.available_balance
                            .ok_or_else(|| anyhow::anyhow!("Missing required config: [backtest.trading] available_balance"))?),
                        early_threshold_secs: t.early_threshold_secs,
                        mid_threshold_secs: t.mid_threshold_secs,
                    }),
                    None => None,
                },
                decision_log_path: toml.backtest.decision_log_path,
                sweep_params: SweepConfig {
                    base_order_sizes: toml.backtest.sweep.base_order_sizes,
                    strong_ratios: toml.backtest.sweep.strong_ratios,
                    lean_ratios: toml.backtest.sweep.lean_ratios,
                    time_conf_floors: toml.backtest.sweep.time_conf_floors,
                    dist_conf_floors: toml.backtest.sweep.dist_conf_floors,
                    dist_conf_per_atrs: toml.backtest.sweep.dist_conf_per_atrs,
                    edge_factors: toml.backtest.sweep.edge_factors,
                },
                book_interval_ms: toml.backtest.book_interval_ms,
            },
            live: LiveConfig {
                auto_claim_interval: toml.live.auto_claim_interval,
            },
            engines: EnginesConfig {
                arbitrage: ArbitrageEngineConfig {
                    enabled: toml.engines.arbitrage.enabled,
                    min_margin_early_bps: toml.engines.arbitrage.min_margin_early_bps,
                    min_margin_mid_bps: toml.engines.arbitrage.min_margin_mid_bps,
                    min_margin_late_bps: toml.engines.arbitrage.min_margin_late_bps,
                    min_confidence: toml.engines.arbitrage.min_confidence,
                    early_threshold_secs: toml.engines.arbitrage.early_threshold_secs,
                    mid_threshold_secs: toml.engines.arbitrage.mid_threshold_secs,
                },
                directional: DirectionalEngineConfig {
                    enabled: toml.engines.directional.enabled,
                    min_seconds_remaining: toml.engines.directional.min_seconds_remaining,
                    max_combined_cost: f64_to_decimal(toml.engines.directional.max_combined_cost),
                    max_spread_bps: toml.engines.directional.max_spread_bps,
                    min_favorable_depth: f64_to_decimal(
                        toml.engines.directional.min_favorable_depth,
                    ),
                    strong_threshold_late_bps: toml.engines.directional.strong_threshold_late_bps,
                    lean_threshold_late_bps: toml.engines.directional.lean_threshold_late_bps,
                    strong_up_ratio: f64_to_decimal(toml.engines.directional.strong_up_ratio),
                    lean_up_ratio: f64_to_decimal(toml.engines.directional.lean_up_ratio),
                    neutral_ratio: f64_to_decimal(toml.engines.directional.neutral_ratio),
                    time_conf_floor: toml.engines.directional.time_conf_floor.map(f64_to_decimal),
                    dist_conf_floor: toml.engines.directional.dist_conf_floor.map(f64_to_decimal),
                    dist_conf_per_atr: toml.engines.directional.dist_conf_per_atr.map(f64_to_decimal),
                    max_edge_factor: toml.engines.directional.max_edge_factor.map(f64_to_decimal),
                },
                maker: MakerEngineConfig {
                    enabled: toml.engines.maker.enabled,
                    min_seconds_remaining: toml.engines.maker.min_seconds_remaining,
                    min_spread_bps: toml.engines.maker.min_spread_bps,
                    max_spread_bps: toml.engines.maker.max_spread_bps,
                    default_order_size: f64_to_decimal(toml.engines.maker.default_order_size),
                    max_imbalance_ratio: f64_to_decimal(toml.engines.maker.max_imbalance_ratio),
                    spread_inside_bps: toml.engines.maker.spread_inside_bps,
                    stale_threshold_ms: toml.engines.maker.stale_threshold_ms,
                    price_refresh_threshold_bps: toml.engines.maker.price_refresh_threshold_bps,
                    max_orders_per_market: toml.engines.maker.max_orders_per_market,
                },
                priority: parse_priority(&toml.engines.priority),
            },
            phases: PhaseConfig {
                early_budget: f64_to_decimal(toml.phases.early_budget),
                build_budget: f64_to_decimal(toml.phases.build_budget),
                core_budget: f64_to_decimal(toml.phases.core_budget),
                final_budget: f64_to_decimal(toml.phases.final_budget),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_default_config() {
        let config = BotConfig::default();
        assert_eq!(config.mode, TradingMode::Shadow);
        assert_eq!(config.assets.len(), 3);
        assert!(config.assets.contains(&"BTC".to_string()));
    }

    #[test]
    fn test_trading_mode_from_str() {
        assert_eq!(TradingMode::from_str("live"), Some(TradingMode::Live));
        assert_eq!(TradingMode::from_str("LIVE"), Some(TradingMode::Live));
        assert_eq!(TradingMode::from_str("paper"), Some(TradingMode::Paper));
        assert_eq!(TradingMode::from_str("shadow"), Some(TradingMode::Shadow));
        assert_eq!(
            TradingMode::from_str("backtest"),
            Some(TradingMode::Backtest)
        );
        assert_eq!(TradingMode::from_str("invalid"), None);
    }

    #[test]
    fn test_trading_mode_display() {
        assert_eq!(TradingMode::Live.to_string(), "live");
        assert_eq!(TradingMode::Paper.to_string(), "paper");
        assert_eq!(TradingMode::Shadow.to_string(), "shadow");
        assert_eq!(TradingMode::Backtest.to_string(), "backtest");
    }

    #[test]
    fn test_parse_toml() {
        let toml = r#"
            [general]
            mode = "paper"
            assets = ["BTC", "ETH"]
            window_duration = "1h"
            log_level = "debug"

            [clickhouse]
            url = "http://db:8123"

            [trading]
            min_margin_early_pct = 3.0
            max_position_per_market = 2000.0
            max_total_exposure = 5000.0
            base_order_size = 10.0
            available_balance = 10000.0

            [risk]
            max_consecutive_failures = 5
            max_daily_loss = 500.0
            emergency_close_threshold = 0.15

            [execution]
            execution_mode = "limit"

            [observability]
            capture_decisions = false
        "#;

        let config = BotConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.mode, TradingMode::Paper);
        assert_eq!(config.assets.len(), 2);
        // Old log_level field maps to console_log_level for backwards compatibility
        assert_eq!(config.console_log_level, "debug");
        assert_eq!(config.clickhouse.url, "http://db:8123");
        assert_eq!(config.trading.min_margin_early, dec!(0.03));
        assert_eq!(config.trading.max_market_exposure, dec!(2000));
        assert_eq!(config.risk.max_consecutive_failures, 5);
        assert!(!config.observability.capture_decisions);
    }

    #[test]
    fn test_cli_overrides() {
        let mut config = BotConfig::default();

        config.apply_cli_overrides(
            Some("live".to_string()),
            Some(vec!["XRP".to_string()]),
            Some("http://override:8123".to_string()),
        );

        assert_eq!(config.mode, TradingMode::Live);
        assert_eq!(config.assets, vec!["XRP".to_string()]);
        assert_eq!(config.clickhouse.url, "http://override:8123");
    }

    #[test]
    fn test_validate_shadow_mode() {
        let config = BotConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_live_mode_no_key() {
        let mut config = BotConfig::default();
        config.mode = TradingMode::Live;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_backtest_no_dates() {
        let mut config = BotConfig::default();
        config.mode = TradingMode::Backtest;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_invalid_margin() {
        let mut config = BotConfig::default();
        config.trading.min_margin_early = Decimal::ZERO;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_position_exceeds_exposure() {
        let mut config = BotConfig::default();
        config.trading.max_market_exposure = dec!(10000);
        config.trading.max_total_exposure = dec!(5000);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_assets() {
        let mut config = BotConfig::default();
        config.assets.clear();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_observability_defaults() {
        let config = ObservabilityConfig::default();
        assert!(config.capture_decisions);
        assert!(config.capture_counterfactuals);
        assert!(config.detect_anomalies);
        assert!(!config.alerts_enabled);
        assert!(config.alert_webhook_url.is_none());
    }

    #[test]
    fn test_pct_to_decimal() {
        assert_eq!(pct_to_decimal(2.5), dec!(0.025));
        assert_eq!(pct_to_decimal(100.0), dec!(1.0));
        assert_eq!(pct_to_decimal(0.0), dec!(0));
    }

    // =========================================================================
    // EnginesConfig tests
    // =========================================================================

    #[test]
    fn test_engines_config_default() {
        let config = EnginesConfig::default();

        // All engines disabled by default (arb fees exceed margins)
        assert!(!config.arbitrage.enabled);
        assert!(!config.directional.enabled);
        assert!(!config.maker.enabled);

        // Default priority order
        assert_eq!(config.priority.len(), 3);
        assert_eq!(config.priority[0], EngineType::Arbitrage);
        assert_eq!(config.priority[1], EngineType::Directional);
        assert_eq!(config.priority[2], EngineType::MakerRebates);
    }

    #[test]
    fn test_engines_config_arbitrage_only() {
        let config = EnginesConfig::arbitrage_only();

        assert!(config.arbitrage.enabled);
        assert!(!config.directional.enabled);
        assert!(!config.maker.enabled);
        assert_eq!(config.priority.len(), 1);
        assert_eq!(config.priority[0], EngineType::Arbitrage);
    }

    #[test]
    fn test_engines_config_any_enabled() {
        // All disabled
        let mut config = EnginesConfig::default();
        config.arbitrage.enabled = false;
        config.directional.enabled = false;
        config.maker.enabled = false;
        assert!(!config.any_enabled());

        // One enabled
        config.directional.enabled = true;
        assert!(config.any_enabled());
    }

    #[test]
    fn test_engines_config_enabled_engines() {
        let mut config = EnginesConfig::default();

        // Default: no engines enabled
        let enabled = config.enabled_engines();
        assert_eq!(enabled.len(), 0);

        // Enable directional
        config.directional.enabled = true;
        let enabled = config.enabled_engines();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0], EngineType::Directional);

        // Enable arbitrage too
        config.arbitrage.enabled = true;
        let enabled = config.enabled_engines();
        assert_eq!(enabled.len(), 2);
        assert_eq!(enabled[0], EngineType::Arbitrage);
        assert_eq!(enabled[1], EngineType::Directional);

        // Enable maker too
        config.maker.enabled = true;
        let enabled = config.enabled_engines();
        assert_eq!(enabled.len(), 3);
    }

    #[test]
    fn test_engines_config_get_priority() {
        let config = EnginesConfig::default();

        assert_eq!(config.get_priority(EngineType::Arbitrage), Some(0));
        assert_eq!(config.get_priority(EngineType::Directional), Some(1));
        assert_eq!(config.get_priority(EngineType::MakerRebates), Some(2));
    }

    #[test]
    fn test_arbitrage_engine_config_default() {
        let config = ArbitrageEngineConfig::default();

        assert!(!config.enabled); // Disabled: taker fees exceed typical arb margins
        assert_eq!(config.min_margin_early_bps, 250);
        assert_eq!(config.min_margin_mid_bps, 150);
        assert_eq!(config.min_margin_late_bps, 50);
        assert_eq!(config.min_confidence, 70);
        assert_eq!(config.early_threshold_secs, 300);
        assert_eq!(config.mid_threshold_secs, 120);
    }

    #[test]
    fn test_directional_engine_config_default() {
        let config = DirectionalEngineConfig::default();

        assert!(!config.enabled);
        assert_eq!(config.min_seconds_remaining, 60);
        assert_eq!(config.max_combined_cost, dec!(0.995));
        assert_eq!(config.max_spread_bps, 1000);
        assert_eq!(config.min_favorable_depth, dec!(100));
        assert_eq!(config.strong_up_ratio, dec!(0.78));
        assert_eq!(config.lean_up_ratio, dec!(0.60));
        assert_eq!(config.neutral_ratio, dec!(0.50));
    }

    #[test]
    fn test_directional_engine_config_allocations() {
        let config = DirectionalEngineConfig::default();

        // Strong up: 78% UP, 22% DOWN
        let (up, down) = config.strong_up_allocation();
        assert_eq!(up, dec!(0.78));
        assert_eq!(down, dec!(0.22));

        // Strong down: 22% UP, 78% DOWN (inverted)
        let (up, down) = config.strong_down_allocation();
        assert_eq!(up, dec!(0.22));
        assert_eq!(down, dec!(0.78));

        // Lean up: 60% UP, 40% DOWN
        let (up, down) = config.lean_up_allocation();
        assert_eq!(up, dec!(0.60));
        assert_eq!(down, dec!(0.40));

        // Lean down: 40% UP, 60% DOWN (inverted)
        let (up, down) = config.lean_down_allocation();
        assert_eq!(up, dec!(0.40));
        assert_eq!(down, dec!(0.60));
    }

    #[test]
    fn test_maker_engine_config_default() {
        let config = MakerEngineConfig::default();

        assert!(!config.enabled);
        assert_eq!(config.min_seconds_remaining, 120);
        assert_eq!(config.min_spread_bps, 50);
        assert_eq!(config.max_spread_bps, 1000);
        assert_eq!(config.default_order_size, dec!(50));
        assert_eq!(config.max_imbalance_ratio, dec!(0.80));
        assert_eq!(config.spread_inside_bps, 10);
        assert_eq!(config.stale_threshold_ms, 5000);
        assert_eq!(config.price_refresh_threshold_bps, 20);
        assert_eq!(config.max_orders_per_market, 4);
    }

    #[test]
    fn test_parse_priority() {
        // Standard names
        let priority = parse_priority(&[
            "arbitrage".to_string(),
            "directional".to_string(),
            "maker".to_string(),
        ]);
        assert_eq!(priority.len(), 3);
        assert_eq!(priority[0], EngineType::Arbitrage);
        assert_eq!(priority[1], EngineType::Directional);
        assert_eq!(priority[2], EngineType::MakerRebates);

        // Short names
        let priority = parse_priority(&[
            "arb".to_string(),
            "dir".to_string(),
            "rebates".to_string(),
        ]);
        assert_eq!(priority.len(), 3);
        assert_eq!(priority[0], EngineType::Arbitrage);
        assert_eq!(priority[1], EngineType::Directional);
        assert_eq!(priority[2], EngineType::MakerRebates);

        // Mixed case
        let priority = parse_priority(&["ARB".to_string(), "DIRECTIONAL".to_string()]);
        assert_eq!(priority.len(), 2);

        // Invalid entries filtered
        let priority = parse_priority(&[
            "arbitrage".to_string(),
            "invalid".to_string(),
            "directional".to_string(),
        ]);
        assert_eq!(priority.len(), 2);
    }

    #[test]
    fn test_engines_toml_parsing() {
        let toml = r#"
            [general]
            mode = "paper"
            assets = ["BTC", "ETH"]
            window_duration = "1h"

            [trading]
            max_position_per_market = 1000.0
            max_total_exposure = 5000.0
            base_order_size = 10.0
            available_balance = 10000.0

            [risk]
            max_daily_loss = 500.0
            emergency_close_threshold = 0.15

            [execution]
            execution_mode = "limit"

            [engines]
            priority = ["directional", "arbitrage", "maker"]

            [engines.arbitrage]
            enabled = true
            min_margin_early_bps = 300
            min_margin_mid_bps = 200
            min_margin_late_bps = 100

            [engines.directional]
            enabled = true
            min_seconds_remaining = 120
            max_spread_bps = 500
            strong_up_ratio = 0.80
            lean_up_ratio = 0.65

            [engines.maker]
            enabled = true
            min_spread_bps = 100
            default_order_size = 100.0
        "#;

        let config = BotConfig::from_toml_str(toml).unwrap();

        // Priority order changed
        assert_eq!(config.engines.priority[0], EngineType::Directional);
        assert_eq!(config.engines.priority[1], EngineType::Arbitrage);
        assert_eq!(config.engines.priority[2], EngineType::MakerRebates);

        // Arbitrage config
        assert!(config.engines.arbitrage.enabled);
        assert_eq!(config.engines.arbitrage.min_margin_early_bps, 300);
        assert_eq!(config.engines.arbitrage.min_margin_mid_bps, 200);
        assert_eq!(config.engines.arbitrage.min_margin_late_bps, 100);

        // Directional config
        assert!(config.engines.directional.enabled);
        assert_eq!(config.engines.directional.min_seconds_remaining, 120);
        assert_eq!(config.engines.directional.max_spread_bps, 500);
        assert_eq!(config.engines.directional.strong_up_ratio, dec!(0.80));
        assert_eq!(config.engines.directional.lean_up_ratio, dec!(0.65));

        // Maker config
        assert!(config.engines.maker.enabled);
        assert_eq!(config.engines.maker.min_spread_bps, 100);
        assert_eq!(config.engines.maker.default_order_size, dec!(100));
    }

    #[test]
    fn test_engines_default_toml_parsing() {
        // Minimal TOML with required fields should use defaults for optional fields
        let toml = r#"
            [general]
            mode = "shadow"
            assets = ["BTC"]
            window_duration = "1h"

            [trading]
            max_position_per_market = 1000.0
            max_total_exposure = 5000.0
            base_order_size = 10.0
            available_balance = 10000.0

            [risk]
            max_daily_loss = 500.0
            emergency_close_threshold = 0.15

            [execution]
            execution_mode = "limit"
        "#;

        let config = BotConfig::from_toml_str(toml).unwrap();

        // Default: no engines enabled (arb fees exceed margins)
        assert!(!config.engines.arbitrage.enabled);
        assert!(!config.engines.directional.enabled);
        assert!(!config.engines.maker.enabled);

        // Default priority (still defined even if engines disabled)
        assert_eq!(config.engines.priority[0], EngineType::Arbitrage);
    }
}
