//! Poly-collect: Real-time data collector for Polymarket arbitrage bot.
//!
//! Usage:
//!   poly-collect [OPTIONS]
//!
//! Options:
//!   -c, --config <FILE>           Config file path (default: config/collect.toml)
//!   --clickhouse-url <URL>        ClickHouse HTTP URL (overrides config)
//!   --assets <ASSETS>             Comma-separated assets to track (overrides config)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use poly_common::{ClickHouseClient, CryptoAsset};
use tokio::sync::{broadcast, RwLock};
use tokio::time::interval;
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use poly_collect::binance::BinanceCapture;
use poly_collect::clob::{update_active_markets, ActiveMarkets, ClobCapture};
use poly_collect::config::CollectConfig;
use poly_collect::discovery::MarketDiscovery;

/// CLI arguments for poly-collect.
#[derive(Parser, Debug)]
#[command(name = "poly-collect")]
#[command(about = "Real-time data collector for Polymarket arbitrage bot")]
#[command(version)]
struct Args {
    /// Config file path
    #[arg(short, long, default_value = "config/collect.toml")]
    config: PathBuf,

    /// ClickHouse HTTP URL (overrides config file)
    #[arg(long)]
    clickhouse_url: Option<String>,

    /// Comma-separated assets to track (e.g., "BTC,ETH,SOL")
    #[arg(long, value_delimiter = ',')]
    assets: Option<Vec<String>>,
}

/// Health statistics for monitoring.
#[derive(Default)]
struct HealthStats {
    discovery_runs: AtomicU64,
    markets_discovered: AtomicU64,
    errors: AtomicU64,
}

impl HealthStats {
    fn log_stats(&self) {
        info!(
            "Health: discovery_runs={}, markets={}, errors={}",
            self.discovery_runs.load(Ordering::Relaxed),
            self.markets_discovered.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments
    let args = Args::parse();

    // Load configuration
    let mut config = if args.config.exists() {
        CollectConfig::from_file(&args.config)?
    } else {
        warn!(
            "Config file not found at {:?}, using defaults",
            args.config
        );
        CollectConfig::default()
    };

    // Apply CLI overrides
    config.apply_overrides(args.assets, args.clickhouse_url);

    // Initialize logging
    let log_level = match config.log_level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };

    let subscriber = FmtSubscriber::builder().with_max_level(log_level).finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Starting poly-collect data collector");
    info!("Assets: {:?}", config.assets);
    info!("ClickHouse URL: {}", config.clickhouse.url);

    // Create ClickHouse client
    let db = Arc::new(ClickHouseClient::new(config.clickhouse.clone()));

    // Test connection
    info!("Testing ClickHouse connection...");
    match db.ping().await {
        Ok(()) => {
            info!("ClickHouse connection successful");
            // Create tables if needed
            if let Err(e) = db.create_tables().await {
                warn!("Failed to create tables: {}", e);
            }
        }
        Err(e) => {
            warn!(
                "ClickHouse not available: {}. Continuing anyway for data capture.",
                e
            );
        }
    }

    // Shared state for active markets (discovery writes, CLOB reads)
    let active_markets: ActiveMarkets = Arc::new(RwLock::new(HashMap::new()));

    // Create shutdown channel (capacity for all subscribers)
    let (shutdown_tx, _) = broadcast::channel::<()>(16);

    // Health statistics
    let stats = Arc::new(HealthStats::default());

    // Spawn discovery task with integrated active markets update
    let discovery_handle = spawn_discovery_task(
        Arc::clone(&db),
        config.assets.clone(),
        Arc::clone(&active_markets),
        Arc::clone(&stats),
        config.discovery_interval,
        shutdown_tx.subscribe(),
    );
    info!("Discovery task started");

    // Spawn Binance capture task
    let binance_handle = spawn_binance_task(
        Arc::clone(&db),
        config.binance.clone(),
        shutdown_tx.subscribe(),
    );
    info!("Binance capture task started");

    // Spawn CLOB capture task
    let clob_handle = spawn_clob_task(
        Arc::clone(&db),
        config.clob.clone(),
        Arc::clone(&active_markets),
        shutdown_tx.subscribe(),
    );
    info!("CLOB capture task started");

    // Spawn health logging task
    let health_handle = spawn_health_task(
        Arc::clone(&stats),
        config.health_log_interval,
        shutdown_tx.subscribe(),
    );
    info!("Health logging task started");

    // Wait for shutdown signal
    info!("All tasks running. Press Ctrl+C to stop.");

    // Handle shutdown signals
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;

        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM");
            }
            _ = sigint.recv() => {
                info!("Received SIGINT");
            }
        }
    }

    #[cfg(windows)]
    {
        tokio::signal::ctrl_c().await?;
        info!("Received Ctrl+C");
    }

    // Send shutdown signal to all tasks
    info!("Initiating graceful shutdown...");
    let _ = shutdown_tx.send(());

    // Wait for all tasks to complete with timeout
    let shutdown_timeout = Duration::from_secs(10);

    tokio::select! {
        _ = async {
            let _ = discovery_handle.await;
            let _ = binance_handle.await;
            let _ = clob_handle.await;
            let _ = health_handle.await;
        } => {
            info!("All tasks completed");
        }
        _ = tokio::time::sleep(shutdown_timeout) => {
            warn!("Shutdown timeout exceeded, forcing exit");
        }
    }

    // Final stats
    stats.log_stats();
    info!("Shutdown complete");

    Ok(())
}

