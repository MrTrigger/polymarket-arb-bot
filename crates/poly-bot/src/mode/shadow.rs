//! Shadow trading mode.
//!
//! Implements shadow mode with real market data but no execution.
//! Uses live WebSocket feeds for price discovery combined with a no-op
//! executor that logs all detected opportunities without trading.
//!
//! ## Components
//!
//! - `LiveDataSource`: Real-time WebSocket data from Binance and Polymarket
//! - `NoOpExecutor`: Logs opportunities but never executes orders
//! - Full observability enabled (decisions, counterfactuals, anomalies)
//!
//! ## Use Cases
//!
//! - Validate feed connectivity before going live
//! - Monitor opportunity detection without risking capital
//! - Debug strategy logic with real market conditions
//! - Benchmark opportunity frequency and quality

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use rust_decimal::Decimal;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use poly_common::ClickHouseClient;

use crate::config::{BotConfig, ObservabilityConfig, TradingMode};
use crate::data_source::live::{LiveDataSource, LiveDataSourceConfig};
use crate::executor::noop::{NoOpExecutor, NoOpExecutorConfig};
use crate::observability::{
    create_shared_analyzer, create_shared_detector_with_capture, AnomalyConfig, CaptureConfig,
    CounterfactualConfig, InMemoryIdLookup, ObservabilityCapture, ProcessorConfig,
};
use crate::state::GlobalState;
use crate::strategy::{StrategyConfig, StrategyLoop};

/// Configuration for shadow trading mode.
#[derive(Debug, Clone)]
pub struct ShadowModeConfig {
    /// Data source configuration.
    pub data_source: LiveDataSourceConfig,
    /// Executor configuration.
    pub executor: NoOpExecutorConfig,
    /// Strategy configuration.
    pub strategy: StrategyConfig,
    /// Observability configuration.
    pub observability: ObservabilityConfig,
    /// Virtual balance for strategy sizing calculations.
    pub virtual_balance: Decimal,
    /// Graceful shutdown timeout (seconds).
    pub shutdown_timeout_secs: u64,
}

impl Default for ShadowModeConfig {
    fn default() -> Self {
        Self {
            data_source: LiveDataSourceConfig::default(),
            executor: NoOpExecutorConfig::default(),
            strategy: StrategyConfig::default(),
            observability: ObservabilityConfig::default(),
            virtual_balance: Decimal::new(10000, 0), // $10,000 virtual
            shutdown_timeout_secs: 30,
        }
    }
}

impl ShadowModeConfig {
    /// Create config from BotConfig.
    pub fn from_bot_config(config: &BotConfig) -> Self {
        let executor_config = NoOpExecutorConfig {
            virtual_balance: Decimal::new(10000, 0),
            log_orders: true,
            rejection_reason: "Shadow mode: execution disabled".to_string(),
        };

        Self {
            data_source: LiveDataSourceConfig::default(),
            executor: executor_config,
            strategy: StrategyConfig::from_trading_config(&config.trading, (config.window_duration.minutes() * 60) as i64),
            observability: config.observability.clone(),
            virtual_balance: Decimal::new(10000, 0),
            shutdown_timeout_secs: 30,
        }
    }
}

/// Shadow trading mode runner.
///
/// Coordinates all components for shadow trading:
/// - Data sources (Binance, Polymarket WebSockets) - real market data
/// - Executor (no-op, logs but never executes)
/// - Strategy loop (arb detection, sizing, opportunity logging)
/// - Observability (decisions, counterfactuals, anomalies)
///
/// Shadow mode is ideal for:
/// - Validating data feed connectivity
/// - Verifying opportunity detection logic
/// - Benchmarking strategy without risk
pub struct ShadowMode {
    /// Configuration.
    config: ShadowModeConfig,
    /// Global shared state.
    state: Arc<GlobalState>,
    /// Shutdown signal sender.
    shutdown_tx: broadcast::Sender<()>,
    /// ClickHouse client for data storage.
    clickhouse: Option<ClickHouseClient>,
}

