//! poly-import: Historical data import CLI for the Polymarket arbitrage bot.
//!
//! Imports price history and trade history from Polymarket APIs into ClickHouse.
//!
//! # Usage
//!
//! Import price history:
//! ```sh
//! poly-import prices --start 2024-01-01 --end 2024-01-31 --tokens TOKEN1,TOKEN2
//! ```
//!
//! Import trade history:
//! ```sh
//! poly-import trades --start 2024-01-01 --end 2024-01-31 --markets MARKET1,MARKET2
//! ```

use std::process::ExitCode;

use anyhow::{Context, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use clap::{Parser, Subcommand};
use poly_common::{ClickHouseClient, ClickHouseConfig};
use poly_import::{
    prices::{PriceImportConfig, PriceImporter},
    trades::{L2Auth, TradeImportConfig, TradeImporter},
};
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, EnvFilter};

/// Historical data importer for Polymarket arbitrage bot.
#[derive(Parser, Debug)]
#[command(name = "poly-import")]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// ClickHouse server URL
    #[arg(long, env = "CLICKHOUSE_URL", default_value = "http://localhost:8123")]
    clickhouse_url: String,

    /// ClickHouse database name
    #[arg(long, env = "CLICKHOUSE_DATABASE", default_value = "polymarket")]
    clickhouse_database: String,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    log_level: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Import price history from Polymarket API
    Prices {
        /// Start date (YYYY-MM-DD format)
        #[arg(long, short = 's')]
        start: String,

        /// End date (YYYY-MM-DD format)
        #[arg(long, short = 'e')]
        end: String,

        /// Comma-separated list of token IDs to import
        #[arg(long, short = 't')]
        tokens: String,

        /// Price fidelity in minutes (resolution)
        #[arg(long, short = 'f')]
        fidelity: Option<u32>,

        /// Maximum requests per second
        #[arg(long, default_value = "5.0")]
        rate_limit: f64,

        /// Batch size for ClickHouse inserts
        #[arg(long, default_value = "1000")]
        batch_size: usize,
    },

    /// Import trade history from Polymarket API
    Trades {
        /// Start date (YYYY-MM-DD format)
        #[arg(long, short = 's')]
        start: String,

        /// End date (YYYY-MM-DD format)
        #[arg(long, short = 'e')]
        end: String,

        /// Comma-separated list of market/condition IDs to import
        #[arg(long, short = 'm')]
        markets: String,

        /// Maximum requests per second
        #[arg(long, default_value = "5.0")]
        rate_limit: f64,

        /// Batch size for ClickHouse inserts
        #[arg(long, default_value = "1000")]
        batch_size: usize,

        /// POLY_ADDRESS for L2 authentication
        #[arg(long, env = "POLY_ADDRESS")]
        poly_address: Option<String>,

        /// POLY_SIGNATURE for L2 authentication
        #[arg(long, env = "POLY_SIGNATURE")]
        poly_signature: Option<String>,

        /// POLY_TIMESTAMP for L2 authentication
        #[arg(long, env = "POLY_TIMESTAMP")]
        poly_timestamp: Option<String>,

        /// POLY_NONCE for L2 authentication (optional)
        #[arg(long, env = "POLY_NONCE")]
        poly_nonce: Option<String>,
    },
}

/// Parse a date string in YYYY-MM-DD format to UTC datetime (start of day).
fn parse_date(date_str: &str) -> Result<chrono::DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .with_context(|| format!("Invalid date format: {}. Expected YYYY-MM-DD", date_str))?;

    let datetime = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow::anyhow!("Failed to create datetime from date: {}", date_str))?;

    Ok(Utc.from_utc_datetime(&datetime))
}

/// Parse a date string in YYYY-MM-DD format to UTC datetime (end of day).
fn parse_date_end_of_day(date_str: &str) -> Result<chrono::DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .with_context(|| format!("Invalid date format: {}. Expected YYYY-MM-DD", date_str))?;

    let datetime = date
        .and_hms_opt(23, 59, 59)
        .ok_or_else(|| anyhow::anyhow!("Failed to create datetime from date: {}", date_str))?;

    Ok(Utc.from_utc_datetime(&datetime))
}

/// Split a comma-separated string into a vector of trimmed strings.
fn parse_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

