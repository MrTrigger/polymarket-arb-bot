//! Backtest trading mode.
//!
//! Implements backtesting with historical data replay and simulated execution.
//! Uses ClickHouse data for price discovery combined with a backtest executor
//! that simulates fills against historical order book depth.
//!
//! ## Components
//!
//! - `ReplayDataSource`: Historical data from ClickHouse
//! - `BacktestExecutor`: Simulated fills against historical order book
//! - Optional observability (decisions, counterfactuals, anomalies)
//!
//! ## Use Cases
//!
//! - Validate strategy performance on historical data
//! - Tune parameters before live trading
//! - Generate P&L reports and performance metrics
//! - Run parameter sweeps to find optimal settings

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use poly_common::types::CryptoAsset;
use poly_common::ClickHouseClient;

use crate::config::{BacktestConfig, BotConfig, ObservabilityConfig, TradingMode};
use crate::data_source::replay::{ReplayConfig, ReplayDataSource};
use crate::data_source::{DataSource, MarketEvent};
use crate::executor::backtest::{BacktestExecutor, BacktestExecutorConfig, BacktestStats};
use crate::executor::Executor;
use crate::observability::{
    create_shared_analyzer, create_shared_detector_with_capture, AnomalyConfig, CaptureConfig,
    CounterfactualConfig, InMemoryIdLookup, ObservabilityCapture, ProcessorConfig,
};
use crate::state::GlobalState;
use crate::strategy::{StrategyConfig, StrategyLoop};
use crate::types::OrderBook;

/// Configuration for backtest mode.
#[derive(Debug, Clone)]
pub struct BacktestModeConfig {
    /// Data source configuration.
    pub data_source: ReplayConfig,
    /// Executor configuration.
    pub executor: BacktestExecutorConfig,
    /// Strategy configuration.
    pub strategy: StrategyConfig,
    /// Observability configuration.
    pub observability: ObservabilityConfig,
    /// Initial virtual balance (USDC).
    pub initial_balance: Decimal,
    /// Graceful shutdown timeout (seconds).
    pub shutdown_timeout_secs: u64,
    /// Enable parameter sweep mode.
    pub sweep_enabled: bool,
    /// Parameter sweep configurations.
    pub sweep_params: Vec<SweepParameter>,
}

impl Default for BacktestModeConfig {
    fn default() -> Self {
        Self {
            data_source: ReplayConfig::default(),
            executor: BacktestExecutorConfig::default(),
            strategy: StrategyConfig::default(),
            observability: ObservabilityConfig::default(),
            initial_balance: dec!(10000),
            shutdown_timeout_secs: 30,
            sweep_enabled: false,
            sweep_params: Vec::new(),
        }
    }
}

impl BacktestModeConfig {
    /// Create config from BotConfig.
    pub fn from_bot_config(config: &BotConfig, _clickhouse: &ClickHouseClient) -> Result<Self> {
        let executor_config = BacktestExecutorConfig {
            initial_balance: dec!(10000),
            fee_rate: Decimal::ZERO, // Polymarket has 0% maker fees
            latency_ms: config.execution.paper_fill_latency_ms,
            enforce_balance: true,
            max_position_per_market: config.trading.max_position_per_market,
            min_fill_ratio: dec!(0.5),
        };

        // Parse date range from config
        let (start_time, end_time) = parse_date_range(&config.backtest)?;

        let replay_config = ReplayConfig {
            start_time,
            end_time,
            event_ids: Vec::new(),
            assets: config
                .assets
                .iter()
                .filter_map(|s| parse_asset(s))
                .collect(),
            batch_size: 10_000,
            speed: config.backtest.speed,
        };

        Ok(Self {
            data_source: replay_config,
            executor: executor_config,
            strategy: StrategyConfig::from_trading_config(&config.trading),
            observability: config.observability.clone(),
            initial_balance: dec!(10000),
            shutdown_timeout_secs: 30,
            sweep_enabled: config.backtest.sweep_enabled,
            sweep_params: Vec::new(),
        })
    }

