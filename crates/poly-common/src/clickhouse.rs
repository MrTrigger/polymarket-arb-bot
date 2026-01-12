//! ClickHouse client wrapper for the Polymarket arbitrage bot.
//!
//! Provides a type-safe interface for batch inserts and schema management.

use std::time::Duration;

use clickhouse::inserter::Inserter;
use clickhouse::Client;
use thiserror::Error;

use crate::{CounterfactualRecord, MarketWindow, OrderBookDelta, OrderBookSnapshot, PriceHistory, SpotPrice, TradeHistory};

/// Errors that can occur during ClickHouse operations.
#[derive(Debug, Error)]
pub enum ClickHouseError {
    #[error("ClickHouse client error: {0}")]
    Client(#[from] clickhouse::error::Error),

    #[error("Connection failed: {0}")]
    Connection(String),

    #[error("Schema creation failed: {0}")]
    Schema(String),
}

/// Configuration for the ClickHouse client.
#[derive(Debug, Clone)]
pub struct ClickHouseConfig {
    /// ClickHouse HTTP URL (e.g., "http://localhost:8123").
    pub url: String,
    /// Database name.
    pub database: String,
    /// Username (optional).
    pub user: Option<String>,
    /// Password (optional).
    pub password: Option<String>,
    /// Maximum rows before auto-commit in inserters.
    pub max_rows: u64,
    /// Maximum bytes before auto-commit in inserters.
    pub max_bytes: u64,
    /// Auto-commit period for inserters.
    pub commit_period: Duration,
}

impl Default for ClickHouseConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:8123".to_string(),
            database: "polymarket".to_string(),
            user: None,
            password: None,
            max_rows: 10_000,
            max_bytes: 10_000_000, // 10MB
            commit_period: Duration::from_secs(5),
        }
    }
}

/// ClickHouse client wrapper with type-safe inserters.
#[derive(Clone)]
pub struct ClickHouseClient {
    client: Client,
    config: ClickHouseConfig,
}

impl ClickHouseClient {
    /// Creates a new ClickHouse client with the given configuration.
    pub fn new(config: ClickHouseConfig) -> Self {
        let mut client = Client::default()
            .with_url(&config.url)
            .with_database(&config.database);

        if let Some(ref user) = config.user {
            client = client.with_user(user);
        }
        if let Some(ref password) = config.password {
            client = client.with_password(password);
        }

        Self { client, config }
    }

