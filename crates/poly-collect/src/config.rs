//! Configuration for poly-collect.
//!
//! Supports loading from TOML file with CLI argument overrides.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use poly_common::{ClickHouseConfig, CryptoAsset, WindowDuration};
use serde::Deserialize;

/// Collection mode.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum CollectionMode {
    /// Live streaming via WebSocket (default)
    #[default]
    Live,
    /// Historical data fetching from APIs
    History,
}

/// Output mode for collected data.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum OutputMode {
    /// Store data in ClickHouse database only (default)
    #[default]
    ClickHouse,
    /// Store data in CSV files only
    Csv {
        /// Directory to store CSV files
        output_dir: PathBuf,
    },
    /// Store data in both ClickHouse and CSV
    Both {
        /// Directory to store CSV files
        output_dir: PathBuf,
    },
}

impl OutputMode {
    /// Returns true if ClickHouse storage is enabled.
    pub fn use_clickhouse(&self) -> bool {
        matches!(self, OutputMode::ClickHouse | OutputMode::Both { .. })
    }

    /// Returns true if CSV storage is enabled.
    pub fn use_csv(&self) -> bool {
        matches!(self, OutputMode::Csv { .. } | OutputMode::Both { .. })
    }

    /// Returns the CSV output directory if CSV is enabled.
    pub fn csv_dir(&self) -> Option<&PathBuf> {
        match self {
            OutputMode::Csv { output_dir } | OutputMode::Both { output_dir } => Some(output_dir),
            OutputMode::ClickHouse => None,
        }
    }
}

use crate::binance::BinanceConfig;
use crate::clob::ClobConfig;

/// Collection duration specification.
#[derive(Debug, Clone, Default)]
pub struct CollectionDuration {
    /// Run for this many days
    pub days: Option<u32>,
    /// Run for this many hours
    pub hours: Option<u32>,
    /// Run until this date (YYYY-MM-DD)
    pub end_date: Option<NaiveDate>,
}

impl CollectionDuration {
    /// Calculate when collection should stop.
    /// Returns None if collection should run indefinitely.
    pub fn stop_time(&self) -> Option<chrono::DateTime<Utc>> {
        if let Some(days) = self.days {
            return Some(Utc::now() + chrono::Duration::days(days as i64));
        }
        if let Some(hours) = self.hours {
            return Some(Utc::now() + chrono::Duration::hours(hours as i64));
        }
        if let Some(end_date) = self.end_date {
            // End at midnight UTC of the end date
            return end_date.and_hms_opt(23, 59, 59)
                .map(|dt| chrono::DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc));
        }
        None
    }

    /// Check if we should stop now.
    pub fn should_stop(&self) -> bool {
        self.stop_time().map(|t| Utc::now() >= t).unwrap_or(false)
    }

    /// Returns true if collection should run indefinitely.
    pub fn is_indefinite(&self) -> bool {
        self.days.is_none() && self.hours.is_none() && self.end_date.is_none()
    }
}

/// Top-level configuration for poly-collect.
#[derive(Debug, Clone)]
pub struct CollectConfig {
    pub mode: CollectionMode,
    pub assets: Vec<CryptoAsset>,
    /// Timeframes to collect (5m, 15m, 1h). Used for market filtering.
    pub timeframes: Vec<WindowDuration>,
    pub start_date: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    pub discovery_interval: Duration,
    pub log_level: String,
    pub output: OutputMode,
    pub clickhouse: ClickHouseConfig,
    pub binance: BinanceConfig,
    pub clob: ClobConfig,
    pub health_log_interval: Duration,
    pub duration: CollectionDuration,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            mode: CollectionMode::default(),
            assets: vec![CryptoAsset::Btc, CryptoAsset::Eth, CryptoAsset::Sol],
            timeframes: vec![WindowDuration::FifteenMin],
            start_date: None,
            end_date: None,
            discovery_interval: Duration::from_secs(300),
            log_level: "info".to_string(),
            output: OutputMode::default(),
            clickhouse: ClickHouseConfig::default(),
            binance: BinanceConfig::default(),
            clob: ClobConfig::default(),
            health_log_interval: Duration::from_secs(30),
            duration: CollectionDuration::default(),
        }
    }
}