    /// Set the date range for backtesting.
    pub fn with_date_range(mut self, start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
        self.data_source.start_time = start;
        self.data_source.end_time = end;
        self
    }

    /// Set the replay speed (0.0 = max speed, 1.0 = real-time).
    pub fn with_speed(mut self, speed: f64) -> Self {
        self.data_source.speed = speed;
        self
    }

    /// Enable parameter sweep with the given parameters.
    pub fn with_sweep(mut self, params: Vec<SweepParameter>) -> Self {
        self.sweep_enabled = true;
        self.sweep_params = params;
        self
    }
}

/// Parameter to sweep during backtesting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepParameter {
    /// Parameter name.
    pub name: String,
    /// Starting value.
    pub start: f64,
    /// Ending value.
    pub end: f64,
    /// Step size.
    pub step: f64,
}

impl SweepParameter {
    /// Create a new sweep parameter.
    pub fn new(name: &str, start: f64, end: f64, step: f64) -> Self {
        Self {
            name: name.to_string(),
            start,
            end,
            step,
        }
    }

    /// Generate all values for this parameter.
    pub fn values(&self) -> Vec<f64> {
        let mut values = Vec::new();
        let mut current = self.start;
        while current <= self.end {
            values.push(current);
            current += self.step;
        }
        values
    }
}

/// Results from a single backtest run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestResult {
    /// Start time of the backtest period.
    pub start_time: DateTime<Utc>,
    /// End time of the backtest period.
    pub end_time: DateTime<Utc>,
    /// Initial balance.
    pub initial_balance: Decimal,
    /// Final balance.
    pub final_balance: Decimal,
    /// Total P&L (realized).
    pub total_pnl: Decimal,
    /// Return percentage.
    pub return_pct: Decimal,
    /// Total trades executed.
    pub trades_executed: u64,
    /// Total trades rejected.
    pub trades_rejected: u64,
    /// Total volume traded.
    pub volume_traded: Decimal,
    /// Total fees paid.
    pub fees_paid: Decimal,
    /// Opportunities detected.
    pub opportunities_detected: u64,
    /// Opportunities skipped.
    pub opportunities_skipped: u64,
    /// Win rate (if applicable).
    pub win_rate: Option<Decimal>,
    /// Sharpe ratio (if applicable).
    pub sharpe_ratio: Option<f64>,
    /// Maximum drawdown.
    pub max_drawdown: Option<Decimal>,
    /// Parameters used (for sweep mode).
    pub parameters: HashMap<String, f64>,
    /// Execution duration.
    pub duration_secs: f64,
}

impl BacktestResult {
    /// Create a new backtest result.
    pub fn new(
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        initial_balance: Decimal,
        final_balance: Decimal,
        stats: &BacktestStats,
        metrics: &crate::state::MetricsSnapshot,
        duration_secs: f64,
    ) -> Self {
        let total_pnl = final_balance - initial_balance;
        let return_pct = if initial_balance > Decimal::ZERO {
            (total_pnl / initial_balance) * dec!(100)
        } else {
            Decimal::ZERO
        };

        Self {
            start_time,
            end_time,
            initial_balance,
            final_balance,
            total_pnl,
            return_pct,
            trades_executed: stats.orders_filled + stats.orders_partial,
            trades_rejected: stats.orders_rejected,
            volume_traded: stats.volume_traded,
            fees_paid: stats.fees_paid,
            opportunities_detected: metrics.opportunities_detected,
            opportunities_skipped: metrics.trades_skipped,
            win_rate: None, // Could be calculated from trade history
            sharpe_ratio: None, // Would need return series
            max_drawdown: None, // Would need balance history
            parameters: HashMap::new(),
            duration_secs,
        }
    }

    /// Add a parameter value (for sweep results).
    pub fn with_parameter(mut self, name: &str, value: f64) -> Self {
        self.parameters.insert(name.to_string(), value);
        self
    }
}

/// P&L report for display.
#[derive(Debug)]
pub struct PnLReport {
    /// Results from the backtest.
    pub result: BacktestResult,
    /// Position summary at end.
    pub positions: Vec<PositionSummary>,
}

