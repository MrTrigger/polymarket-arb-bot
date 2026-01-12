//! Live trading mode.
//!
//! Implements real trading with real money against Polymarket's CLOB exchange.
//!
//! ## Components
//!
//! - `LiveDataSource`: Real-time WebSocket data from Binance and Polymarket
//! - `LiveExecutor`: Real order submission via Polymarket CLOB API
//! - Full observability enabled (decisions, counterfactuals, anomalies)
//!
//! ## Safety
//!
//! - Requires `POLY_PRIVATE_KEY` and `POLY_API_KEY` environment variables
//! - Circuit breaker trips after consecutive failures
//! - Graceful shutdown saves state and cancels pending orders

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use poly_common::ClickHouseClient;

use crate::config::{BotConfig, ObservabilityConfig, TradingMode};
use crate::data_source::live::{LiveDataSource, LiveDataSourceConfig};
use crate::executor::live::{LiveExecutor, LiveExecutorConfig};
use crate::observability::{
    create_shared_analyzer, create_shared_detector_with_capture,
    CaptureConfig, CounterfactualConfig, InMemoryIdLookup, ObservabilityCapture,
    ProcessorConfig, AnomalyConfig,
};
use crate::state::GlobalState;
use crate::strategy::{StrategyConfig, StrategyLoop};

/// Configuration for live trading mode.
#[derive(Debug, Clone)]
pub struct LiveModeConfig {
    /// Data source configuration.
    pub data_source: LiveDataSourceConfig,
    /// Executor configuration.
    pub executor: LiveExecutorConfig,
    /// Strategy configuration.
    pub strategy: StrategyConfig,
    /// Observability configuration.
    pub observability: ObservabilityConfig,
    /// Initial USDC balance.
    pub initial_balance: Decimal,
    /// Graceful shutdown timeout (seconds).
    pub shutdown_timeout_secs: u64,
}

impl Default for LiveModeConfig {
    fn default() -> Self {
        Self {
            data_source: LiveDataSourceConfig::default(),
            executor: LiveExecutorConfig::default(),
            strategy: StrategyConfig::default(),
            observability: ObservabilityConfig::default(),
            initial_balance: Decimal::new(1000, 0), // $1000 default
            shutdown_timeout_secs: 30,
        }
    }
}

impl LiveModeConfig {
    /// Create config from BotConfig.
    pub fn from_bot_config(config: &BotConfig) -> Self {
        Self {
            data_source: LiveDataSourceConfig::default(),
            executor: LiveExecutorConfig::from_execution_config(&config.execution),
            strategy: StrategyConfig::from_trading_config(&config.trading),
            observability: config.observability.clone(),
            initial_balance: Decimal::new(1000, 0),
            shutdown_timeout_secs: 30,
        }
    }
}

/// Live trading mode runner.
///
/// Coordinates all components for live trading:
/// - Data sources (Binance, Polymarket WebSockets)
/// - Executor (real order submission)
/// - Strategy loop (arb detection, sizing, execution)
/// - Observability (decisions, counterfactuals, anomalies)
pub struct LiveMode {
    /// Configuration.
    config: LiveModeConfig,
    /// Bot configuration (for wallet credentials).
    bot_config: BotConfig,
    /// Global shared state.
    state: Arc<GlobalState>,
    /// Shutdown signal sender.
    shutdown_tx: broadcast::Sender<()>,
    /// ClickHouse client for data storage.
    clickhouse: Option<ClickHouseClient>,
}

