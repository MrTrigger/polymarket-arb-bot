//! Paper trading mode.
//!
//! Implements paper trading with real market data but simulated execution.
//! Uses live WebSocket feeds for price discovery combined with a simulated
//! executor that tracks virtual positions and P&L.
//!
//! ## Components
//!
//! - `LiveDataSource`: Real-time WebSocket data from Binance and Polymarket
//! - `PaperExecutor`: Simulated order execution with configurable latency
//! - Full observability enabled (decisions, counterfactuals, anomalies)
//!
//! ## Use Cases
//!
//! - Validate strategy logic with real market conditions
//! - Test risk management without risking capital
//! - Debug order flow and timing before going live
//! - Benchmark expected P&L against real opportunities

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use rust_decimal::Decimal;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use poly_common::{ClickHouseClient, CryptoAsset};
use poly_market::DiscoveryConfig;

use crate::config::{BotConfig, EnginesConfig, ObservabilityConfig, TradingMode};
use crate::dashboard::{
    create_shared_session_manager, end_shared_session, BotMode, DashboardIntegration, ExitReason,
    SharedSessionManager,
};
use crate::data_source::live::{LiveDataSource, LiveDataSourceConfig};
use crate::executor::simulated::{SimulatedExecutor, SimulatedExecutorConfig};
use crate::state::GlobalState;
use crate::strategy::{StrategyConfig, StrategyLoop};

use super::common;

/// Configuration for paper trading mode.
#[derive(Debug, Clone)]
pub struct PaperModeConfig {
    /// Data source configuration.
    pub data_source: LiveDataSourceConfig,
    /// Executor configuration.
    pub executor: SimulatedExecutorConfig,
    /// Strategy configuration.
    pub strategy: StrategyConfig,
    /// Observability configuration.
    pub observability: ObservabilityConfig,
    /// Engines configuration (arbitrage, directional, maker).
    pub engines: EnginesConfig,
    /// Market discovery configuration.
    pub discovery: DiscoveryConfig,
    /// Dashboard configuration.
    pub dashboard: crate::config::DashboardConfig,
    /// Initial virtual balance (USDC).
    pub initial_balance: Decimal,
    /// Graceful shutdown timeout (seconds).
    pub shutdown_timeout_secs: u64,
}

impl Default for PaperModeConfig {
    fn default() -> Self {
        Self {
            data_source: LiveDataSourceConfig::default(),
            executor: SimulatedExecutorConfig::paper(),
            strategy: StrategyConfig::default(),
            observability: ObservabilityConfig::default(),
            engines: EnginesConfig::default(),
            discovery: DiscoveryConfig::default(),
            dashboard: crate::config::DashboardConfig::default(),
            initial_balance: Decimal::new(10000, 0), // $10,000 default for paper
            shutdown_timeout_secs: 30,
        }
    }
}

impl PaperModeConfig {
    /// Create config from BotConfig.
    pub fn from_bot_config(config: &BotConfig) -> Self {
        // Use allocated balance from config (trading.sizing.available_balance)
        let allocated_balance = config.trading.sizing.available_balance;

        let mut executor_config = SimulatedExecutorConfig::paper();
        executor_config.initial_balance = allocated_balance;
        executor_config.latency_ms = config.execution.paper_fill_latency_ms;
        executor_config.fee_rate = Decimal::ZERO; // Polymarket has 0% maker fees
        executor_config.enforce_balance = true;
        executor_config.max_position_per_market = config.trading.max_position_per_market;

        // Convert string assets to CryptoAsset enum for discovery
        let discovery_assets: Vec<CryptoAsset> = config
            .assets
            .iter()
            .filter_map(|s| match s.to_uppercase().as_str() {
                "BTC" | "BITCOIN" => Some(CryptoAsset::Btc),
                "ETH" | "ETHEREUM" => Some(CryptoAsset::Eth),
                "SOL" | "SOLANA" => Some(CryptoAsset::Sol),
                "XRP" | "RIPPLE" => Some(CryptoAsset::Xrp),
                _ => None,
            })
            .collect();

        let discovery_config = DiscoveryConfig {
            assets: discovery_assets,
            window_duration: config.window_duration,
            ..Default::default()
        };

        Self {
            data_source: LiveDataSourceConfig::default(),
            executor: executor_config,
            strategy: StrategyConfig::from_trading_config(&config.trading, (config.window_duration.minutes() * 60) as i64),
            observability: config.observability.clone(),
            engines: config.engines.clone(),
            discovery: discovery_config,
            dashboard: config.dashboard.clone(),
            initial_balance: allocated_balance,
            shutdown_timeout_secs: 30,
        }
    }
}