impl PnLReport {
    /// Format as a human-readable string.
    pub fn to_string_report(&self) -> String {
        let mut report = String::new();
        report.push_str("═══════════════════════════════════════════════════════════\n");
        report.push_str("                    BACKTEST P&L REPORT                     \n");
        report.push_str("═══════════════════════════════════════════════════════════\n\n");

        report.push_str(&format!(
            "Period: {} to {}\n",
            self.result.start_time.format("%Y-%m-%d %H:%M"),
            self.result.end_time.format("%Y-%m-%d %H:%M")
        ));
        report.push_str(&format!(
            "Duration: {:.2} seconds\n\n",
            self.result.duration_secs
        ));

        report.push_str("─── PERFORMANCE ───────────────────────────────────────────\n");
        report.push_str(&format!(
            "Initial Balance:      ${:.2}\n",
            self.result.initial_balance
        ));
        report.push_str(&format!(
            "Final Balance:        ${:.2}\n",
            self.result.final_balance
        ));
        report.push_str(&format!(
            "Total P&L:            ${:.2}\n",
            self.result.total_pnl
        ));
        report.push_str(&format!(
            "Return:               {:.2}%\n\n",
            self.result.return_pct
        ));

        report.push_str("─── TRADING ACTIVITY ──────────────────────────────────────\n");
        report.push_str(&format!(
            "Opportunities Found:  {}\n",
            self.result.opportunities_detected
        ));
        report.push_str(&format!(
            "Opportunities Skipped:{}\n",
            self.result.opportunities_skipped
        ));
        report.push_str(&format!(
            "Trades Executed:      {}\n",
            self.result.trades_executed
        ));
        report.push_str(&format!(
            "Trades Rejected:      {}\n",
            self.result.trades_rejected
        ));
        report.push_str(&format!(
            "Volume Traded:        ${:.2}\n",
            self.result.volume_traded
        ));
        report.push_str(&format!(
            "Fees Paid:            ${:.4}\n\n",
            self.result.fees_paid
        ));

        if !self.positions.is_empty() {
            report.push_str("─── FINAL POSITIONS ───────────────────────────────────────\n");
            for pos in &self.positions {
                report.push_str(&format!(
                    "{}: YES={:.0}, NO={:.0}, Cost=${:.2}\n",
                    pos.event_id, pos.yes_shares, pos.no_shares, pos.cost_basis
                ));
            }
            report.push('\n');
        }

        if !self.result.parameters.is_empty() {
            report.push_str("─── PARAMETERS ────────────────────────────────────────────\n");
            for (name, value) in &self.result.parameters {
                report.push_str(&format!("{}: {:.4}\n", name, value));
            }
            report.push('\n');
        }

        report.push_str("═══════════════════════════════════════════════════════════\n");

        report
    }
}

/// Summary of a position at end of backtest.
#[derive(Debug, Clone)]
pub struct PositionSummary {
    /// Event ID.
    pub event_id: String,
    /// YES shares held.
    pub yes_shares: Decimal,
    /// NO shares held.
    pub no_shares: Decimal,
    /// Cost basis.
    pub cost_basis: Decimal,
}

/// Backtest trading mode runner.
///
/// Coordinates all components for backtesting:
/// - Data source (historical ClickHouse data)
/// - Executor (simulated fills against order book)
/// - Strategy loop (arb detection, sizing, execution)
/// - Optional observability
pub struct BacktestMode {
    /// Configuration.
    config: BacktestModeConfig,
    /// Global shared state.
    state: Arc<GlobalState>,
    /// Shutdown signal sender.
    shutdown_tx: broadcast::Sender<()>,
    /// ClickHouse client for data retrieval.
    clickhouse: ClickHouseClient,
    /// Results from sweep runs.
    sweep_results: Vec<BacktestResult>,
}

