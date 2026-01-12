//! Configuration for poly-collect.
//!
//! Supports loading from TOML file with CLI argument overrides.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use poly_common::{ClickHouseConfig, CryptoAsset};
use serde::Deserialize;

use crate::binance::BinanceConfig;
use crate::clob::ClobConfig;

/// Top-level configuration for poly-collect.
#[derive(Debug, Clone)]
pub struct CollectConfig {
    pub assets: Vec<CryptoAsset>,
    pub discovery_interval: Duration,
    pub log_level: String,
    pub clickhouse: ClickHouseConfig,
    pub binance: BinanceConfig,
    pub clob: ClobConfig,
    pub health_log_interval: Duration,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            assets: vec![CryptoAsset::Btc, CryptoAsset::Eth, CryptoAsset::Sol],
            discovery_interval: Duration::from_secs(300),
            log_level: "info".to_string(),
            clickhouse: ClickHouseConfig::default(),
            binance: BinanceConfig::default(),
            clob: ClobConfig::default(),
            health_log_interval: Duration::from_secs(30),
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
    clickhouse: ClickHouseToml,
    #[serde(default)]
    binance: BinanceToml,
    #[serde(default)]
    clob: ClobToml,
    #[serde(default)]
    health: HealthToml,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct GeneralConfig {
    assets: Vec<String>,
    discovery_interval_secs: u64,
    log_level: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            assets: vec!["BTC".to_string(), "ETH".to_string(), "SOL".to_string()],
            discovery_interval_secs: 300,
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

        Self {
            assets,
            discovery_interval: Duration::from_secs(toml.general.discovery_interval_secs),
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
            discovery_interval_secs = 600
            log_level = "debug"

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