    /// Creates a client with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(ClickHouseConfig::default())
    }

    /// Returns a reference to the underlying clickhouse client.
    pub fn inner(&self) -> &Client {
        &self.client
    }

    /// Tests the connection by running a simple query.
    pub async fn ping(&self) -> Result<(), ClickHouseError> {
        self.client
            .query("SELECT 1")
            .fetch_one::<u8>()
            .await
            .map_err(|e| ClickHouseError::Connection(e.to_string()))?;
        Ok(())
    }

    /// Creates all required tables using the embedded schema.
    pub async fn create_tables(&self) -> Result<(), ClickHouseError> {
        let schema = include_str!("schema.sql");

        // Split by semicolons and execute each statement
        for statement in schema.split(';') {
            let statement = statement.trim();
            if statement.is_empty() || statement.starts_with("--") {
                continue;
            }

            // Skip comment-only blocks
            let non_comment_lines: Vec<&str> = statement
                .lines()
                .filter(|line| !line.trim().starts_with("--") && !line.trim().is_empty())
                .collect();

            if non_comment_lines.is_empty() {
                continue;
            }

            self.client
                .query(statement)
                .execute()
                .await
                .map_err(|e| ClickHouseError::Schema(format!("{}: {}", e, statement)))?;
        }

        Ok(())
    }

    /// Creates an inserter for spot prices with auto-commit configuration.
    pub fn spot_price_inserter(&self) -> Result<Inserter<SpotPrice>, ClickHouseError> {
        self.create_inserter("spot_prices")
    }

    /// Creates an inserter for order book snapshots with auto-commit configuration.
    pub fn snapshot_inserter(&self) -> Result<Inserter<OrderBookSnapshot>, ClickHouseError> {
        self.create_inserter("orderbook_snapshots")
    }

    /// Creates an inserter for order book deltas with auto-commit configuration.
    pub fn delta_inserter(&self) -> Result<Inserter<OrderBookDelta>, ClickHouseError> {
        self.create_inserter("orderbook_deltas")
    }

    /// Creates an inserter for market windows with auto-commit configuration.
    pub fn market_window_inserter(&self) -> Result<Inserter<MarketWindow>, ClickHouseError> {
        self.create_inserter("market_windows")
    }

    /// Creates an inserter for price history with auto-commit configuration.
    pub fn price_history_inserter(&self) -> Result<Inserter<PriceHistory>, ClickHouseError> {
        self.create_inserter("price_history")
    }

    /// Creates an inserter for trade history with auto-commit configuration.
    pub fn trade_history_inserter(&self) -> Result<Inserter<TradeHistory>, ClickHouseError> {
        self.create_inserter("trade_history")
    }

    /// Creates an inserter for counterfactuals with auto-commit configuration.
    pub fn counterfactual_inserter(&self) -> Result<Inserter<CounterfactualRecord>, ClickHouseError> {
        self.create_inserter("counterfactuals")
    }

    /// Creates a generic inserter with the configured auto-commit settings.
    fn create_inserter<T>(&self, table: &str) -> Result<Inserter<T>, ClickHouseError>
    where
        T: clickhouse::Row,
    {
        let inserter = self
            .client
            .inserter(table)?
            .with_max_rows(self.config.max_rows)
            .with_max_bytes(self.config.max_bytes)
            .with_period(Some(self.config.commit_period));

        Ok(inserter)
    }

    /// Performs a single batch insert of spot prices.
    pub async fn insert_spot_prices(&self, prices: &[SpotPrice]) -> Result<(), ClickHouseError> {
        if prices.is_empty() {
            return Ok(());
        }

        let mut insert = self.client.insert("spot_prices")?;
        for price in prices {
            insert.write(price).await?;
        }
        insert.end().await?;
        Ok(())
    }

    /// Performs a single batch insert of order book snapshots.
    pub async fn insert_snapshots(
        &self,
        snapshots: &[OrderBookSnapshot],
    ) -> Result<(), ClickHouseError> {
        if snapshots.is_empty() {
            return Ok(());
        }

        let mut insert = self.client.insert("orderbook_snapshots")?;
        for snapshot in snapshots {
            insert.write(snapshot).await?;
        }
        insert.end().await?;
        Ok(())
    }

    /// Performs a single batch insert of order book deltas.
    pub async fn insert_deltas(&self, deltas: &[OrderBookDelta]) -> Result<(), ClickHouseError> {
        if deltas.is_empty() {
            return Ok(());
        }

        let mut insert = self.client.insert("orderbook_deltas")?;
        for delta in deltas {
            insert.write(delta).await?;
        }
        insert.end().await?;
        Ok(())
    }

    /// Performs a single batch insert of market windows.
    pub async fn insert_market_windows(
        &self,
        windows: &[MarketWindow],
    ) -> Result<(), ClickHouseError> {
        if windows.is_empty() {
            return Ok(());
        }

        let mut insert = self.client.insert("market_windows")?;
        for window in windows {
            insert.write(window).await?;
        }
        insert.end().await?;
        Ok(())
    }

    /// Performs a single batch insert of price history.
    pub async fn insert_price_history(
        &self,
        prices: &[PriceHistory],
    ) -> Result<(), ClickHouseError> {
        if prices.is_empty() {
            return Ok(());
        }

        let mut insert = self.client.insert("price_history")?;
        for price in prices {
            insert.write(price).await?;
        }
        insert.end().await?;
        Ok(())
    }

    /// Performs a single batch insert of trade history.
    pub async fn insert_trade_history(
        &self,
        trades: &[TradeHistory],
    ) -> Result<(), ClickHouseError> {
        if trades.is_empty() {
            return Ok(());
        }

        let mut insert = self.client.insert("trade_history")?;
        for trade in trades {
            insert.write(trade).await?;
        }
        insert.end().await?;
        Ok(())
    }

    /// Performs a single batch insert of counterfactuals.
    pub async fn insert_counterfactuals(
        &self,
        counterfactuals: &[CounterfactualRecord],
    ) -> Result<(), ClickHouseError> {
        if counterfactuals.is_empty() {
            return Ok(());
        }

        let mut insert = self.client.insert("counterfactuals")?;
        for cf in counterfactuals {
            insert.write(cf).await?;
        }
        insert.end().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ClickHouseConfig::default();
        assert_eq!(config.url, "http://localhost:8123");
        assert_eq!(config.database, "polymarket");
        assert!(config.user.is_none());
        assert!(config.password.is_none());
        assert_eq!(config.max_rows, 10_000);
    }

    #[test]
    fn test_client_creation() {
        let config = ClickHouseConfig {
            url: "http://clickhouse:8123".to_string(),
            database: "test".to_string(),
            user: Some("admin".to_string()),
            password: Some("secret".to_string()),
            ..Default::default()
        };
        let _client = ClickHouseClient::new(config);
        // Client creation should not panic
    }

    #[test]
    fn test_client_with_defaults() {
        let _client = ClickHouseClient::with_defaults();
        // Should create successfully with defaults
    }
}
