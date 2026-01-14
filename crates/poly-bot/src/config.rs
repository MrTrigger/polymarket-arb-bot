//! Configuration for poly-bot.
//!
//! Supports loading from TOML file with environment variable overrides.
//! All trading parameters from the spec are defined here.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use poly_common::ClickHouseConfig;
use rust_decimal::Decimal;
use serde::Deserialize;

/// Top-level configuration for poly-bot.
#[derive(Debug, Clone)]
pub struct BotConfig {
    /// Trading mode: live, paper, shadow, backtest.
    pub mode: TradingMode,

    /// Assets to trade.
    pub assets: Vec<String>,

    /// Logging level.
    pub log_level: String,

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

    /// Wallet configuration (for live trading).
    pub wallet: WalletConfig,

    /// Backtest-specific configuration.
    pub backtest: BacktestConfig,
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

    /// Maximum position size per market (USDC).
    pub max_position_per_market: Decimal,

    /// Maximum total exposure across all markets (USDC).
    pub max_total_exposure: Decimal,

    /// Base order size (USDC). Used by limit-based sizing.
    pub base_order_size: Decimal,

    /// Window time boundary for "early" phase (seconds).
    pub early_threshold_secs: u64,

    /// Window time boundary for "mid" phase (seconds).
    pub mid_threshold_secs: u64,

    /// Confidence-based sizing configuration.
    pub sizing: SizingConfig,
}

/// Configuration for confidence-based order sizing.
///
/// Scales order sizes with available balance using the formula:
/// - `market_budget = available_balance * max_market_allocation`
/// - `base_order_size = market_budget / expected_trades_per_market`
/// - Actual size = base_order_size × confidence_multiplier (0.5x to 3.0x)
#[derive(Debug, Clone)]
pub struct SizingConfig {
    /// Sizing mode: "limits" | "confidence" | "hybrid".
    /// - limits: Fixed base sizing with max position/exposure limits
    /// - confidence: Dynamic sizing based on market confidence factors
    /// - hybrid: Confidence-based sizing with limit caps (recommended)
    pub mode: String,

    /// Total available balance for trading (USDC).
    pub available_balance: Decimal,

    /// Maximum % of balance to risk per market (default: 0.20 = 20%).
    pub max_market_allocation: Decimal,

    /// Expected number of trades per market window (default: 200).
    /// Used to calculate base order size: budget / expected_trades.
    pub expected_trades_per_market: u32,

    /// Minimum order size in USDC (Polymarket minimum, default: $1.00).
    pub min_order_size: Decimal,

    /// Maximum confidence multiplier (caps aggressive sizing, default: 3.0).
    pub max_confidence_multiplier: Decimal,

    /// Minimum hedge ratio - always keep some on both sides (default: 0.20 = 20%).
    pub min_hedge_ratio: Decimal,
}

impl SizingConfig {
    /// Create a new SizingConfig with the given available balance.
    /// Uses sensible defaults for all other parameters.
    pub fn new(available_balance: Decimal) -> Self {
        Self {
            mode: "limits".to_string(),
            available_balance,
            max_market_allocation: Decimal::new(20, 2),    // 0.20 = 20%
            expected_trades_per_market: 200,
            min_order_size: Decimal::ONE,                  // $1.00
            max_confidence_multiplier: Decimal::new(3, 0), // 3.0x
            min_hedge_ratio: Decimal::new(20, 2),          // 0.20 = 20%
        }
    }

    /// Create a new SizingConfig with a specific mode.
    pub fn with_mode(available_balance: Decimal, mode: &str) -> Self {
        Self {
            mode: mode.to_string(),
            available_balance,
            max_market_allocation: Decimal::new(20, 2),    // 0.20 = 20%
            expected_trades_per_market: 200,
            min_order_size: Decimal::ONE,                  // $1.00
            max_confidence_multiplier: Decimal::new(3, 0), // 3.0x
            min_hedge_ratio: Decimal::new(20, 2),          // 0.20 = 20%
        }
    }

    /// Calculate the budget for a single market.
    ///
    /// Returns: `available_balance * max_market_allocation`
    ///
    /// # Examples
    /// - $1,000 balance × 20% = $200 market budget
    /// - $5,000 balance × 20% = $1,000 market budget
    /// - $25,000 balance × 20% = $5,000 market budget
    #[inline]
    pub fn market_budget(&self) -> Decimal {
        self.available_balance * self.max_market_allocation
    }