async fn run_prices_import(
    ch_client: ClickHouseClient,
    start: &str,
    end: &str,
    tokens: &str,
    fidelity: Option<u32>,
    rate_limit: f64,
    batch_size: usize,
) -> Result<()> {
    let start_dt = parse_date(start)?;
    let end_dt = parse_date_end_of_day(end)?;
    let token_list = parse_list(tokens);

    if token_list.is_empty() {
        anyhow::bail!("No tokens specified. Use --tokens TOKEN1,TOKEN2,...");
    }

    info!(
        "Starting price history import: {} tokens, {} to {}",
        token_list.len(),
        start_dt,
        end_dt
    );

    if let Some(fid) = fidelity {
        info!("Fidelity: {} minutes", fid);
    }

    let config = PriceImportConfig {
        requests_per_second: rate_limit,
        batch_size,
        ..Default::default()
    };

    let importer = PriceImporter::new(ch_client, config)?;
    let stats = importer
        .import_tokens(&token_list, start_dt, end_dt, fidelity)
        .await?;

    info!("Price import complete:\n{}", stats);

    if stats.tokens_failed > 0 {
        warn!("{} tokens failed to import", stats.tokens_failed);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_trades_import(
    ch_client: ClickHouseClient,
    start: &str,
    end: &str,
    markets: &str,
    rate_limit: f64,
    batch_size: usize,
    poly_address: Option<String>,
    poly_signature: Option<String>,
    poly_timestamp: Option<String>,
    poly_nonce: Option<String>,
) -> Result<()> {
    let start_dt = parse_date(start)?;
    let end_dt = parse_date_end_of_day(end)?;
    let market_list = parse_list(markets);

    if market_list.is_empty() {
        anyhow::bail!("No markets specified. Use --markets MARKET1,MARKET2,...");
    }

    info!(
        "Starting trade history import: {} markets, {} to {}",
        market_list.len(),
        start_dt,
        end_dt
    );

    let config = TradeImportConfig {
        requests_per_second: rate_limit,
        batch_size,
        ..Default::default()
    };

    // Check for L2 authentication credentials
    let importer = if let (Some(address), Some(signature), Some(timestamp)) =
        (poly_address, poly_signature, poly_timestamp)
    {
        info!("Using L2 authentication");
        let auth = L2Auth {
            address,
            signature,
            timestamp,
            nonce: poly_nonce,
        };
        TradeImporter::with_auth(ch_client, config, auth)?
    } else {
        warn!("No L2 authentication provided. Some trade data may not be accessible.");
        warn!("Set POLY_ADDRESS, POLY_SIGNATURE, and POLY_TIMESTAMP environment variables");
        warn!("or use --poly-address, --poly-signature, --poly-timestamp flags");
        TradeImporter::new(ch_client, config)?
    };

    let stats = importer
        .import_markets(&market_list, start_dt, end_dt)
        .await?;

    info!("Trade import complete:\n{}", stats);

    if stats.markets_failed > 0 {
        warn!("{} markets failed to import", stats.markets_failed);
    }

    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Initialize logging
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cli.log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    info!("poly-import v{}", env!("CARGO_PKG_VERSION"));

    // Initialize ClickHouse client
    let ch_config = ClickHouseConfig {
        url: cli.clickhouse_url.clone(),
        database: cli.clickhouse_database.clone(),
        ..Default::default()
    };

    let ch_client = ClickHouseClient::new(ch_config);

    // Test ClickHouse connection
    info!("Connecting to ClickHouse at {}", cli.clickhouse_url);
    if let Err(e) = ch_client.ping().await {
        error!("Failed to connect to ClickHouse: {}", e);
        error!("Ensure ClickHouse is running and accessible at {}", cli.clickhouse_url);
        return ExitCode::FAILURE;
    }
    info!("ClickHouse connection established");

    // Ensure tables exist
    if let Err(e) = ch_client.create_tables().await {
        error!("Failed to create tables: {}", e);
        return ExitCode::FAILURE;
    }

    // Run the appropriate subcommand
    let result = match cli.command {
        Commands::Prices {
            start,
            end,
            tokens,
            fidelity,
            rate_limit,
            batch_size,
        } => {
            run_prices_import(
                ch_client,
                &start,
                &end,
                &tokens,
                fidelity,
                rate_limit,
                batch_size,
            )
            .await
        }
        Commands::Trades {
            start,
            end,
            markets,
            rate_limit,
            batch_size,
            poly_address,
            poly_signature,
            poly_timestamp,
            poly_nonce,
        } => {
            run_trades_import(
                ch_client,
                &start,
                &end,
                &markets,
                rate_limit,
                batch_size,
                poly_address,
                poly_signature,
                poly_timestamp,
                poly_nonce,
            )
            .await
        }
    };

    match result {
        Ok(()) => {
            info!("Import completed successfully");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("Import failed: {:#}", e);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_date() {
        let dt = parse_date("2024-01-15").unwrap();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2024-01-15");
        // Start of day
        assert_eq!(dt.format("%H:%M:%S").to_string(), "00:00:00");
    }

    #[test]
    fn test_parse_date_end_of_day() {
        let dt = parse_date_end_of_day("2024-01-15").unwrap();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2024-01-15");
        // End of day
        assert_eq!(dt.format("%H:%M:%S").to_string(), "23:59:59");
    }

    #[test]
    fn test_parse_date_invalid() {
        assert!(parse_date("invalid").is_err());
        assert!(parse_date("01-15-2024").is_err());
        assert!(parse_date("2024/01/15").is_err());
    }

    #[test]
    fn test_parse_list() {
        let list = parse_list("a,b,c");
        assert_eq!(list, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_parse_list_with_spaces() {
        let list = parse_list(" a , b , c ");
        assert_eq!(list, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_parse_list_empty() {
        let list = parse_list("");
        assert!(list.is_empty());
    }

    #[test]
    fn test_parse_list_single() {
        let list = parse_list("single");
        assert_eq!(list, vec!["single"]);
    }

    #[test]
    fn test_cli_parsing() {
        // Test that CLI can be parsed (compile-time check)
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
