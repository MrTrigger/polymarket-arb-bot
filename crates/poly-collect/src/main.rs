//! Poly-collect: Data collector for Polymarket arbitrage bot.
//!
//! Supports two modes:
//! - history: Fetch historical data from Binance/Polymarket APIs
//! - live: Stream real-time data via WebSocket
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
use chrono::Utc;
use clap::Parser;
use poly_common::{ClickHouseClient, CryptoAsset};
use tokio::sync::{broadcast, RwLock};
use tokio::time::interval;
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use poly_collect::binance::BinanceCapture;
use poly_collect::clob::{update_active_markets, ActiveMarkets, ClobCapture};
use poly_collect::config::{CollectConfig, CollectionMode};
use poly_collect::data_writer::DataWriter;
use poly_collect::discovery::MarketDiscovery;
use poly_collect::history::HistoricalCollector;

/// CLI arguments for poly-collect.
#[derive(Parser, Debug)]
#[command(name = "poly-collect")]
#[command(about = "Data collector for Polymarket arbitrage bot")]
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
    // Load .env file (if present)
    dotenvy::dotenv().ok();

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
    info!("Mode: {:?}", config.mode);
    info!("Assets: {:?}", config.assets);

    // Create ClickHouse client if needed
    let clickhouse = if config.output.use_clickhouse() {
        info!("ClickHouse URL: {}", config.clickhouse.url);
        let client = Arc::new(ClickHouseClient::new(config.clickhouse.clone()));

        // Test connection
        info!("Testing ClickHouse connection...");
        match client.ping().await {
            Ok(()) => {
                info!("ClickHouse connection successful");
                if let Err(e) = client.create_tables().await {
                    warn!("Failed to create tables: {}", e);
                }
            }
            Err(e) => {
                warn!("ClickHouse not available: {}. Continuing anyway.", e);
            }
        }
        Some(client)
    } else {
        info!("ClickHouse output disabled");
        None
    };

    // Run the appropriate mode
    match config.mode {
        CollectionMode::History => run_history_mode(&config, clickhouse).await,
        CollectionMode::Live => {
            // Create DataWriter for live mode (no collection params)
            let writer = Arc::new(DataWriter::new(&config.output, clickhouse.clone())?);
            if let Some(dir) = writer.csv_dir() {
                info!("CSV output directory: {:?}", dir);
            }
            run_live_mode(&config, writer, clickhouse).await
        }
    }
}

/// Run historical data collection mode.
async fn run_history_mode(
    config: &CollectConfig,
    clickhouse: Option<Arc<ClickHouseClient>>,
) -> Result<()> {
    use poly_collect::data_writer::CollectionParams;

    // Determine date range from start_date/end_date or days
    let (start_date, end_date) = if let (Some(start), Some(end)) =
        (config.start_date, config.end_date)
    {
        (start, end)
    } else if let Some(days) = config.duration.days {
        // Use last N days
        let end = chrono::Utc::now().date_naive();
        let start = end - chrono::Duration::days(days as i64);
        (start, end)
    } else {
        anyhow::bail!("Either start_date/end_date or days must be specified for history mode");
    };

    info!(
        "Running historical collection from {} to {}",
        start_date, end_date
    );

    // Create DataWriter with collection params for organized CSV output
    // Use configured timeframes for folder naming (e.g., "15m" or "5m,15m,1h")
    let interval = config
        .timeframes
        .iter()
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let params = CollectionParams {
        start_date,
        end_date,
        interval,
    };
    let writer = Arc::new(DataWriter::with_params(
        &config.output,
        clickhouse,
        Some(params),
    )?);

    if let Some(dir) = writer.csv_dir() {
        info!("CSV output directory: {:?}", dir);
    }

    let collector = HistoricalCollector::new(writer, config.timeframes.clone())?;
    let stats = collector
        .collect(&config.assets, start_date, end_date)
        .await?;

    info!("Historical collection complete:");
    info!("  Assets completed: {}", stats.assets_completed);
    info!("  Assets failed: {}", stats.assets_failed);
    info!("  Binance records: {}", stats.binance_records);
    info!("  Polymarket records: {}", stats.polymarket_records);

    Ok(())
}