/// Spawn the discovery task.
fn spawn_discovery_task(
    db: Arc<ClickHouseClient>,
    assets: Vec<CryptoAsset>,
    active_markets: ActiveMarkets,
    stats: Arc<HealthStats>,
    discovery_interval: Duration,
    mut shutdown: broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut discovery = MarketDiscovery::new(db, assets);

        info!(
            "Starting discovery loop with {} second interval",
            discovery_interval.as_secs()
        );

        loop {
            stats.discovery_runs.fetch_add(1, Ordering::Relaxed);

            match discovery.discover().await {
                Ok(count) => {
                    if count > 0 {
                        info!("Discovery found {} new markets", count);
                        stats
                            .markets_discovered
                            .fetch_add(count as u64, Ordering::Relaxed);

                        // Update active markets for CLOB capture
                        // Get discovered market windows from discovery's internal state
                        if let Ok(windows) = discovery.get_discovered_windows().await {
                            update_active_markets(&active_markets, &windows).await;
                        }
                    }
                }
                Err(e) => {
                    error!("Discovery error: {}", e);
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(discovery_interval) => {}
                _ = shutdown.recv() => {
                    info!("Discovery loop received shutdown signal");
                    break;
                }
            }
        }
    })
}

/// Spawn the Binance capture task.
fn spawn_binance_task(
    db: Arc<ClickHouseClient>,
    config: poly_collect::binance::BinanceConfig,
    shutdown: broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // BinanceCapture takes ClickHouseClient by value (it's Clone)
        let binance = BinanceCapture::new(config, (*db).clone());
        if let Err(e) = binance.run(shutdown).await {
            error!("Binance capture error: {}", e);
        }
    })
}

/// Spawn the CLOB capture task.
fn spawn_clob_task(
    db: Arc<ClickHouseClient>,
    config: poly_collect::clob::ClobConfig,
    active_markets: ActiveMarkets,
    shutdown: broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // ClobCapture takes ClickHouseClient by value (it's Clone)
        let clob = ClobCapture::new(config, (*db).clone(), active_markets);
        if let Err(e) = clob.run(shutdown).await {
            error!("CLOB capture error: {}", e);
        }
    })
}

/// Spawn the health logging task.
fn spawn_health_task(
    stats: Arc<HealthStats>,
    log_interval: Duration,
    mut shutdown: broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut timer = interval(log_interval);

        loop {
            tokio::select! {
                _ = timer.tick() => {
                    stats.log_stats();
                }
                _ = shutdown.recv() => {
                    info!("Health logger received shutdown signal");
                    break;
                }
            }
        }
    })
}
