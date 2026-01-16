//! Poly-bot: Polymarket 15-minute arbitrage trading bot.
//!
//! Usage:
//!   poly-bot [OPTIONS]
//!
//! Options:
//!   -m, --mode <MODE>           Trading mode: live, paper, shadow, backtest
//!   -c, --config <FILE>         Config file path (default: config/bot.toml)
//!   --clickhouse-url <URL>      ClickHouse HTTP URL (overrides config)
//!   --assets <ASSETS>           Comma-separated assets (overrides config)
//!   --start <DATE>              Backtest start date (YYYY-MM-DD)
//!   --end <DATE>                Backtest end date (YYYY-MM-DD)
//!   --speed <SPEED>             Backtest speed (0 = max, 1.0 = real-time)
//!   --sweep                     Enable parameter sweep mode for backtest

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use poly_common::{ClickHouseClient, WindowDuration};
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use poly_bot::config::{BotConfig, DashboardConfig, SweepConfig, TradingMode};
use poly_bot::dashboard::{
    create_shared_dashboard_state_manager, spawn_api_server, spawn_websocket_server,
    ApiServerConfig, WebSocketServerConfig,
};
use poly_bot::mode::{
    BacktestMode, BacktestModeConfig, LiveMode, LiveModeConfig, PaperMode, PaperModeConfig,
    ShadowMode, ShadowModeConfig,
};

/// CLI arguments for poly-bot.
#[derive(Parser, Debug)]
#[command(name = "poly-bot")]
#[command(about = "Polymarket 15-minute arbitrage trading bot")]
#[command(version)]
struct Args {
    /// Trading mode: live, paper, shadow, backtest
    #[arg(short, long)]
    mode: Option<String>,

    /// Config file path
    #[arg(short, long, default_value = "config/bot.toml")]
    config: PathBuf,

    /// ClickHouse HTTP URL (overrides config file)
    #[arg(long)]
    clickhouse_url: Option<String>,

    /// Comma-separated assets to trade (e.g., "BTC,ETH,SOL")
    #[arg(long, value_delimiter = ',')]
    assets: Option<Vec<String>>,

    /// Backtest start date (YYYY-MM-DD)
    #[arg(long)]
    start: Option<String>,

    /// Backtest end date (YYYY-MM-DD)
    #[arg(long)]
    end: Option<String>,

    /// Backtest speed multiplier (0 = max speed, 1.0 = real-time)
    #[arg(long)]
    speed: Option<f64>,

    /// Enable parameter sweep mode for backtest
    #[arg(long)]
    sweep: bool,