impl BacktestMode {
    /// Create a new backtest mode runner.
    ///
    /// # Arguments
    ///
    /// * `config` - Backtest mode configuration
    /// * `bot_config` - Full bot configuration (for validation)
    /// * `clickhouse` - ClickHouse client for data loading
    ///
    /// # Errors
    ///
    /// Returns error if configuration validation fails.
    pub fn new(
        config: BacktestModeConfig,
        bot_config: &BotConfig,
        clickhouse: ClickHouseClient,
    ) -> Result<Self> {
        // Validate mode
        if bot_config.mode != TradingMode::Backtest {
            anyhow::bail!("BacktestMode requires TradingMode::Backtest");
        }

        let state = Arc::new(GlobalState::new());
        let (shutdown_tx, _) = broadcast::channel(16);

        Ok(Self {
            config,
            state,
            shutdown_tx,
            clickhouse,
            sweep_results: Vec::new(),
        })
    }

    /// Get a shutdown signal receiver.
    pub fn shutdown_signal(&self) -> broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    /// Get the global state.
    pub fn state(&self) -> Arc<GlobalState> {
        self.state.clone()
    }

    /// Get sweep results after running.
    pub fn sweep_results(&self) -> &[BacktestResult] {
        &self.sweep_results
    }

    /// Run backtest mode.
    ///
    /// This method runs until all historical data is processed.
    ///
    /// # Returns
    ///
    /// Returns `Ok(BacktestResult)` on completion, `Err` on error.
    pub async fn run(&mut self) -> Result<BacktestResult> {
        if self.config.sweep_enabled && !self.config.sweep_params.is_empty() {
            self.run_sweep().await
        } else {
            self.run_single().await
        }
    }