/// Paper trading mode runner.
///
/// Coordinates all components for paper trading:
/// - Data sources (Binance, Polymarket WebSockets) - real market data
/// - Executor (simulated fills with configurable latency)
/// - Strategy loop (arb detection, sizing, execution)
/// - Observability (decisions, counterfactuals, anomalies)
/// - Session tracking (dashboard events)
/// - Dashboard integration (P&L snapshots, trade capture)
pub struct PaperMode {
    /// Configuration.
    config: PaperModeConfig,
    /// Global shared state.
    state: Arc<GlobalState>,
    /// Shutdown signal sender.
    shutdown_tx: broadcast::Sender<()>,
    /// ClickHouse client for data storage.
    clickhouse: Option<ClickHouseClient>,
    /// Session manager for dashboard tracking.
    session: SharedSessionManager,
    /// Dashboard integration for P&L capture.
    dashboard: Option<DashboardIntegration>,
}

impl PaperMode {
    /// Create a new paper mode runner.
    ///
    /// # Arguments
    ///
    /// * `config` - Paper mode configuration
    /// * `bot_config` - Full bot configuration (for validation)
    ///
    /// # Errors
    ///
    /// Returns error if configuration validation fails.
    pub fn new(config: PaperModeConfig, bot_config: &BotConfig) -> Result<Self> {
        // Validate mode
        if bot_config.mode != TradingMode::Paper {
            anyhow::bail!("PaperMode requires TradingMode::Paper");
        }

        let state = Arc::new(GlobalState::new());
        let (shutdown_tx, _) = broadcast::channel(16);

        // Create session manager (capture will be set up later in setup_observability)
        let session = create_shared_session_manager(BotMode::Paper, bot_config, None);

        Ok(Self {
            config,
            state,
            shutdown_tx,
            clickhouse: None,
            session,
            dashboard: None,
        })
    }

    /// Set the ClickHouse client for data storage.
    pub fn with_clickhouse(mut self, client: ClickHouseClient) -> Self {
        self.clickhouse = Some(client);
        self
    }

    /// Use an existing GlobalState (for sharing with dashboard).
    pub fn with_state(mut self, state: Arc<GlobalState>) -> Self {
        tracing::info!("PaperMode using shared GlobalState at {:p}", Arc::as_ptr(&state));
        self.state = state;
        self
    }

    /// Get a shutdown signal receiver.
    pub fn shutdown_signal(&self) -> broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    /// Get the global state.
    pub fn state(&self) -> Arc<GlobalState> {
        self.state.clone()
    }