    /// Calculate base order size (minimum size before confidence scaling).
    ///
    /// Returns: `max(market_budget / expected_trades_per_market, min_order_size)`
    ///
    /// # Examples
    /// - $1,000 balance: $200 budget / 200 trades = $1.00 (hits min)
    /// - $5,000 balance: $1,000 budget / 200 trades = $5.00
    /// - $25,000 balance: $5,000 budget / 200 trades = $25.00
    #[inline]
    pub fn base_order_size(&self) -> Decimal {
        let budget = self.market_budget();
        let size = budget / Decimal::from(self.expected_trades_per_market);
        size.max(self.min_order_size)
    }

    /// Calculate the daily loss limit (10% of capital).
    #[inline]
    pub fn daily_loss_limit(&self) -> Decimal {
        self.available_balance * Decimal::new(10, 2) // 0.10 = 10%
    }
}

impl Default for SizingConfig {
    fn default() -> Self {
        Self::new(Decimal::new(5000, 0)) // Default $5,000 balance
    }
}

impl Default for TradingConfig {
    fn default() -> Self {
        Self {
            // Time-based thresholds from spec
            min_margin_early: Decimal::new(25, 3),    // 2.5%
            min_margin_mid: Decimal::new(15, 3),      // 1.5%
            min_margin_late: Decimal::new(5, 3),      // 0.5%
            min_time_remaining_secs: 30,
            max_position_per_market: Decimal::new(1000, 0), // $1000
            max_total_exposure: Decimal::new(5000, 0),      // $5000
            base_order_size: Decimal::new(50, 0),           // $50
            early_threshold_secs: 300, // 5 minutes
            mid_threshold_secs: 120,   // 2 minutes
            sizing: SizingConfig::default(),
        }
    }
}

/// Risk management parameters.
#[derive(Debug, Clone)]
pub struct RiskConfig {
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

/// Execution configuration.
#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    /// Enable price chasing.
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
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            chase_enabled: true,
            chase_step_size: Decimal::new(1, 3), // 0.001
            chase_check_interval_ms: 100,
            max_chase_time_ms: 5000,
            paper_fill_latency_ms: 50,
            order_timeout_ms: 10000,
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

    /// Playback speed multiplier (1.0 = real-time, 0 = max speed).
    pub speed: f64,

