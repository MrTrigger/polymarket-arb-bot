//! Poly-collect: Real-time data collector for Polymarket arbitrage bot.
//!
//! Usage:
//!   poly-collect [OPTIONS]
//!
//! Options:
//!   --clickhouse-url <URL>  ClickHouse HTTP URL (default: http://localhost:8123)
//!   --assets <ASSETS>       Comma-separated assets to track (default: BTC,ETH,SOL)

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use poly_common::{ClickHouseClient, ClickHouseConfig, CryptoAsset};
use tokio::sync::broadcast;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use poly_collect::discovery::MarketDiscovery;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Starting poly-collect data collector");

    // Parse configuration (simplified for now)
    let config = ClickHouseConfig::default();
    let assets = vec![CryptoAsset::Btc, CryptoAsset::Eth, CryptoAsset::Sol];

    // Create ClickHouse client
    let db = Arc::new(ClickHouseClient::new(config));

    // Test connection
    info!("Testing ClickHouse connection...");
    if let Err(e) = db.ping().await {
        tracing::warn!("ClickHouse not available: {}. Continuing anyway for discovery testing.", e);
    } else {
        info!("ClickHouse connection successful");
        // Create tables if needed
        if let Err(e) = db.create_tables().await {
            tracing::warn!("Failed to create tables: {}", e);
        }
    }

    // Create shutdown channel
    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

    // Spawn discovery task
    let discovery = MarketDiscovery::new(Arc::clone(&db), assets);
    let discovery_handle = tokio::spawn(async move {
        if let Err(e) = discovery.run_loop(Duration::from_secs(300), shutdown_rx).await {
            tracing::error!("Discovery loop error: {}", e);
        }
    });

    // Handle shutdown signal
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal");
            let _ = shutdown_tx.send(());
        }
    }

    // Wait for tasks to complete
    let _ = discovery_handle.await;

    info!("Shutdown complete");
    Ok(())
}