    /// Market window duration: 15min or 1h (overrides config file)
    #[arg(long, short = 'w')]
    window: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {:#}", e);
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    // Load environment variables from .env file (if present)
    if let Err(e) = dotenvy::dotenv() {
        // Only warn if it's not a "file not found" error
        if !matches!(e, dotenvy::Error::Io(ref io_err) if io_err.kind() == std::io::ErrorKind::NotFound) {
            eprintln!("Warning: Failed to load .env file: {}", e);
        }
    }

    // Parse CLI arguments
    let args = Args::parse();

    // Load configuration
    let mut config = if args.config.exists() {
        BotConfig::from_file(&args.config)
            .with_context(|| format!("Failed to load config from {:?}", args.config))?
    } else {
        warn!(
            "Config file not found at {:?}, using defaults",
            args.config
        );
        BotConfig::default()
    };

    // Apply environment variable overrides (credentials, etc.)
    config.apply_env_overrides();

    // Apply CLI overrides
    config.apply_cli_overrides(args.mode.clone(), args.assets, args.clickhouse_url);

    // Apply backtest-specific CLI overrides
    if let Some(start) = args.start {
        config.backtest.start_date = Some(start);
    }
    if let Some(end) = args.end {
        config.backtest.end_date = Some(end);
    }
    if let Some(speed) = args.speed {
        config.backtest.speed = speed;
    }
    if args.sweep {
        config.backtest.sweep_enabled = true;
    }

    // Apply window duration override from CLI
    if let Some(window_str) = &args.window {
        if let Ok(window_duration) = window_str.parse::<WindowDuration>() {
            config.window_duration = window_duration;
        }
    }

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
    tracing::subscriber::set_global_default(subscriber)
        .context("Failed to set global tracing subscriber")?;

    info!("Starting poly-bot trading bot");
    info!("Mode: {}", config.mode);
    info!("Window duration: {}", config.window_duration);
    info!("Assets: {:?}", config.assets);

    // Validate configuration before proceeding
    config.validate().context("Configuration validation failed")?;

    // Create ClickHouse client
    let clickhouse = ClickHouseClient::new(config.clickhouse.clone());

    // Test connection
    info!("Testing ClickHouse connection...");
    match clickhouse.ping().await {
        Ok(()) => {
            info!("ClickHouse connection successful");
            // Create tables if needed
            if let Err(e) = clickhouse.create_tables().await {
                warn!("Failed to create tables: {}", e);
            }
        }
        Err(e) => {
            if config.mode != TradingMode::Shadow {
                // Shadow mode can run without DB
                warn!(
                    "ClickHouse not available: {}. Some features may not work.",
                    e
                );
            }
        }
    }

    // Create shared GlobalState for dashboard and mode to share
    // This enables the dashboard to display real-time data from the trading strategy
    let shared_state = Arc::new(poly_bot::state::GlobalState::new());

    // Start dashboard servers if enabled (except in backtest mode)
    let dashboard_handles = if config.dashboard.enabled && config.mode != TradingMode::Backtest {
        Some(start_dashboard_servers(&config.dashboard, clickhouse.clone(), shared_state.clone()).await?)
    } else {
        None
    };

    // Run the selected mode
    let result = match config.mode {
        TradingMode::Live => run_live_mode(config, clickhouse, shared_state).await,
        TradingMode::Paper => run_paper_mode(config, clickhouse, shared_state).await,
        TradingMode::Shadow => run_shadow_mode(config, clickhouse, shared_state).await,
        TradingMode::Backtest => run_backtest_mode(config, clickhouse).await,
    };

    // Shutdown dashboard servers
    if let Some((ws_server, ws_handle, api_handle)) = dashboard_handles {
        info!("Shutting down dashboard servers...");
        let _ = ws_server.shutdown_handle().send(());
        ws_handle.abort();
        api_handle.abort();
    }

    result
}

/// Start the dashboard WebSocket and REST API servers.
///
/// Returns handles to both servers for graceful shutdown.
async fn start_dashboard_servers(
    config: &DashboardConfig,
    clickhouse: ClickHouseClient,
    global_state: Arc<poly_bot::state::GlobalState>,
) -> Result<(
    poly_bot::dashboard::SharedWebSocketServer,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
)> {
    info!(
        ws_port = config.websocket_port,
        api_port = config.api_port,
        broadcast_interval_ms = config.broadcast_interval_ms,
        "Starting dashboard servers"
    );

    // Create shared state manager for WebSocket broadcasts
    let state_manager = create_shared_dashboard_state_manager();

    // Configure and start WebSocket server
    let ws_config = WebSocketServerConfig::from_dashboard_config(config);
    let (ws_server, ws_handle) =
        spawn_websocket_server(ws_config, state_manager, global_state);

    // Configure and start REST API server
    let api_config = ApiServerConfig::from_dashboard_config(config);
    let api_handle = spawn_api_server(api_config, clickhouse);

    Ok((ws_server, ws_handle, api_handle))
}

/// Run live trading mode.
async fn run_live_mode(
    config: BotConfig,
    clickhouse: ClickHouseClient,
    _shared_state: Arc<poly_bot::state::GlobalState>,
) -> Result<()> {
    info!("Initializing live trading mode");

    // Create mode config
    let mode_config = LiveModeConfig::from_bot_config(&config);

    // Create live mode runner
    // TODO: Wire shared_state to LiveMode once it supports with_state
    let mut mode = LiveMode::new(mode_config, config)
        .context("Failed to create live mode")?
        .with_clickhouse(clickhouse);

    // Set up shutdown handler
    let state = mode.state();
    let shutdown_state = state.clone();

    // Handle shutdown signals
    tokio::spawn(async move {
        if let Err(e) = wait_for_shutdown().await {
            error!("Shutdown signal handler error: {}", e);
        }
        info!("Requesting shutdown...");
        shutdown_state.control.request_shutdown();
    });

    // Run the mode
    mode.run().await
}

/// Run paper trading mode.
async fn run_paper_mode(
    config: BotConfig,
    clickhouse: ClickHouseClient,
    shared_state: Arc<poly_bot::state::GlobalState>,
) -> Result<()> {
    info!("Initializing paper trading mode");

    // Create mode config
    let mode_config = PaperModeConfig::from_bot_config(&config);

    // Create paper mode runner with shared state (for dashboard integration)
    let mut mode = PaperMode::new(mode_config, &config)
        .context("Failed to create paper mode")?
        .with_state(shared_state)
        .with_clickhouse(clickhouse);

    // Set up shutdown handler
    let state = mode.state();
    let shutdown_state = state.clone();

    tokio::spawn(async move {
        if let Err(e) = wait_for_shutdown().await {
            error!("Shutdown signal handler error: {}", e);
        }
        info!("Requesting shutdown...");
        shutdown_state.control.request_shutdown();
    });

    // Run the mode
    mode.run().await
}

/// Run shadow mode.
async fn run_shadow_mode(
    config: BotConfig,
    clickhouse: ClickHouseClient,
    _shared_state: Arc<poly_bot::state::GlobalState>,
) -> Result<()> {
    info!("Initializing shadow mode");

    // Create mode config
    let mode_config = ShadowModeConfig::from_bot_config(&config);

    // Create shadow mode runner
    // TODO: Wire shared_state to ShadowMode once it supports with_state
    let mut mode = ShadowMode::new(mode_config, &config)
        .context("Failed to create shadow mode")?
        .with_clickhouse(clickhouse);

    // Set up shutdown handler
    let state = mode.state();
    let shutdown_state = state.clone();

    tokio::spawn(async move {
        if let Err(e) = wait_for_shutdown().await {
            error!("Shutdown signal handler error: {}", e);
        }
        info!("Requesting shutdown...");
        shutdown_state.control.request_shutdown();
    });

    // Run the mode
    mode.run().await
}

/// Run backtest mode.
async fn run_backtest_mode(config: BotConfig, clickhouse: ClickHouseClient) -> Result<()> {
    info!("Initializing backtest mode");

    // Log backtest parameters
    info!(
        "Backtest period: {} to {}",
        config.backtest.start_date.as_deref().unwrap_or("default"),
        config.backtest.end_date.as_deref().unwrap_or("default")
    );
    info!("Backtest speed: {}", config.backtest.speed);
    if config.backtest.sweep_enabled {
        info!("Parameter sweep enabled");
    }

    // Create mode config
    let mut mode_config = BacktestModeConfig::from_bot_config(&config, &clickhouse)
        .context("Failed to create backtest config")?;

    // Load sweep params from strategy.toml if sweep is enabled
    if config.backtest.sweep_enabled {
        let strategy_path = std::path::Path::new("config/strategy.toml");
        match SweepConfig::from_file(strategy_path) {
            Ok(sweep_config) => {
                let sweep_params = sweep_config.to_sweep_parameters();
                if sweep_params.is_empty() {
                    warn!("Sweep enabled but no sweep parameters found in {}", strategy_path.display());
                } else {
                    info!("Loaded {} sweep parameters from {}", sweep_params.len(), strategy_path.display());
                    for param in &sweep_params {
                        info!("  {} [{} to {} step {}]", param.name, param.start, param.end, param.step);
                    }
                    mode_config.sweep_params = sweep_params;
                }
            }
            Err(e) => {
                warn!("Failed to load sweep config from {}: {}", strategy_path.display(), e);
                warn!("Sweep will run with default parameters");
            }
        }
    }

    // Create backtest mode runner
    let mut mode = BacktestMode::new(mode_config, &config, clickhouse)
        .context("Failed to create backtest mode")?;

    // Set up shutdown handler
    let state = mode.state();
    let shutdown_state = state.clone();

    tokio::spawn(async move {
        if let Err(e) = wait_for_shutdown().await {
            error!("Shutdown signal handler error: {}", e);
        }
        info!("Requesting shutdown...");
        shutdown_state.control.request_shutdown();
    });

    // Run the backtest
    let result = mode.run().await?;

    // Log summary
    info!("Backtest completed");
    info!("Total P&L: ${:.2}", result.total_pnl);
    info!("Return: {:.2}%", result.return_pct);
    info!("Trades executed: {}", result.trades_executed);
    info!("Volume traded: ${:.2}", result.volume_traded);

    // If sweep was enabled, log sweep results sorted by P&L
    let sweep_results = mode.sweep_results();
    if !sweep_results.is_empty() {
        // Sort by P&L descending
        let mut sorted: Vec<_> = sweep_results.iter().enumerate().collect();
        sorted.sort_by(|a, b| b.1.total_pnl.cmp(&a.1.total_pnl));

        info!("");
        info!("═══════════════════════════════════════════════════════════════════════════");
        info!("                      SWEEP RESULTS ({} runs, sorted by P&L)", sweep_results.len());
        info!("═══════════════════════════════════════════════════════════════════════════");
        info!("");
        info!("{:>4}  {:>10}  {:>8}  {}", "Rank", "P&L", "Return", "Parameters");
        info!("{:>4}  {:>10}  {:>8}  {}", "----", "----------", "--------", "----------");

        for (rank, (_orig_idx, res)) in sorted.iter().enumerate() {
            // Format parameters more compactly
            let params: Vec<String> = res.parameters.iter()
                .map(|(k, v)| format!("{}={:.2}", k, v))
                .collect();
            let params_str = params.join(", ");

            info!(
                "{:>4}  ${:>9.2}  {:>7.2}%  {}",
                rank + 1,
                res.total_pnl,
                res.return_pct,
                params_str
            );
        }

        info!("");
        info!("═══════════════════════════════════════════════════════════════════════════");

        // Show best run prominently
        if let Some((_, best)) = sorted.first() {
            let params: Vec<String> = best.parameters.iter()
                .map(|(k, v)| format!("{}={:.2}", k, v))
                .collect();
            info!("BEST: P&L=${:.2} ({:.2}%)", best.total_pnl, best.return_pct);
            info!("      {}", params.join(", "));
        }

        // Show worst run for comparison
        if let Some((_, worst)) = sorted.last() {
            let params: Vec<String> = worst.parameters.iter()
                .map(|(k, v)| format!("{}={:.2}", k, v))
                .collect();
            info!("WORST: P&L=${:.2} ({:.2}%)", worst.total_pnl, worst.return_pct);
            info!("       {}", params.join(", "));
        }

        info!("═══════════════════════════════════════════════════════════════════════════");
    }

    Ok(())
}

/// Wait for shutdown signal (Ctrl+C or SIGTERM).
async fn wait_for_shutdown() -> Result<()> {
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parsing() {
        // Test default config path
        let args = Args::try_parse_from(["poly-bot"]).unwrap();
        assert_eq!(args.config.to_str().unwrap(), "config/bot.toml");
        assert!(args.mode.is_none());
    }

    #[test]
    fn test_cli_mode_override() {
        let args = Args::try_parse_from(["poly-bot", "--mode", "paper"]).unwrap();
        assert_eq!(args.mode, Some("paper".to_string()));
    }

    #[test]
    fn test_cli_backtest_options() {
        let args = Args::try_parse_from([
            "poly-bot",
            "--mode",
            "backtest",
            "--start",
            "2025-01-01",
            "--end",
            "2025-01-31",
            "--speed",
            "0.5",
            "--sweep",
        ])
        .unwrap();

        assert_eq!(args.mode, Some("backtest".to_string()));
        assert_eq!(args.start, Some("2025-01-01".to_string()));
        assert_eq!(args.end, Some("2025-01-31".to_string()));
        assert_eq!(args.speed, Some(0.5));
        assert!(args.sweep);
    }

    #[test]
    fn test_cli_assets_override() {
        let args = Args::try_parse_from(["poly-bot", "--assets", "BTC,XRP"]).unwrap();
        assert_eq!(args.assets, Some(vec!["BTC".to_string(), "XRP".to_string()]));
    }

    #[test]
    fn test_cli_clickhouse_override() {
        let args =
            Args::try_parse_from(["poly-bot", "--clickhouse-url", "http://db:8123"]).unwrap();
        assert_eq!(
            args.clickhouse_url,
            Some("http://db:8123".to_string())
        );
    }

    #[test]
    fn test_cli_config_path() {
        let args = Args::try_parse_from(["poly-bot", "-c", "/custom/bot.toml"]).unwrap();
        assert_eq!(args.config.to_str().unwrap(), "/custom/bot.toml");
    }

    #[test]
    fn test_cli_combined_options() {
        let args = Args::try_parse_from([
            "poly-bot",
            "-m",
            "live",
            "-c",
            "/etc/bot.toml",
            "--assets",
            "BTC,ETH",
            "--clickhouse-url",
            "http://prod:8123",
        ])
        .unwrap();

        assert_eq!(args.mode, Some("live".to_string()));
        assert_eq!(args.config.to_str().unwrap(), "/etc/bot.toml");
        assert_eq!(args.assets, Some(vec!["BTC".to_string(), "ETH".to_string()]));
        assert_eq!(args.clickhouse_url, Some("http://prod:8123".to_string()));
    }
}