impl CollectConfig {
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

    /// Apply CLI overrides to the configuration.
    pub fn apply_overrides(
        &mut self,
        assets: Option<Vec<String>>,
        clickhouse_url: Option<String>,
    ) {
        if let Some(asset_strs) = assets {
            let mut parsed_assets = Vec::new();
            for s in asset_strs {
                if let Some(asset) = parse_asset(&s) {
                    parsed_assets.push(asset);
                }
            }
            if !parsed_assets.is_empty() {
                self.assets = parsed_assets;
                // Update Binance symbols based on assets
                self.binance.symbols = self
                    .assets
                    .iter()
                    .map(|a| a.binance_symbol().to_lowercase())
                    .collect();
            }
        }

        if let Some(url) = clickhouse_url {
            self.clickhouse.url = url;
        }
    }
}

/// Parse asset string to CryptoAsset.
fn parse_asset(s: &str) -> Option<CryptoAsset> {
    match s.to_uppercase().as_str() {
        "BTC" | "BITCOIN" => Some(CryptoAsset::Btc),
        "ETH" | "ETHEREUM" => Some(CryptoAsset::Eth),
        "SOL" | "SOLANA" => Some(CryptoAsset::Sol),
        "XRP" | "RIPPLE" => Some(CryptoAsset::Xrp),
        _ => None,
    }
}