/// Run live streaming mode.
async fn run_live_mode(
    config: &CollectConfig,
    writer: Arc<DataWriter>,
    clickhouse: Option<Arc<ClickHouseClient>>,
) -> Result<()> {
    // Log collection duration
    if config.duration.is_indefinite() {
        info!("Collection duration: indefinite (press Ctrl+C to stop)");
    } else if let Some(stop_time) = config.duration.stop_time() {
        info!("Collection will stop at: {}", stop_time);
    }

    // Shared state for active markets (discovery writes, CLOB reads)
    let active_markets: ActiveMarkets = Arc::new(RwLock::new(HashMap::new()));

    // Create shutdown channel (capacity for all subscribers)
    let (shutdown_tx, _) = broadcast::channel::<()>(16);

    // Health statistics
    let stats = Arc::new(HealthStats::default());

    // Spawn discovery task (needs ClickHouse for market window storage)
    let discovery_handle = if let Some(db) = clickhouse.clone() {
        Some(spawn_discovery_task(
            db,
            Arc::clone(&writer),
            config.assets.clone(),
            Arc::clone(&active_markets),
            Arc::clone(&stats),
            config.discovery_interval,
            shutdown_tx.subscribe(),
        ))
    } else {
        warn!("Discovery disabled (requires ClickHouse)");
        None
    };

    // Spawn Binance capture task
    let binance_handle = spawn_binance_task(
        Arc::clone(&writer),
        config.binance.clone(),
        shutdown_tx.subscribe(),
    );
    info!("Binance capture task started");

    // Spawn CLOB capture task
    let clob_handle = spawn_clob_task(
        Arc::clone(&writer),
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

    // Calculate duration until stop time (if set)
    let duration_until_stop = config.duration.stop_time().map(|stop_time| {
        let now = Utc::now();
        if stop_time > now {
            (stop_time - now).to_std().unwrap_or(Duration::from_secs(0))
        } else {
            Duration::from_secs(0)
        }
    });

    // Handle shutdown signals (Ctrl+C or duration expiry)
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
            _ = async {
                if let Some(duration) = duration_until_stop {
                    tokio::time::sleep(duration).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                info!("Collection duration reached, stopping...");
            }
        }
    }

    #[cfg(windows)]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C");
            }
            _ = async {
                if let Some(duration) = duration_until_stop {
                    tokio::time::sleep(duration).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                info!("Collection duration reached, stopping...");
            }
        }
    }

    // Send shutdown signal to all tasks
    info!("Initiating graceful shutdown...");
    let _ = shutdown_tx.send(());

    // Wait for all tasks to complete with timeout
    let shutdown_timeout = Duration::from_secs(10);

    tokio::select! {
        _ = async {
            if let Some(h) = discovery_handle { let _ = h.await; }
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
    writer: Arc<DataWriter>,
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

                        if let Ok(windows) = discovery.get_discovered_windows().await {
                            update_active_markets(&active_markets, &windows).await;
                            // Only write currently active markets (we have data for them)
                            let now = Utc::now();
                            let active_windows: Vec<_> = windows
                                .into_iter()
                                .filter(|w| w.window_start <= now && w.window_end > now)
                                .collect();
                            if !active_windows.is_empty() {
                                if let Err(e) = writer.write_market_windows(&active_windows).await {
                                    error!("Failed to write market windows to CSV: {}", e);
                                }
                            }
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
    writer: Arc<DataWriter>,
    config: poly_collect::binance::BinanceConfig,
    shutdown: broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let binance = BinanceCapture::new(config, writer);
        if let Err(e) = binance.run(shutdown).await {
            error!("Binance capture error: {}", e);
        }
    })
}

/// Spawn the CLOB capture task.
fn spawn_clob_task(
    writer: Arc<DataWriter>,
    config: poly_collect::clob::ClobConfig,
    active_markets: ActiveMarkets,
    shutdown: broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let clob = ClobCapture::new(config, writer, active_markets);
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