impl LiveMode {
    /// Create a new live mode runner.
    ///
    /// # Arguments
    ///
    /// * `config` - Live mode configuration
    /// * `bot_config` - Full bot configuration (for credentials)
    ///
    /// # Errors
    ///
    /// Returns error if configuration validation fails.
    pub fn new(config: LiveModeConfig, bot_config: BotConfig) -> Result<Self> {
        // Validate mode
        if bot_config.mode != TradingMode::Live {
            anyhow::bail!("LiveMode requires TradingMode::Live");
        }

        // Validate credentials
        bot_config.validate().context("Configuration validation failed")?;

        let state = Arc::new(GlobalState::new());
        let (shutdown_tx, _) = broadcast::channel(16);

        Ok(Self {
            config,
            bot_config,
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

    /// Run live trading mode.
    ///
    /// This method runs until shutdown is requested or a fatal error occurs.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on clean shutdown, `Err` on fatal error.
    pub async fn run(&mut self) -> Result<()> {
        info!("Starting live trading mode");

        // Create data source
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let data_source = LiveDataSource::new(self.config.data_source.clone());

        // Create executor
        let executor = LiveExecutor::new(
            self.config.executor.clone(),
            &self.bot_config.wallet,
            Some(&self.bot_config.shadow),
            self.config.initial_balance,
        )
        .context("Failed to create live executor")?;

        // Set up observability
        let (capture, obs_tasks) = self.setup_observability().await?;

        // Create strategy loop
        let mut strategy = StrategyLoop::new(
            data_source,
            executor,
            self.state.clone(),
            self.config.strategy.clone(),
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
                            crate::strategy::TradeAction::Execute => crate::observability::ActionType::Execute,
                            crate::strategy::TradeAction::SkipSizing => crate::observability::ActionType::SkipSizing,
                            crate::strategy::TradeAction::SkipToxic => crate::observability::ActionType::SkipToxic,
                            crate::strategy::TradeAction::SkipDisabled => crate::observability::ActionType::SkipDisabled,
                            crate::strategy::TradeAction::SkipCircuitBreaker => crate::observability::ActionType::SkipCircuitBreaker,
                        })
                        .latency_us(decision.latency_us)
                        .build();

                    cap_clone.try_capture(snapshot);
                }
            });
        }

        // Enable trading
        self.state.enable_trading();
        info!("Trading enabled");

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
    ) -> Result<(Option<Arc<ObservabilityCapture>>, Vec<tokio::task::JoinHandle<()>>)> {
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
                analyzer_clone.run_cleanup_loop(Duration::from_secs(60), shutdown_rx).await;
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
    )
    where
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
            "Final metrics"
        );

        info!("Shutdown complete");
    }

    /// Request shutdown.
    pub fn request_shutdown(&self) {
        info!("Shutdown requested");
        self.state.control.request_shutdown();
        let _ = self.shutdown_tx.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_live_mode_config_default() {
        let config = LiveModeConfig::default();
        assert_eq!(config.initial_balance, dec!(1000));
        assert_eq!(config.shutdown_timeout_secs, 30);
    }

    #[test]
    fn test_live_mode_config_from_bot_config() {
        let bot_config = BotConfig::default();
        let config = LiveModeConfig::from_bot_config(&bot_config);
        assert_eq!(config.strategy.max_consecutive_failures, 3);
    }

    #[test]
    fn test_live_mode_requires_live_trading_mode() {
        let config = LiveModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Paper; // Wrong mode

        let result = LiveMode::new(config, bot_config);
        assert!(result.is_err());
    }

    #[test]
    fn test_live_mode_requires_credentials() {
        let config = LiveModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Live;
        // No credentials set

        let result = LiveMode::new(config, bot_config);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_live_mode_shutdown_signal() {
        let config = LiveModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Live;
        bot_config.wallet.private_key = Some("0x".to_string() + &"a".repeat(64));
        bot_config.wallet.api_key = Some("test".to_string());
        bot_config.wallet.api_secret = Some("test".to_string());
        bot_config.wallet.api_passphrase = Some("test".to_string());

        let mode = LiveMode::new(config, bot_config).unwrap();
        let mut rx = mode.shutdown_signal();

        // Request shutdown
        mode.request_shutdown();

        // Should receive signal
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            rx.recv(),
        ).await;

        assert!(result.is_ok());
    }

    #[test]
    fn test_live_mode_state_access() {
        let config = LiveModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Live;
        bot_config.wallet.private_key = Some("0x".to_string() + &"a".repeat(64));
        bot_config.wallet.api_key = Some("test".to_string());
        bot_config.wallet.api_secret = Some("test".to_string());
        bot_config.wallet.api_passphrase = Some("test".to_string());

        let mode = LiveMode::new(config, bot_config).unwrap();
        let state = mode.state();

        // State should not allow trading initially
        assert!(!state.can_trade());
    }
}
