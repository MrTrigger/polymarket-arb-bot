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
use poly_market::{DiscoveryConfig, MarketDiscovery};

use crate::config::{BotConfig, EnginesConfig, ObservabilityConfig, TradingMode};
use crate::data_source::live::{ActiveMarket, LiveDataSource, LiveDataSourceConfig};
use crate::executor::paper::{PaperExecutor, PaperExecutorConfig};
use crate::observability::{
    create_shared_analyzer, create_shared_detector_with_capture, AnomalyConfig, CaptureConfig,
    CounterfactualConfig, InMemoryIdLookup, ObservabilityCapture, ProcessorConfig,
};
use crate::state::GlobalState;
use crate::strategy::{StrategyConfig, StrategyLoop};

/// Configuration for paper trading mode.
#[derive(Debug, Clone)]
pub struct PaperModeConfig {
    /// Data source configuration.
    pub data_source: LiveDataSourceConfig,
    /// Executor configuration.
    pub executor: PaperExecutorConfig,
    /// Strategy configuration.
    pub strategy: StrategyConfig,
    /// Observability configuration.
    pub observability: ObservabilityConfig,
    /// Engines configuration (arbitrage, directional, maker).
    pub engines: EnginesConfig,
    /// Market discovery configuration.
    pub discovery: DiscoveryConfig,
    /// Initial virtual balance (USDC).
    pub initial_balance: Decimal,
    /// Graceful shutdown timeout (seconds).
    pub shutdown_timeout_secs: u64,
}

impl Default for PaperModeConfig {
    fn default() -> Self {
        Self {
            data_source: LiveDataSourceConfig::default(),
            executor: PaperExecutorConfig::default(),
            strategy: StrategyConfig::default(),
            observability: ObservabilityConfig::default(),
            engines: EnginesConfig::default(),
            discovery: DiscoveryConfig::default(),
            initial_balance: Decimal::new(10000, 0), // $10,000 default for paper
            shutdown_timeout_secs: 30,
        }
    }
}

impl PaperModeConfig {
    /// Create config from BotConfig.
    pub fn from_bot_config(config: &BotConfig) -> Self {
        let executor_config = PaperExecutorConfig {
            initial_balance: Decimal::new(10000, 0),
            fill_latency_ms: config.execution.paper_fill_latency_ms,
            fee_rate: Decimal::ZERO, // Polymarket has 0% maker fees
            enforce_balance: true,
            max_position_per_market: config.trading.max_position_per_market,
            ..Default::default()
        };

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
            ..Default::default()
        };