/// TOML file structure for deserialization.
#[derive(Debug, Deserialize)]
struct TomlConfig {
    #[serde(default)]
    general: GeneralConfig,
    #[serde(default)]
    collection: CollectionToml,
    #[serde(default)]
    output: OutputToml,
    #[serde(default)]
    live: LiveToml,
    #[serde(default)]
    clickhouse: ClickHouseToml,
    #[serde(default)]
    binance: BinanceToml,
    #[serde(default)]
    clob: ClobToml,
    #[serde(default)]
    health: HealthToml,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct CollectionToml {
    /// Collection mode: "live" or "history"
    mode: Option<String>,
    /// Start date for collection (YYYY-MM-DD)
    start_date: Option<String>,
    /// End date for collection (YYYY-MM-DD)
    end_date: Option<String>,
    /// Alternative: collect for N days (from today backwards for history, forward for live)
    days: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct LiveToml {
    /// Discovery interval in seconds
    discovery_interval_secs: u64,
}

impl Default for LiveToml {
    fn default() -> Self {
        Self {
            discovery_interval_secs: 300,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct OutputToml {
    /// Output mode: "clickhouse" or "csv"
    mode: String,
    /// Directory for CSV output (only used when mode = "csv")
    csv_dir: Option<String>,
}

impl Default for OutputToml {
    fn default() -> Self {
        Self {
            mode: "clickhouse".to_string(),
            csv_dir: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct GeneralConfig {
    assets: Vec<String>,
    /// Timeframes to collect: "5m", "15m", "1h"
    timeframes: Vec<String>,
    log_level: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            assets: vec!["BTC".to_string(), "ETH".to_string(), "SOL".to_string()],
            timeframes: vec!["15m".to_string()],
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
struct BinanceToml {
    buffer_size: usize,
    flush_interval_ms: u64,
    connect_timeout_secs: u64,
    initial_reconnect_delay_secs: u64,
    max_reconnect_delay_secs: u64,
}

impl Default for BinanceToml {
    fn default() -> Self {
        Self {
            buffer_size: 500,
            flush_interval_ms: 5000,
            connect_timeout_secs: 10,
            initial_reconnect_delay_secs: 1,
            max_reconnect_delay_secs: 60,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ClobToml {
    snapshot_buffer_size: usize,
    delta_buffer_size: usize,
    flush_interval_ms: u64,
    snapshot_interval_ms: u64,
    connect_timeout_secs: u64,
    initial_reconnect_delay_secs: u64,
    max_reconnect_delay_secs: u64,
}

impl Default for ClobToml {
    fn default() -> Self {
        Self {
            snapshot_buffer_size: 500,
            delta_buffer_size: 1000,
            flush_interval_ms: 5000,
            snapshot_interval_ms: 100,
            connect_timeout_secs: 10,
            initial_reconnect_delay_secs: 1,
            max_reconnect_delay_secs: 60,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct HealthToml {
    log_interval_secs: u64,
}

impl Default for HealthToml {
    fn default() -> Self {
        Self {
            log_interval_secs: 30,
        }
    }
}

/// Build ClickHouse config from TOML, then override with environment variables.
fn build_clickhouse_config(toml: &ClickHouseToml) -> ClickHouseConfig {
    // Start with TOML defaults
    let mut url = toml.url.clone();
    let mut database = toml.database.clone();
    let mut user = None;
    let mut password = None;

    // Override with environment variables if set
    // CLICKHOUSE_URL + CLICKHOUSE_HTTP_PORT -> http://{url}:{port}
    if let Ok(host) = std::env::var("CLICKHOUSE_URL") {
        let port = std::env::var("CLICKHOUSE_HTTP_PORT").unwrap_or_else(|_| "8123".to_string());
        url = format!("http://{}:{}", host, port);
    }

    if let Ok(db) = std::env::var("CLICKHOUSE_DATABASE") {
        database = db;
    }

    if let Ok(u) = std::env::var("CLICKHOUSE_USER") {
        user = Some(u);
    }

    if let Ok(p) = std::env::var("CLICKHOUSE_PASSWORD") {
        password = Some(p);
    }

    ClickHouseConfig {
        url,
        database,
        user,
        password,
        max_rows: toml.max_rows,
        max_bytes: toml.max_bytes,
        commit_period: Duration::from_secs(toml.period_secs),
    }
}

impl From<TomlConfig> for CollectConfig {
    fn from(toml: TomlConfig) -> Self {
        // Parse assets
        let assets: Vec<CryptoAsset> = toml
            .general
            .assets
            .iter()
            .filter_map(|s| parse_asset(s))
            .collect();

        // Build Binance symbols from assets
        let binance_symbols: Vec<String> = assets
            .iter()
            .map(|a| a.binance_symbol().to_lowercase())
            .collect();

        // Parse collection mode
        let mode = match toml.collection.mode.as_deref() {
            Some("history") | Some("historical") => CollectionMode::History,
            _ => CollectionMode::Live,
        };

        // Parse dates for historical mode
        let start_date = toml.collection.start_date.as_ref().and_then(|s| {
            NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
        });
        let end_date = toml.collection.end_date.as_ref().and_then(|s| {
            NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
        });

        // Parse collection duration (for live mode)
        let duration = CollectionDuration {
            days: toml.collection.days,
            hours: None,
            end_date: None,
        };

        // Parse timeframes
        let timeframes: Vec<WindowDuration> = toml
            .general
            .timeframes
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        let timeframes = if timeframes.is_empty() {
            vec![WindowDuration::FifteenMin]
        } else {
            timeframes
        };

        // Parse output mode
        let output = match toml.output.mode.to_lowercase().as_str() {
            "csv" => {
                let dir = toml.output.csv_dir.unwrap_or_else(|| "data".to_string());
                OutputMode::Csv {
                    output_dir: PathBuf::from(dir),
                }
            }
            "both" => {
                let dir = toml.output.csv_dir.unwrap_or_else(|| "data".to_string());
                OutputMode::Both {
                    output_dir: PathBuf::from(dir),
                }
            }
            _ => OutputMode::ClickHouse,
        };

        // Build ClickHouse config with env var overrides
        let clickhouse = build_clickhouse_config(&toml.clickhouse);

        Self {
            mode,
            assets,
            timeframes,
            start_date,
            end_date,
            discovery_interval: Duration::from_secs(toml.live.discovery_interval_secs),
            log_level: toml.general.log_level,
            output,
            clickhouse,
            binance: BinanceConfig {
                symbols: binance_symbols,
                buffer_size: toml.binance.buffer_size,
                flush_interval: Duration::from_millis(toml.binance.flush_interval_ms),
                connect_timeout: Duration::from_secs(toml.binance.connect_timeout_secs),
                initial_reconnect_delay: Duration::from_secs(
                    toml.binance.initial_reconnect_delay_secs,
                ),
                max_reconnect_delay: Duration::from_secs(toml.binance.max_reconnect_delay_secs),
            },
            clob: ClobConfig {
                snapshot_buffer_size: toml.clob.snapshot_buffer_size,
                delta_buffer_size: toml.clob.delta_buffer_size,
                flush_interval: Duration::from_millis(toml.clob.flush_interval_ms),
                snapshot_interval: Duration::from_millis(toml.clob.snapshot_interval_ms),
                connect_timeout: Duration::from_secs(toml.clob.connect_timeout_secs),
                initial_reconnect_delay: Duration::from_secs(
                    toml.clob.initial_reconnect_delay_secs,
                ),
                max_reconnect_delay: Duration::from_secs(toml.clob.max_reconnect_delay_secs),
            },
            health_log_interval: Duration::from_secs(toml.health.log_interval_secs),
            duration,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = CollectConfig::default();
        assert_eq!(config.assets.len(), 3);
        assert!(config.assets.contains(&CryptoAsset::Btc));
        assert!(config.assets.contains(&CryptoAsset::Eth));
        assert!(config.assets.contains(&CryptoAsset::Sol));
    }

    #[test]
    fn test_parse_asset() {
        assert_eq!(parse_asset("BTC"), Some(CryptoAsset::Btc));
        assert_eq!(parse_asset("btc"), Some(CryptoAsset::Btc));
        assert_eq!(parse_asset("BITCOIN"), Some(CryptoAsset::Btc));
        assert_eq!(parse_asset("ETH"), Some(CryptoAsset::Eth));
        assert_eq!(parse_asset("SOL"), Some(CryptoAsset::Sol));
        assert_eq!(parse_asset("XRP"), Some(CryptoAsset::Xrp));
        assert_eq!(parse_asset("UNKNOWN"), None);
    }

    #[test]
    fn test_parse_toml() {
        let toml = r#"
            [general]
            assets = ["BTC", "ETH"]
            log_level = "debug"

            [live]
            discovery_interval_secs = 600

            [clickhouse]
            url = "http://db:8123"

            [binance]
            buffer_size = 1000
        "#;

        let config = CollectConfig::from_toml_str(toml).unwrap();
        assert_eq!(config.assets.len(), 2);
        assert_eq!(config.discovery_interval, Duration::from_secs(600));
        assert_eq!(config.log_level, "debug");
        assert_eq!(config.clickhouse.url, "http://db:8123");
        assert_eq!(config.binance.buffer_size, 1000);
    }

    #[test]
    fn test_apply_overrides() {
        let mut config = CollectConfig::default();

        config.apply_overrides(
            Some(vec!["ETH".to_string(), "XRP".to_string()]),
            Some("http://override:8123".to_string()),
        );

        assert_eq!(config.assets.len(), 2);
        assert!(config.assets.contains(&CryptoAsset::Eth));
        assert!(config.assets.contains(&CryptoAsset::Xrp));
        assert_eq!(config.clickhouse.url, "http://override:8123");
        assert_eq!(config.binance.symbols.len(), 2);
        assert!(config.binance.symbols.contains(&"ethusdt".to_string()));
        assert!(config.binance.symbols.contains(&"xrpusdt".to_string()));
    }
}