    /// Run a single backtest.
    async fn run_single(&mut self) -> Result<BacktestResult> {
        let start_instant = std::time::Instant::now();

        info!("Starting backtest mode");
        info!(
            start = %self.config.data_source.start_time,
            end = %self.config.data_source.end_time,
            speed = self.config.data_source.speed,
            initial_balance = %self.config.initial_balance,
            "Backtest configuration"
        );

        // Reset state for this run
        self.state = Arc::new(GlobalState::new());

        // Create data source (historical data from ClickHouse)
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let mut data_source =
            ReplayDataSource::new(self.config.data_source.clone(), self.clickhouse.clone());

        // Create backtest executor
        let mut executor_config = self.config.executor.clone();
        executor_config.initial_balance = self.config.initial_balance;
        let mut executor = BacktestExecutor::new(executor_config);
        let order_books = executor.order_books();

        // Set up observability
        let (capture, obs_tasks) = self.setup_observability().await?;

        // Create strategy loop (used for observability channel setup)
        // Note: In backtest mode, we drive events manually rather than using
        // the strategy loop's run() method
        let mut _strategy = StrategyLoop::new(
            MockDataSource, // Placeholder - we'll drive events manually
            MockExecutor,   // Placeholder - we'll drive execution manually
            self.state.clone(),
            self.config.strategy.clone(),
        );

        // Add observability sender if capture is enabled
        if let Some(ref cap) = capture
            && cap.is_enabled()
        {
            let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
            _strategy = _strategy.with_observability(tx);

            // Spawn task to forward decisions to capture
            let cap_clone = cap.clone();
            tokio::spawn(async move {
                while let Some(decision) = rx.recv().await {
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
        info!("Backtest started - processing historical data");

        // Process events from replay source
        let mut events_processed: u64 = 0;
        let result = loop {
            tokio::select! {
                event_result = data_source.next_event() => {
                    match event_result {
                        Ok(Some(event)) => {
                            events_processed += 1;

                            // Update simulation time
                            let time = event.timestamp();
                            executor.set_time(time);

                            // Process the event
                            self.process_event(&event, &mut executor, &order_books).await;

                            // Log progress periodically
                            if events_processed.is_multiple_of(10_000) {
                                debug!(
                                    events = events_processed,
                                    balance = %executor.balance(),
                                    "Backtest progress"
                                );
                            }
                        }
                        Ok(None) => {
                            info!("Replay complete - all historical data processed");
                            break Ok(());
                        }
                        Err(e) => {
                            error!("Error reading replay data: {}", e);
                            break Err(anyhow::anyhow!("Replay error: {}", e));
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!("Shutdown signal received");
                    break Ok(());
                }
            }
        };

        // Graceful shutdown
        self.shutdown_internal(&mut data_source, &mut executor, obs_tasks)
            .await;

        // Generate result
        let duration_secs = start_instant.elapsed().as_secs_f64();
        let metrics = self.state.metrics.snapshot();

        let backtest_result = BacktestResult::new(
            self.config.data_source.start_time,
            self.config.data_source.end_time,
            self.config.initial_balance,
            executor.balance(),
            executor.stats(),
            &metrics,
            duration_secs,
        );

        // Log final report
        let positions: Vec<PositionSummary> = Vec::new(); // Would collect from executor
        let report = PnLReport {
            result: backtest_result.clone(),
            positions,
        };
        info!("\n{}", report.to_string_report());

        result?;
        Ok(backtest_result)
    }

    /// Run parameter sweep.
    async fn run_sweep(&mut self) -> Result<BacktestResult> {
        info!("Starting parameter sweep backtest");

        // Generate all parameter combinations
        let combinations = self.generate_sweep_combinations();
        let total_runs = combinations.len();

        info!(
            "Running {} parameter combinations",
            total_runs
        );

        let mut best_result: Option<BacktestResult> = None;
        let mut best_pnl = Decimal::MIN;

        for (run_idx, params) in combinations.into_iter().enumerate() {
            info!(
                "Sweep run {}/{}: {:?}",
                run_idx + 1,
                total_runs,
                params
            );

            // Apply parameters to config
            let mut run_config = self.config.clone();
            for (name, value) in &params {
                apply_parameter(&mut run_config, name, *value);
            }

            // Run backtest with this config
            let original_config = std::mem::replace(&mut self.config, run_config);
            let result = self.run_single().await;
            self.config = original_config;

            match result {
                Ok(mut res) => {
                    // Add parameters to result
                    for (name, value) in &params {
                        res = res.with_parameter(name, *value);
                    }

                    info!(
                        "Run {}: P&L=${:.2}, Return={:.2}%",
                        run_idx + 1,
                        res.total_pnl,
                        res.return_pct
                    );

                    if res.total_pnl > best_pnl {
                        best_pnl = res.total_pnl;
                        best_result = Some(res.clone());
                    }

                    self.sweep_results.push(res);
                }
                Err(e) => {
                    warn!("Sweep run {} failed: {}", run_idx + 1, e);
                }
            }
        }

        // Return best result or error
        best_result.ok_or_else(|| anyhow::anyhow!("All sweep runs failed"))
    }

    /// Generate all combinations of sweep parameters.
    fn generate_sweep_combinations(&self) -> Vec<HashMap<String, f64>> {
        let mut combinations = vec![HashMap::new()];

        for param in &self.config.sweep_params {
            let values = param.values();
            let mut new_combinations = Vec::new();

            for combo in combinations {
                for value in &values {
                    let mut new_combo = combo.clone();
                    new_combo.insert(param.name.clone(), *value);
                    new_combinations.push(new_combo);
                }
            }

            combinations = new_combinations;
        }

        combinations
    }

    /// Process a single market event.
    async fn process_event(
        &self,
        event: &MarketEvent,
        executor: &mut BacktestExecutor,
        order_books: &Arc<tokio::sync::RwLock<HashMap<String, OrderBook>>>,
    ) {
        match event {
            MarketEvent::SpotPrice(price_event) => {
                // Update spot price in state
                self.state.market_data.update_spot_price(
                    price_event.asset.as_str(),
                    price_event.price,
                    price_event.timestamp.timestamp_millis(),
                );
            }
            MarketEvent::BookSnapshot(snapshot) => {
                // Update order book in executor
                let mut book = OrderBook::new(snapshot.token_id.clone());
                book.apply_snapshot(
                    snapshot.bids.clone(),
                    snapshot.asks.clone(),
                    snapshot.timestamp.timestamp_millis(),
                );
                executor.update_book(&snapshot.token_id, book.clone()).await;

                // Also update in shared order books
                let mut books = order_books.write().await;
                books.insert(snapshot.token_id.clone(), book);
            }
            MarketEvent::BookDelta(delta) => {
                // Apply delta to order book
                let mut books = order_books.write().await;
                if let Some(book) = books.get_mut(&delta.token_id) {
                    book.apply_delta(
                        delta.side,
                        delta.price,
                        delta.size,
                        delta.timestamp.timestamp_millis(),
                    );
                    // Update executor's copy too
                    executor.update_book(&delta.token_id, book.clone()).await;
                }
            }
            MarketEvent::WindowOpen(window) => {
                // Track new market window
                debug!(
                    event_id = %window.event_id,
                    asset = ?window.asset,
                    strike = %window.strike_price,
                    "Market window opened"
                );
                self.state.metrics.inc_events();
            }
            MarketEvent::WindowClose(window) => {
                debug!(
                    event_id = %window.event_id,
                    outcome = ?window.outcome,
                    "Market window closed"
                );
            }
            MarketEvent::Fill(fill) => {
                debug!(
                    event_id = %fill.event_id,
                    outcome = ?fill.outcome,
                    size = %fill.size,
                    price = %fill.price,
                    "Fill received"
                );
            }
            MarketEvent::Heartbeat(_) => {
                // Ignore heartbeats
            }
        }

        // Update metrics
        self.state.metrics.inc_events();
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

        // Create capture channel
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

        // Create processor if we have a receiver
        if let Some(receiver) = receiver {
            let processor_config = ProcessorConfig {
                batch_size: obs_config.batch_size,
                flush_interval: Duration::from_secs(obs_config.flush_interval_secs),
                max_buffer_size: obs_config.channel_buffer_size,
                process_counterfactuals: obs_config.capture_counterfactuals,
                process_anomalies: obs_config.detect_anomalies,
            };

            let processor = crate::observability::ObservabilityProcessor::new(
                processor_config,
                id_lookup.clone(),
            );

            let shutdown_rx = self.shutdown_tx.subscribe();
            let client_clone = self.clickhouse.clone();
            let handle = tokio::spawn(async move {
                processor.run(receiver, client_clone, shutdown_rx).await;
            });
            tasks.push(handle);
        }

        // Create counterfactual analyzer if enabled
        if obs_config.capture_counterfactuals {
            let cf_config = CounterfactualConfig::default();
            let analyzer = create_shared_analyzer(cf_config);

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
        }

        Ok((Some(capture), tasks))
    }

    /// Perform graceful shutdown.
    async fn shutdown_internal(
        &self,
        data_source: &mut ReplayDataSource,
        executor: &mut BacktestExecutor,
        obs_tasks: Vec<tokio::task::JoinHandle<()>>,
    ) {
        info!("Beginning backtest shutdown");

        // Disable trading
        self.state.disable_trading();

        // Signal shutdown
        let _ = self.shutdown_tx.send(());

        // Shutdown data source and executor
        data_source.shutdown().await;
        executor.shutdown().await;

        // Wait for observability tasks with timeout
        let timeout = Duration::from_secs(self.config.shutdown_timeout_secs);
        for handle in obs_tasks {
            match tokio::time::timeout(timeout, handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Observability task panicked: {}", e),
                Err(_) => warn!("Observability task timed out during shutdown"),
            }
        }

        info!("Backtest shutdown complete");
    }

    /// Request shutdown.
    pub fn request_shutdown(&self) {
        info!("Backtest shutdown requested");
        self.state.control.request_shutdown();
        let _ = self.shutdown_tx.send(());
    }
}

// Helper types for strategy loop placeholders
struct MockDataSource;
struct MockExecutor;

#[async_trait::async_trait]
impl DataSource for MockDataSource {
    async fn next_event(&mut self) -> Result<Option<MarketEvent>, crate::data_source::DataSourceError> {
        Ok(None)
    }
    fn has_more(&self) -> bool {
        false
    }
    fn current_time(&self) -> Option<DateTime<Utc>> {
        None
    }
    async fn shutdown(&mut self) {}
}

#[async_trait::async_trait]
impl Executor for MockExecutor {
    async fn place_order(
        &mut self,
        _order: crate::executor::OrderRequest,
    ) -> Result<crate::executor::OrderResult, crate::executor::ExecutorError> {
        Err(crate::executor::ExecutorError::Connection(
            "Mock executor".to_string(),
        ))
    }
    async fn cancel_order(
        &mut self,
        _order_id: &str,
    ) -> Result<crate::executor::OrderCancellation, crate::executor::ExecutorError> {
        Err(crate::executor::ExecutorError::Connection(
            "Mock executor".to_string(),
        ))
    }
    async fn order_status(&self, _order_id: &str) -> Option<crate::executor::OrderResult> {
        None
    }
    fn pending_orders(&self) -> Vec<crate::executor::PendingOrder> {
        Vec::new()
    }
    fn available_balance(&self) -> Decimal {
        Decimal::ZERO
    }
    async fn shutdown(&mut self) {}
}

/// Parse date range from BacktestConfig.
fn parse_date_range(config: &BacktestConfig) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let start = if let Some(ref date_str) = config.start_date {
        parse_date(date_str).context("Invalid start_date")?
    } else {
        // Default to 24 hours ago
        Utc::now() - chrono::Duration::hours(24)
    };

    let end = if let Some(ref date_str) = config.end_date {
        parse_date(date_str)
            .context("Invalid end_date")?
            // End of day
            + chrono::Duration::hours(23)
            + chrono::Duration::minutes(59)
            + chrono::Duration::seconds(59)
    } else {
        // Default to now
        Utc::now()
    };

    if start >= end {
        anyhow::bail!("start_date must be before end_date");
    }

    Ok((start, end))
}

/// Parse a date string (YYYY-MM-DD) to DateTime<Utc>.
fn parse_date(s: &str) -> Result<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("Invalid date format: {}. Expected YYYY-MM-DD", s))?;
    Ok(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap()))
}

/// Parse asset string to CryptoAsset.
fn parse_asset(s: &str) -> Option<CryptoAsset> {
    match s.to_uppercase().as_str() {
        "BTC" => Some(CryptoAsset::Btc),
        "ETH" => Some(CryptoAsset::Eth),
        "SOL" => Some(CryptoAsset::Sol),
        "XRP" => Some(CryptoAsset::Xrp),
        _ => None,
    }
}

/// Apply a sweep parameter to the config.
fn apply_parameter(config: &mut BacktestModeConfig, name: &str, value: f64) {
    match name {
        "margin_early" => {
            config.strategy.arb_thresholds.early =
                Decimal::from_f64_retain(value).unwrap_or_default();
        }
        "margin_mid" => {
            config.strategy.arb_thresholds.mid =
                Decimal::from_f64_retain(value).unwrap_or_default();
        }
        "margin_late" => {
            config.strategy.arb_thresholds.late =
                Decimal::from_f64_retain(value).unwrap_or_default();
        }
        "max_position_per_market" => {
            config.strategy.sizing_config.max_position_per_market =
                Decimal::from_f64_retain(value).unwrap_or_default();
        }
        "base_order_size" => {
            config.strategy.sizing_config.base_order_size =
                Decimal::from_f64_retain(value).unwrap_or_default();
        }
        "min_fill_ratio" => {
            config.executor.min_fill_ratio = Decimal::from_f64_retain(value).unwrap_or_default();
        }
        _ => {
            warn!("Unknown sweep parameter: {}", name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backtest_mode_config_default() {
        let config = BacktestModeConfig::default();
        assert_eq!(config.initial_balance, dec!(10000));
        assert_eq!(config.shutdown_timeout_secs, 30);
        assert!(!config.sweep_enabled);
    }

    #[test]
    fn test_sweep_parameter_values() {
        let param = SweepParameter::new("margin_early_pct", 1.0, 3.0, 0.5);
        let values = param.values();
        assert_eq!(values, vec![1.0, 1.5, 2.0, 2.5, 3.0]);
    }

    #[test]
    fn test_parse_date() {
        let date = parse_date("2024-01-15").unwrap();
        assert_eq!(date.format("%Y-%m-%d").to_string(), "2024-01-15");
    }

    #[test]
    fn test_parse_date_invalid() {
        assert!(parse_date("invalid").is_err());
        assert!(parse_date("01-15-2024").is_err());
    }

    #[test]
    fn test_parse_asset() {
        assert_eq!(parse_asset("BTC"), Some(CryptoAsset::Btc));
        assert_eq!(parse_asset("btc"), Some(CryptoAsset::Btc));
        assert_eq!(parse_asset("ETH"), Some(CryptoAsset::Eth));
        assert_eq!(parse_asset("SOL"), Some(CryptoAsset::Sol));
        assert_eq!(parse_asset("XRP"), Some(CryptoAsset::Xrp));
        assert_eq!(parse_asset("UNKNOWN"), None);
    }

    #[test]
    fn test_backtest_result() {
        let stats = BacktestStats {
            orders_placed: 100,
            orders_filled: 90,
            orders_partial: 5,
            orders_rejected: 5,
            volume_traded: dec!(5000),
            fees_paid: dec!(5),
            realized_pnl: dec!(250),
        };

        let metrics = crate::state::MetricsSnapshot {
            events_processed: 10000,
            opportunities_detected: 100,
            trades_executed: 95,
            trades_failed: 0,
            trades_skipped: 5,
            pnl_usdc: dec!(250),
            volume_usdc: dec!(5000),
            shadow_orders_fired: 0,
            shadow_orders_filled: 0,
        };

        let result = BacktestResult::new(
            Utc::now() - chrono::Duration::hours(24),
            Utc::now(),
            dec!(10000),
            dec!(10250),
            &stats,
            &metrics,
            60.5,
        );

        assert_eq!(result.total_pnl, dec!(250));
        assert_eq!(result.return_pct, dec!(2.5));
        assert_eq!(result.trades_executed, 95);
    }

    #[test]
    fn test_pnl_report_format() {
        let result = BacktestResult {
            start_time: Utc::now() - chrono::Duration::hours(24),
            end_time: Utc::now(),
            initial_balance: dec!(10000),
            final_balance: dec!(10250),
            total_pnl: dec!(250),
            return_pct: dec!(2.5),
            trades_executed: 95,
            trades_rejected: 5,
            volume_traded: dec!(5000),
            fees_paid: dec!(5),
            opportunities_detected: 100,
            opportunities_skipped: 5,
            win_rate: None,
            sharpe_ratio: None,
            max_drawdown: None,
            parameters: HashMap::new(),
            duration_secs: 60.5,
        };

        let report = PnLReport {
            result,
            positions: vec![],
        };

        let output = report.to_string_report();
        assert!(output.contains("BACKTEST P&L REPORT"));
        assert!(output.contains("Total P&L"));
        assert!(output.contains("$250.00"));
    }

    #[test]
    fn test_sweep_combinations() {
        let config = BacktestModeConfig {
            sweep_enabled: true,
            sweep_params: vec![
                SweepParameter::new("param_a", 1.0, 2.0, 1.0),
                SweepParameter::new("param_b", 0.5, 1.0, 0.5),
            ],
            ..Default::default()
        };

        let mode = BacktestMode {
            config,
            state: Arc::new(GlobalState::new()),
            shutdown_tx: broadcast::channel(16).0,
            clickhouse: ClickHouseClient::with_defaults(),
            sweep_results: Vec::new(),
        };

        let combinations = mode.generate_sweep_combinations();
        // 2 values for param_a * 2 values for param_b = 4 combinations
        assert_eq!(combinations.len(), 4);
    }

    #[test]
    fn test_backtest_mode_requires_backtest_trading_mode() {
        let config = BacktestModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Live; // Wrong mode

        let result = BacktestMode::new(config, &bot_config, ClickHouseClient::with_defaults());
        assert!(result.is_err());
    }

    #[test]
    fn test_backtest_mode_accepts_backtest_mode() {
        let config = BacktestModeConfig::default();
        let mut bot_config = BotConfig::default();
        bot_config.mode = TradingMode::Backtest;

        let result = BacktestMode::new(config, &bot_config, ClickHouseClient::with_defaults());
        assert!(result.is_ok());
    }
}