        Self {
            data_source: LiveDataSourceConfig::default(),
            executor: executor_config,
            strategy: StrategyConfig::from_trading_config(&config.trading),
            observability: config.observability.clone(),
            engines: config.engines.clone(),
            discovery: discovery_config,
            initial_balance: Decimal::new(10000, 0),
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
pub struct PaperMode {
    /// Configuration.
    config: PaperModeConfig,
    /// Global shared state.
    state: Arc<GlobalState>,
    /// Shutdown signal sender.
    shutdown_tx: broadcast::Sender<()>,
    /// ClickHouse client for data storage.
    clickhouse: Option<ClickHouseClient>,
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

        Ok(Self {
            config,
            state,
            shutdown_tx,
            clickhouse: None,
        })
    }

    /// Set the ClickHouse client for data storage.
    pub fn with_clickhouse(mut self, client: ClickHouseClient) -> Self {
        self.clickhouse = Some(client);
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
        info!(
            initial_balance = %self.config.initial_balance,
            fill_latency_ms = self.config.executor.fill_latency_ms,
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

        // Get handle to active markets before moving data_source
        let active_markets = data_source.active_markets_handle();

        // Spawn market discovery task
        let discovery_shutdown = self.shutdown_tx.subscribe();
        let discovery_config = self.config.discovery.clone();
        let _discovery_handle = tokio::spawn(run_market_discovery(
            discovery_config,
            active_markets,
            discovery_shutdown,
        ));

        // Create paper executor (simulated fills)
        let mut executor_config = self.config.executor.clone();
        executor_config.initial_balance = self.config.initial_balance;
        let executor = PaperExecutor::new(executor_config);

        // Set up observability
        let (capture, obs_tasks) = self.setup_observability().await?;

        // Create strategy loop with engines config
        let mut strategy = StrategyLoop::with_engines(
            data_source,
            executor,
            self.state.clone(),
            self.config.strategy.clone(),
            self.config.engines.clone(),
        );

        // Add observability sender if capture is enabled
        if let Some(ref cap) = capture
            && cap.is_enabled()
        {
            // Create a simple channel for TradeDecision (separate from ObservabilityEvent)
            let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
            strategy = strategy.with_observability(tx);

            // Spawn task to forward decisions to capture
            let cap_clone = cap.clone();
            tokio::spawn(async move {
                while let Some(decision) = rx.recv().await {
                    // Convert TradeDecision to DecisionSnapshot and capture
                    let snapshot = crate::observability::SnapshotBuilder::new()
                        .event_id(&decision.event_id)
                        .yes_token_id(&decision.opportunity.yes_token_id)
                        .no_token_id(&decision.opportunity.no_token_id)
                        .yes_ask(decision.opportunity.yes_ask)
                        .no_ask(decision.opportunity.no_ask)
                        .margin(decision.opportunity.margin)
                        .size(decision.opportunity.max_size)
                        .seconds_remaining(decision.opportunity.seconds_remaining)
                        .confidence(decision.opportunity.confidence)
                        .asset(decision.asset)
                        .phase(decision.opportunity.phase)
                        .action(match decision.action {
                            crate::strategy::TradeAction::Execute => {
                                crate::observability::ActionType::Execute
                            }
                            crate::strategy::TradeAction::SkipSizing => {
                                crate::observability::ActionType::SkipSizing
                            }
                            crate::strategy::TradeAction::SkipToxic => {
                                crate::observability::ActionType::SkipToxic
                            }
                            crate::strategy::TradeAction::SkipDisabled => {
                                crate::observability::ActionType::SkipDisabled
                            }
                            crate::strategy::TradeAction::SkipCircuitBreaker => {
                                crate::observability::ActionType::SkipCircuitBreaker
                            }
                        })
                        .latency_us(decision.latency_us)
                        .build();

                    cap_clone.try_capture(snapshot);
                }
            });
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
        self.shutdown(&mut strategy, obs_tasks).await;

        result
    }

    /// Set up observability components.
    async fn setup_observability(
        &self,
    ) -> Result<(
        Option<Arc<ObservabilityCapture>>,
        Vec<tokio::task::JoinHandle<()>>,
    )> {
        let obs_config = &self.config.observability;
        let mut tasks = Vec::new();

        if !obs_config.capture_decisions {
            return Ok((None, tasks));
        }

        // Create capture channel using from_config
        let capture_config = CaptureConfig {
            enabled: true,
            channel_capacity: obs_config.channel_buffer_size,
            log_drops: true,
            drop_log_threshold: 100,
        };

        let (capture, receiver) = ObservabilityCapture::from_config(capture_config);
        let capture = Arc::new(capture);

        // Create ID lookup for enrichment
        let id_lookup = Arc::new(InMemoryIdLookup::new());

        // Create processor if we have both ClickHouse and a receiver
        if let (Some(client), Some(receiver)) = (&self.clickhouse, receiver) {
            let processor_config = ProcessorConfig {
                batch_size: obs_config.batch_size,
                flush_interval: Duration::from_secs(obs_config.flush_interval_secs),
                max_buffer_size: obs_config.channel_buffer_size,
                process_counterfactuals: obs_config.capture_counterfactuals,
                process_anomalies: obs_config.detect_anomalies,
            };

            // Create processor and run it
            let processor = crate::observability::ObservabilityProcessor::new(
                processor_config,
                id_lookup.clone(),
            );

            let shutdown_rx = self.shutdown_tx.subscribe();
            let client_clone = client.clone();
            let handle = tokio::spawn(async move {
                processor.run(receiver, client_clone, shutdown_rx).await;
            });
            tasks.push(handle);
        }

        // Create counterfactual analyzer if enabled
        if obs_config.capture_counterfactuals {
            let cf_config = CounterfactualConfig::default();
            let analyzer = create_shared_analyzer(cf_config);

            // Spawn cleanup loop
            let analyzer_clone = analyzer.clone();
            let shutdown_rx = self.shutdown_tx.subscribe();
            let handle = tokio::spawn(async move {
                analyzer_clone
                    .run_cleanup_loop(Duration::from_secs(60), shutdown_rx)
                    .await;
            });
            tasks.push(handle);
        }

        // Create anomaly detector if enabled
        if obs_config.detect_anomalies {
            let _detector = create_shared_detector_with_capture(
                AnomalyConfig::from_observability_config(obs_config),
                capture.clone(),
            );
            // Detector is used by strategy loop for real-time detection
        }

        Ok((Some(capture), tasks))
    }

    /// Perform graceful shutdown.
    async fn shutdown<D, E>(
        &self,
        strategy: &mut StrategyLoop<D, E>,
        obs_tasks: Vec<tokio::task::JoinHandle<()>>,
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

        // Wait for observability tasks with timeout
        let timeout = Duration::from_secs(self.config.shutdown_timeout_secs);
        for handle in obs_tasks {
            match tokio::time::timeout(timeout, handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Observability task panicked: {}", e),
                Err(_) => warn!("Observability task timed out during shutdown"),
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

        info!("Paper trading shutdown complete");
    }

    /// Request shutdown.
    pub fn request_shutdown(&self) {
        info!("Paper trading shutdown requested");
        self.state.control.request_shutdown();
        let _ = self.shutdown_tx.send(());
    }
}

/// Run market discovery in a background task.
///
/// Periodically queries the Gamma API for new 15-minute markets
/// and adds them to the active markets state for CLOB subscription.
async fn run_market_discovery(
    config: DiscoveryConfig,
    active_markets: crate::data_source::live::ActiveMarketsState,
    mut shutdown: broadcast::Receiver<()>,
) {
    info!(
        "Starting market discovery for {:?}",
        config.assets.iter().map(|a| a.as_str()).collect::<Vec<_>>()
    );

    let mut discovery = MarketDiscovery::new(config.clone());
    let poll_interval = config.poll_interval;

    loop {
        // Discover new markets
        match discovery.discover().await {
            Ok(markets) => {
                if !markets.is_empty() {
                    info!("Discovered {} new markets", markets.len());

                    // Add discovered markets to the active markets state
                    let mut active = active_markets.write().await;
                    for market in &markets {
                        let active_market = ActiveMarket {
                            event_id: market.event_id.clone(),
                            yes_token_id: market.yes_token_id.clone(),
                            no_token_id: market.no_token_id.clone(),
                            asset: market.asset,
                            strike_price: market.strike_price,
                            window_end: market.window_end,
                        };
                        info!(
                            "Adding market {} ({} strike={}) to active markets",
                            market.event_id, market.asset, market.strike_price
                        );
                        active.insert(market.event_id.clone(), active_market);
                    }
                }
            }
            Err(e) => {
                error!("Market discovery error: {}", e);
            }
        }

        // Wait for next poll or shutdown
        tokio::select! {
            _ = tokio::time::sleep(poll_interval) => {}
            _ = shutdown.recv() => {
                info!("Market discovery received shutdown signal");
                break;
            }
        }
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
        assert_eq!(config.executor.fill_latency_ms, 50);
    }

    #[test]
    fn test_paper_mode_config_from_bot_config() {
        let mut bot_config = BotConfig::default();
        bot_config.execution.paper_fill_latency_ms = 100;

        let config = PaperModeConfig::from_bot_config(&bot_config);
        assert_eq!(config.executor.fill_latency_ms, 100);
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

        assert_eq!(config.executor.fill_latency_ms, 25);
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