    /// Run paper trading mode.
    ///
    /// This method runs until shutdown is requested or a fatal error occurs.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on clean shutdown, `Err` on fatal error.
    pub async fn run(&mut self) -> Result<()> {
        info!("Starting paper trading mode");

        // Start session tracking
        if let Ok(session) = self.session.read() {
            session.start();
        }

        info!(
            initial_balance = %self.config.initial_balance,
            latency_ms = self.config.executor.latency_ms,
            "Paper executor configuration"
        );

        // Log enabled engines
        let enabled_engines = self.config.engines.enabled_engines();
        info!(
            engines = ?enabled_engines,
            arbitrage = self.config.engines.arbitrage.enabled,
            directional = self.config.engines.directional.enabled,
            maker = self.config.engines.maker.enabled,
            "Trading engines configuration"
        );

        // Create data source (real market data)
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let data_source = LiveDataSource::new(self.config.data_source.clone());

        // Get handles before moving data_source
        let active_markets = data_source.active_markets_handle();
        let event_sender = data_source.event_sender();

        // Spawn market discovery task (shared with live mode)
        let discovery_shutdown = self.shutdown_tx.subscribe();
        let discovery_config = self.config.discovery.clone();
        let _discovery_handle = tokio::spawn(common::run_market_discovery(
            discovery_config,
            active_markets,
            event_sender,
            discovery_shutdown,
        ));

        // Create paper executor (simulated fills)
        let mut executor_config = self.config.executor.clone();
        executor_config.initial_balance = self.config.initial_balance;
        let executor = SimulatedExecutor::new(executor_config);

        // Set balance info in global state for dashboard
        self.state.metrics.set_allocated_balance(self.config.initial_balance);
        self.state.metrics.set_current_balance(self.config.initial_balance);

        // Set up observability (shared with live mode)
        let (capture, obs_tasks) = common::setup_observability(
            &self.config.observability,
            self.clickhouse.as_ref(),
            &self.shutdown_tx,
        ).await?;

        // Set up dashboard integration
        let session_id = self.session.read().map(|s| s.session_id()).unwrap_or_default();
        let mut dashboard_tasks = Vec::new();
        self.dashboard = DashboardIntegration::new(
            &self.config.dashboard,
            self.clickhouse.as_ref(),
            &self.shutdown_tx,
            session_id,
        )
        .await;

        // Start P&L snapshot timer if dashboard is enabled
        if let Some(ref dashboard) = self.dashboard {
            let pnl_timer = dashboard.start_pnl_timer(
                self.state.clone(),
                self.shutdown_tx.subscribe(),
            );
            dashboard_tasks.push(pnl_timer);
        }

        // Create strategy loop with engines config
        let mut strategy = StrategyLoop::with_engines(
            data_source,
            executor,
            self.state.clone(),
            self.config.strategy.clone(),
            self.config.engines.clone(),
        );

        // Warm up ATR tracker with recent historical prices (shared with live mode)
        common::warmup_atr(&mut strategy, &self.config.discovery.assets).await;

        // Add observability sender if capture is enabled (shared with live mode)
        if let Some(ref cap) = capture
            && cap.is_enabled()
        {
            strategy = common::setup_observability_forwarding(strategy, cap);
        }

        // Enable trading
        self.state.enable_trading();
        info!("Paper trading enabled");

        // Run strategy loop
        let result = tokio::select! {
            result = strategy.run() => {
                match result {
                    Ok(()) => {
                        info!("Strategy loop completed");
                        Ok(())
                    }
                    Err(crate::strategy::StrategyError::Shutdown) => {
                        info!("Strategy loop shutdown requested");
                        Ok(())
                    }
                    Err(e) => {
                        error!("Strategy loop error: {}", e);
                        Err(anyhow::anyhow!("Strategy error: {}", e))
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                info!("Shutdown signal received");
                Ok(())
            }
        };

        // Graceful shutdown
        self.shutdown(&mut strategy, obs_tasks, dashboard_tasks).await;

        result
    }

    /// Perform graceful shutdown.
    async fn shutdown<D, E>(
        &self,
        strategy: &mut StrategyLoop<D, E>,
        obs_tasks: Vec<tokio::task::JoinHandle<()>>,
        dashboard_tasks: Vec<tokio::task::JoinHandle<()>>,
    ) where
        D: crate::data_source::DataSource,
        E: crate::executor::Executor,
    {
        info!("Beginning graceful shutdown");

        // Disable trading
        self.state.disable_trading();

        // Signal shutdown
        let _ = self.shutdown_tx.send(());

        // Shutdown strategy (which shuts down data source and executor)
        strategy.shutdown().await;

        // Wait for observability and dashboard tasks with timeout
        let timeout = Duration::from_secs(self.config.shutdown_timeout_secs);
        for handle in obs_tasks {
            match tokio::time::timeout(timeout, handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Observability task panicked: {}", e),
                Err(_) => warn!("Observability task timed out during shutdown"),
            }
        }
        for handle in dashboard_tasks {
            match tokio::time::timeout(timeout, handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Dashboard task panicked: {}", e),
                Err(_) => warn!("Dashboard task timed out during shutdown"),
            }
        }

        // Log final metrics
        let metrics = self.state.metrics.snapshot();
        info!(
            events = metrics.events_processed,
            opportunities = metrics.opportunities_detected,
            trades = metrics.trades_executed,
            skipped = metrics.trades_skipped,
            pnl = %metrics.pnl_usdc,
            volume = %metrics.volume_usdc,
            "Paper trading final metrics"
        );

        // End session with graceful exit
        end_shared_session(&self.session, ExitReason::Graceful, &metrics);

        info!("Paper trading shutdown complete");
    }

    /// Request shutdown.
    pub fn request_shutdown(&self) {
        info!("Paper trading shutdown requested");
        self.state.control.request_shutdown();
        let _ = self.shutdown_tx.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_paper_mode_config_default() {
        let config = PaperModeConfig::default();
        assert_eq!(config.initial_balance, dec!(10000));
        assert_eq!(config.shutdown_timeout_secs, 30);
        assert_eq!(config.executor.latency_ms, 50);
    }

    #[test]
    fn test_paper_mode_config_from_bot_config() {
        let mut bot_config = BotConfig::default();
        bot_config.execution.paper_fill_latency_ms = 100;

        let config = PaperModeConfig::from_bot_config(&bot_config);
        assert_eq!(config.executor.latency_ms, 100);
        assert_eq!(config.strategy.max_consecutive_failures, 3);
    }

    #[test]
    fn test_paper_mode_requires_paper_trading_mode() {
        let config = PaperModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Live; // Wrong mode

        let result = PaperMode::new(config, &bot_config);
        assert!(result.is_err());
    }

    #[test]
    fn test_paper_mode_accepts_paper_mode() {
        let config = PaperModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Paper;

        let result = PaperMode::new(config, &bot_config);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_paper_mode_shutdown_signal() {
        let config = PaperModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Paper;

        let mode = PaperMode::new(config, &bot_config).unwrap();
        let mut rx = mode.shutdown_signal();

        // Request shutdown
        mode.request_shutdown();

        // Should receive signal
        let result = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;

        assert!(result.is_ok());
    }

    #[test]
    fn test_paper_mode_state_access() {
        let config = PaperModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Paper;

        let mode = PaperMode::new(config, &bot_config).unwrap();
        let state = mode.state();

        // State should not allow trading initially
        assert!(!state.can_trade());
    }

    #[test]
    fn test_paper_mode_executor_config() {
        let mut bot_config = BotConfig::default();
        bot_config.execution.paper_fill_latency_ms = 25;
        bot_config.trading.max_position_per_market = dec!(500);

        let config = PaperModeConfig::from_bot_config(&bot_config);

        assert_eq!(config.executor.latency_ms, 25);
        assert_eq!(config.executor.max_position_per_market, dec!(500));
        assert_eq!(config.executor.fee_rate, Decimal::ZERO);
        assert!(config.executor.enforce_balance);
    }

    #[test]
    fn test_paper_mode_engines_config() {
        let mut bot_config = BotConfig::default();
        bot_config.engines.arbitrage.enabled = true;
        bot_config.engines.directional.enabled = true;
        bot_config.engines.maker.enabled = false;

        let config = PaperModeConfig::from_bot_config(&bot_config);

        assert!(config.engines.arbitrage.enabled);
        assert!(config.engines.directional.enabled);
        assert!(!config.engines.maker.enabled);
    }
}