    /// Enable parameter sweep mode.
    pub sweep_enabled: bool,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            start_date: None,
            end_date: None,
            speed: 0.0, // Max speed by default
            sweep_enabled: false,
        }
    }
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            mode: TradingMode::Shadow,
            assets: vec!["BTC".to_string(), "ETH".to_string(), "SOL".to_string()],
            log_level: "info".to_string(),
            clickhouse: ClickHouseConfig::default(),
            trading: TradingConfig::default(),
            risk: RiskConfig::default(),
            shadow: ShadowConfig::default(),
            execution: ExecutionConfig::default(),
            observability: ObservabilityConfig::default(),
            wallet: WalletConfig::default(),
            backtest: BacktestConfig::default(),
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
        Ok(Self::from(file))
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
        if let Ok(url) = std::env::var("CLICKHOUSE_URL") {
            self.clickhouse.url = url;
        }
        if let Ok(user) = std::env::var("CLICKHOUSE_USER") {
            self.clickhouse.user = Some(user);
        }
        if let Ok(pass) = std::env::var("CLICKHOUSE_PASSWORD") {
            self.clickhouse.password = Some(pass);
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
        if self.mode == TradingMode::Live {
            if self.wallet.private_key.is_none() {
                bail!("Live mode requires POLY_PRIVATE_KEY environment variable");
            }
            if self.wallet.api_key.is_none() {
                bail!("Live mode requires POLY_API_KEY environment variable");
            }
        }

        if self.mode == TradingMode::Backtest
            && (self.backtest.start_date.is_none() || self.backtest.end_date.is_none())
        {
            bail!("Backtest mode requires start_date and end_date");
        }

        // Trading config validation
        if self.trading.min_margin_early <= Decimal::ZERO {
            bail!("min_margin_early must be positive");
        }
        if self.trading.max_position_per_market <= Decimal::ZERO {
            bail!("max_position_per_market must be positive");
        }
        if self.trading.max_total_exposure <= Decimal::ZERO {
            bail!("max_total_exposure must be positive");
        }
        if self.trading.max_position_per_market > self.trading.max_total_exposure {
            bail!("max_position_per_market cannot exceed max_total_exposure");
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
    backtest: BacktestToml,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct GeneralToml {
    mode: String,
    assets: Vec<String>,
    log_level: String,
}

impl Default for GeneralToml {
    fn default() -> Self {
        Self {
            mode: "shadow".to_string(),
            assets: vec!["BTC".to_string(), "ETH".to_string(), "SOL".to_string()],
            log_level: "info".to_string(),
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
            database: "default".to_string(),
            max_rows: 10000,
            max_bytes: 10 * 1024 * 1024,
            period_secs: 5,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct TradingToml {
    min_margin_early_pct: f64,
    min_margin_mid_pct: f64,
    min_margin_late_pct: f64,
    min_time_remaining_secs: u64,
    max_position_per_market: f64,
    max_total_exposure: f64,
    base_order_size: f64,
    early_threshold_secs: u64,
    mid_threshold_secs: u64,
    #[serde(default)]
    sizing: SizingToml,
}

impl Default for TradingToml {
    fn default() -> Self {
        Self {
            min_margin_early_pct: 2.5,
            min_margin_mid_pct: 1.5,
            min_margin_late_pct: 0.5,
            min_time_remaining_secs: 30,
            max_position_per_market: 1000.0,
            max_total_exposure: 5000.0,
            base_order_size: 50.0,
            early_threshold_secs: 300,
            mid_threshold_secs: 120,
            sizing: SizingToml::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct SizingToml {
    /// Sizing mode: "limits" | "confidence" | "hybrid"
    mode: String,
    available_balance: f64,
    max_market_allocation: f64,
    expected_trades_per_market: u32,
    min_order_size: f64,
    max_confidence_multiplier: f64,
    min_hedge_ratio: f64,
}

impl Default for SizingToml {
    fn default() -> Self {
        Self {
            mode: "limits".to_string(),
            available_balance: 5000.0,
            max_market_allocation: 0.20,
            expected_trades_per_market: 200,
            min_order_size: 1.0,
            max_confidence_multiplier: 3.0,
            min_hedge_ratio: 0.20,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RiskToml {
    max_consecutive_failures: u32,
    circuit_breaker_cooldown_secs: u64,
    max_daily_loss: f64,
    max_imbalance_ratio: f64,
    toxic_flow_threshold: u8,
    emergency_close_threshold: f64,
}

impl Default for RiskToml {
    fn default() -> Self {
        Self {
            max_consecutive_failures: 3,
            circuit_breaker_cooldown_secs: 300,
            max_daily_loss: 500.0,
            max_imbalance_ratio: 0.7,
            toxic_flow_threshold: 80,
            emergency_close_threshold: 200.0,
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
    chase_enabled: bool,
    chase_step_size: f64,
    chase_check_interval_ms: u64,
    max_chase_time_ms: u64,
    paper_fill_latency_ms: u64,
    order_timeout_ms: u64,
}

impl Default for ExecutionToml {
    fn default() -> Self {
        Self {
            chase_enabled: true,
            chase_step_size: 0.001,
            chase_check_interval_ms: 100,
            max_chase_time_ms: 5000,
            paper_fill_latency_ms: 50,
            order_timeout_ms: 10000,
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
struct BacktestToml {
    start_date: Option<String>,
    end_date: Option<String>,
    speed: f64,
    sweep_enabled: bool,
}

impl Default for BacktestToml {
    fn default() -> Self {
        Self {
            start_date: None,
            end_date: None,
            speed: 0.0,
            sweep_enabled: false,
        }
    }
}

/// Convert f64 percentage to Decimal ratio (e.g., 2.5 -> 0.025).
fn pct_to_decimal(pct: f64) -> Decimal {
    Decimal::try_from(pct / 100.0).unwrap_or(Decimal::ZERO)
}

/// Convert f64 to Decimal.
fn f64_to_decimal(val: f64) -> Decimal {
    Decimal::try_from(val).unwrap_or(Decimal::ZERO)
}

impl From<TomlConfig> for BotConfig {
    fn from(toml: TomlConfig) -> Self {
        Self {
            mode: TradingMode::from_str(&toml.general.mode).unwrap_or(TradingMode::Shadow),
            assets: toml.general.assets,
            log_level: toml.general.log_level,
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
                max_position_per_market: f64_to_decimal(toml.trading.max_position_per_market),
                max_total_exposure: f64_to_decimal(toml.trading.max_total_exposure),
                base_order_size: f64_to_decimal(toml.trading.base_order_size),
                early_threshold_secs: toml.trading.early_threshold_secs,
                mid_threshold_secs: toml.trading.mid_threshold_secs,
                sizing: SizingConfig {
                    mode: toml.trading.sizing.mode,
                    available_balance: f64_to_decimal(toml.trading.sizing.available_balance),
                    max_market_allocation: f64_to_decimal(toml.trading.sizing.max_market_allocation),
                    expected_trades_per_market: toml.trading.sizing.expected_trades_per_market,
                    min_order_size: f64_to_decimal(toml.trading.sizing.min_order_size),
                    max_confidence_multiplier: f64_to_decimal(
                        toml.trading.sizing.max_confidence_multiplier,
                    ),
                    min_hedge_ratio: f64_to_decimal(toml.trading.sizing.min_hedge_ratio),
                },
            },
            risk: RiskConfig {
                max_consecutive_failures: toml.risk.max_consecutive_failures,
                circuit_breaker_cooldown_secs: toml.risk.circuit_breaker_cooldown_secs,
                max_daily_loss: f64_to_decimal(toml.risk.max_daily_loss),
                max_imbalance_ratio: f64_to_decimal(toml.risk.max_imbalance_ratio),
                toxic_flow_threshold: toml.risk.toxic_flow_threshold,
                emergency_close_threshold: f64_to_decimal(toml.risk.emergency_close_threshold),
            },
            shadow: ShadowConfig {
                enabled: toml.shadow.enabled,
                price_offset_bps: toml.shadow.price_offset_bps,
                max_wait_ms: toml.shadow.max_wait_ms,
            },
            execution: ExecutionConfig {
                chase_enabled: toml.execution.chase_enabled,
                chase_step_size: f64_to_decimal(toml.execution.chase_step_size),
                chase_check_interval_ms: toml.execution.chase_check_interval_ms,
                max_chase_time_ms: toml.execution.max_chase_time_ms,
                paper_fill_latency_ms: toml.execution.paper_fill_latency_ms,
                order_timeout_ms: toml.execution.order_timeout_ms,
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
            wallet: WalletConfig::default(), // Always from env vars
            backtest: BacktestConfig {
                start_date: toml.backtest.start_date,
                end_date: toml.backtest.end_date,
                speed: toml.backtest.speed,
                sweep_enabled: toml.backtest.sweep_enabled,
            },
        }
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
            log_level = "debug"

            [clickhouse]
            url = "http://db:8123"

            [trading]
            min_margin_early_pct = 3.0
            max_position_per_market = 2000.0

            [risk]
            max_consecutive_failures = 5

            [observability]
            capture_decisions = false
        "#;

        let config = BotConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.mode, TradingMode::Paper);
        assert_eq!(config.assets.len(), 2);
        assert_eq!(config.log_level, "debug");
        assert_eq!(config.clickhouse.url, "http://db:8123");
        assert_eq!(config.trading.min_margin_early, dec!(0.03));
        assert_eq!(config.trading.max_position_per_market, dec!(2000));
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
        config.trading.max_position_per_market = dec!(10000);
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
    // SizingConfig tests
    // =========================================================================

    #[test]
    fn test_sizing_config_default() {
        let config = SizingConfig::default();
        assert_eq!(config.available_balance, dec!(5000));
        assert_eq!(config.max_market_allocation, dec!(0.20));
        assert_eq!(config.expected_trades_per_market, 200);
        assert_eq!(config.min_order_size, dec!(1));
        assert_eq!(config.max_confidence_multiplier, dec!(3));
        assert_eq!(config.min_hedge_ratio, dec!(0.20));
    }

    #[test]
    fn test_sizing_config_new() {
        let config = SizingConfig::new(dec!(10000));
        assert_eq!(config.available_balance, dec!(10000));
        // Other fields should have defaults
        assert_eq!(config.max_market_allocation, dec!(0.20));
    }

    #[test]
    fn test_sizing_small_account_500() {
        // $500 account (very small)
        let config = SizingConfig::new(dec!(500));

        // Market budget = $500 * 0.20 = $100
        assert_eq!(config.market_budget(), dec!(100));

        // Base order size = $100 / 200 = $0.50, but min is $1.00
        assert_eq!(config.base_order_size(), dec!(1));

        // Daily loss limit = $500 * 0.10 = $50
        assert_eq!(config.daily_loss_limit(), dec!(50));
    }

    #[test]
    fn test_sizing_small_account_1000() {
        // $1,000 account
        let config = SizingConfig::new(dec!(1000));

        // Market budget = $1,000 * 0.20 = $200
        assert_eq!(config.market_budget(), dec!(200));

        // Base order size = $200 / 200 = $1.00 (exactly at min)
        assert_eq!(config.base_order_size(), dec!(1));

        // Daily loss limit = $1,000 * 0.10 = $100
        assert_eq!(config.daily_loss_limit(), dec!(100));
    }

    #[test]
    fn test_sizing_medium_account_5000() {
        // $5,000 account (default)
        let config = SizingConfig::new(dec!(5000));

        // Market budget = $5,000 * 0.20 = $1,000
        assert_eq!(config.market_budget(), dec!(1000));

        // Base order size = $1,000 / 200 = $5.00
        assert_eq!(config.base_order_size(), dec!(5));

        // Daily loss limit = $5,000 * 0.10 = $500
        assert_eq!(config.daily_loss_limit(), dec!(500));
    }

    #[test]
    fn test_sizing_large_account_25000() {
        // $25,000 account
        let config = SizingConfig::new(dec!(25000));

        // Market budget = $25,000 * 0.20 = $5,000
        assert_eq!(config.market_budget(), dec!(5000));

        // Base order size = $5,000 / 200 = $25.00
        assert_eq!(config.base_order_size(), dec!(25));

        // Daily loss limit = $25,000 * 0.10 = $2,500
        assert_eq!(config.daily_loss_limit(), dec!(2500));
    }

    #[test]
    fn test_sizing_custom_allocation() {
        // Custom allocation: 30% per market instead of 20%
        let mut config = SizingConfig::new(dec!(10000));
        config.max_market_allocation = dec!(0.30);

        // Market budget = $10,000 * 0.30 = $3,000
        assert_eq!(config.market_budget(), dec!(3000));

        // Base order size = $3,000 / 200 = $15.00
        assert_eq!(config.base_order_size(), dec!(15));
    }

    #[test]
    fn test_sizing_custom_trades_per_market() {
        // Custom trades: 100 instead of 200
        let mut config = SizingConfig::new(dec!(10000));
        config.expected_trades_per_market = 100;

        // Market budget = $10,000 * 0.20 = $2,000
        assert_eq!(config.market_budget(), dec!(2000));

        // Base order size = $2,000 / 100 = $20.00
        assert_eq!(config.base_order_size(), dec!(20));
    }

    #[test]
    fn test_sizing_min_order_floor() {
        // Very small account where calculated size < min
        let config = SizingConfig::new(dec!(100));

        // Market budget = $100 * 0.20 = $20
        assert_eq!(config.market_budget(), dec!(20));

        // Base order size = $20 / 200 = $0.10, floored to $1.00
        assert_eq!(config.base_order_size(), dec!(1));
    }

    #[test]
    fn test_sizing_toml_parsing() {
        let toml = r#"
            [general]
            mode = "paper"

            [trading]
            [trading.sizing]
            available_balance = 15000.0
            max_market_allocation = 0.25
            expected_trades_per_market = 150
            min_order_size = 2.0
            max_confidence_multiplier = 2.5
            min_hedge_ratio = 0.25
        "#;

        let config = BotConfig::from_toml_str(toml).unwrap();

        assert_eq!(config.trading.sizing.available_balance, dec!(15000));
        assert_eq!(config.trading.sizing.max_market_allocation, dec!(0.25));
        assert_eq!(config.trading.sizing.expected_trades_per_market, 150);
        assert_eq!(config.trading.sizing.min_order_size, dec!(2));
        assert_eq!(config.trading.sizing.max_confidence_multiplier, dec!(2.5));
        assert_eq!(config.trading.sizing.min_hedge_ratio, dec!(0.25));

        // Verify calculated values
        // Market budget = $15,000 * 0.25 = $3,750
        assert_eq!(config.trading.sizing.market_budget(), dec!(3750));

        // Base order size = $3,750 / 150 = $25.00
        assert_eq!(config.trading.sizing.base_order_size(), dec!(25));
    }
}