impl ShadowMode {
    /// Create a new shadow mode runner.
    ///
    /// # Arguments
    ///
    /// * `config` - Shadow mode configuration
    /// * `bot_config` - Full bot configuration (for validation)
    ///
    /// # Errors
    ///
    /// Returns error if configuration validation fails.
    pub fn new(config: ShadowModeConfig, bot_config: &BotConfig) -> Result<Self> {
        // Validate mode
        if bot_config.mode != TradingMode::Shadow {
            anyhow::bail!("ShadowMode requires TradingMode::Shadow");
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

    /// Run shadow trading mode.
    ///
    /// This method runs until shutdown is requested or a fatal error occurs.
    /// In shadow mode, all detected opportunities are logged but never executed.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on clean shutdown, `Err` on fatal error.
    pub async fn run(&mut self) -> Result<()> {
        info!("Starting shadow trading mode");
        info!(
            virtual_balance = %self.config.virtual_balance,
            "Shadow mode configuration (no real execution)"
        );

        // Create data source (real market data)
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let data_source = LiveDataSource::new(self.config.data_source.clone());

        // Create no-op executor (logs but never executes)
        let mut executor_config = self.config.executor.clone();
        executor_config.virtual_balance = self.config.virtual_balance;
        let executor = NoOpExecutor::new(executor_config);

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

        // Enable trading (strategy will detect opportunities, executor will log them)
        self.state.enable_trading();
        info!("Shadow mode enabled - detecting opportunities without execution");

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
            skipped = metrics.trades_skipped,
            "Shadow mode final metrics (no trades executed)"
        );

        info!("Shadow mode shutdown complete");
    }

    /// Request shutdown.
    pub fn request_shutdown(&self) {
        info!("Shadow mode shutdown requested");
        self.state.control.request_shutdown();
        let _ = self.shutdown_tx.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_shadow_mode_config_default() {
        let config = ShadowModeConfig::default();
        assert_eq!(config.virtual_balance, dec!(10000));
        assert_eq!(config.shutdown_timeout_secs, 30);
        assert!(config.executor.log_orders);
    }

    #[test]
    fn test_shadow_mode_config_from_bot_config() {
        let bot_config = BotConfig::default();

        let config = ShadowModeConfig::from_bot_config(&bot_config);
        assert!(config.executor.log_orders);
        assert!(config
            .executor
            .rejection_reason
            .contains("Shadow mode"));
        assert_eq!(config.strategy.max_consecutive_failures, 3);
    }

    #[test]
    fn test_shadow_mode_requires_shadow_trading_mode() {
        let config = ShadowModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Live; // Wrong mode

        let result = ShadowMode::new(config, &bot_config);
        assert!(result.is_err());
    }

    #[test]
    fn test_shadow_mode_accepts_shadow_mode() {
        let config = ShadowModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Shadow;

        let result = ShadowMode::new(config, &bot_config);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_shadow_mode_shutdown_signal() {
        let config = ShadowModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Shadow;

        let mode = ShadowMode::new(config, &bot_config).unwrap();
        let mut rx = mode.shutdown_signal();

        // Request shutdown
        mode.request_shutdown();

        // Should receive signal
        let result = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;

        assert!(result.is_ok());
    }

    #[test]
    fn test_shadow_mode_state_access() {
        let config = ShadowModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Shadow;

        let mode = ShadowMode::new(config, &bot_config).unwrap();
        let state = mode.state();

        // State should not allow trading initially
        assert!(!state.can_trade());
    }

    #[test]
    fn test_shadow_mode_executor_config() {
        let bot_config = BotConfig::default();

        let config = ShadowModeConfig::from_bot_config(&bot_config);

        assert!(config.executor.log_orders);
        assert_eq!(config.executor.virtual_balance, dec!(10000));
    }
}
