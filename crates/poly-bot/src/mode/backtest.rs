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
use tracing::{error, info, warn};

use poly_common::types::CryptoAsset;
use poly_common::ClickHouseClient;

use crate::config::{BacktestConfig, BotConfig, EnginesConfig, ObservabilityConfig, TradingMode};
use crate::data_source::csv_replay::{CsvReplayConfig, CsvReplayDataSource};
use crate::data_source::replay::{ReplayConfig, ReplayDataSource};
use crate::data_source::vec_replay::VecReplayDataSource;
use crate::data_source::{DataSource, MarketEvent};
use crate::executor::simulated::{SimulatedExecutor, SimulatedExecutorConfig, SimulatedStats};
use crate::observability::{
    create_shared_analyzer, create_shared_detector_with_capture, AnomalyConfig, CaptureConfig,
    CounterfactualConfig, InMemoryIdLookup, ObservabilityCapture, ProcessorConfig,
};
use crate::state::GlobalState;
use crate::strategy::{StrategyConfig, StrategyLoop};

/// Configuration for backtest mode.
#[derive(Debug, Clone)]
pub struct BacktestModeConfig {
    /// Data source configuration (for ClickHouse replay).
    pub data_source: ReplayConfig,
    /// CSV data directory (if set, uses CSV instead of ClickHouse).
    pub csv_data_dir: Option<String>,
    /// Executor configuration.
    pub executor: SimulatedExecutorConfig,
    /// Strategy configuration.
    pub strategy: StrategyConfig,
    /// Engines configuration (arbitrage, directional, maker).
    pub engines: EnginesConfig,
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
    /// Number of parallel workers for sweep mode (0 = number of CPU cores).
    pub sweep_parallel_workers: usize,
    /// Path to write decision log (for comparing live vs backtest behavior).
    pub decision_log_path: Option<String>,
    /// Maximum duration to replay from first event (e.g., "2h" limits to 2 hours of data).
    pub max_duration: Option<chrono::Duration>,
    /// Minimum interval between orderbook snapshots per event (ms). Downsamples L2 data.
    pub book_interval_ms: Option<u64>,
    /// Skip session directory creation (used by sweep runner to avoid hundreds of dirs).
    pub skip_session_dir: bool,
}

impl Default for BacktestModeConfig {
    fn default() -> Self {
        Self {
            data_source: ReplayConfig::default(),
            csv_data_dir: None,
            executor: SimulatedExecutorConfig::backtest(),
            strategy: StrategyConfig::default(),
            engines: EnginesConfig::default(),
            observability: ObservabilityConfig::default(),
            initial_balance: dec!(10000),
            shutdown_timeout_secs: 30,
            sweep_enabled: false,
            sweep_params: Vec::new(),
            sweep_parallel_workers: 0, // 0 = use number of CPU cores
            decision_log_path: None,
            max_duration: None,
            book_interval_ms: None,
            skip_session_dir: false,
        }
    }
}

impl BacktestModeConfig {
    /// Create config from BotConfig.
    pub fn from_bot_config(config: &BotConfig, _clickhouse: &ClickHouseClient) -> Result<Self> {
        let mut executor_config = SimulatedExecutorConfig::backtest();
        executor_config.initial_balance = config.effective_trading_config().available_balance;
        executor_config.fee_rate = Decimal::ZERO; // Not used when use_realistic_fees=true
        executor_config.latency_ms = config.backtest.fill_delay_ms;
        executor_config.enforce_balance = true;
        executor_config.max_market_exposure = config.effective_trading_config().max_market_exposure;
        executor_config.max_total_exposure = config.effective_trading_config().max_total_exposure;
        executor_config.min_fill_ratio = dec!(0.5);
        // Set taker mode based on execution mode from config
        executor_config.is_taker_mode = matches!(
            config.execution.execution_mode,
            crate::config::ExecutionMode::Market
        );
        // Only 15-minute markets have fees/rebates; 5-min and 1-hour are fee-free
        // zero_fees override allows testing strategies as if they were on fee-free markets
        executor_config.use_realistic_fees = !config.backtest.zero_fees && matches!(
            config.window_duration,
            poly_common::WindowDuration::FifteenMin
        );

        // Parse date range from config
        let (start_time, end_time, max_duration) = parse_date_range(&config.backtest)?;

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

        let mut strategy = StrategyConfig::from_trading_config(config.effective_trading_config(), (config.window_duration.minutes() * 60) as i64)
            .with_execution(config.execution.clone())
            .with_oracle(&config.oracle);

        // Apply directional engine overrides from TOML (if set)
        let dir = &config.engines.directional;
        if let Some(v) = dir.time_conf_floor { strategy.time_conf_floor = v; }
        if let Some(v) = dir.dist_conf_floor { strategy.dist_conf_floor = v; }
        if let Some(v) = dir.dist_conf_per_atr { strategy.dist_conf_per_atr = v; }
        if let Some(v) = dir.max_edge_factor { strategy.max_edge_factor = v; }
        if let Some(v) = dir.kelly_fraction { strategy.kelly_fraction = v; }
        if let Some(v) = dir.early_exit_enabled { strategy.early_exit_enabled = v; }

        Ok(Self {
            data_source: replay_config,
            csv_data_dir: config.backtest.data_dir.clone(),
            executor: executor_config,
            strategy,
            engines: config.engines.clone(),
            observability: config.observability.clone(),
            initial_balance: config.effective_trading_config().available_balance,
            shutdown_timeout_secs: 30,
            sweep_enabled: config.backtest.sweep_enabled,
            sweep_params: Vec::new(),
            sweep_parallel_workers: config.backtest.sweep_parallel_workers.unwrap_or(0),
            decision_log_path: config.backtest.decision_log_path.clone(),
            max_duration,
            book_interval_ms: config.backtest.book_interval_ms,
            skip_session_dir: false,
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

    /// Force a specific data source ("csv" or "clickhouse").
    /// If "csv" is specified but no data_dir is configured, returns an error.
    /// If "clickhouse" is specified, clears csv_data_dir to force ClickHouse usage.
    pub fn with_data_source(mut self, source: &str) -> Result<Self> {
        match source {
            "csv" => {
                if self.csv_data_dir.is_none() {
                    anyhow::bail!("--data-source csv requires data_dir to be set in config");
                }
                // Already using CSV, nothing to change
            }
            "clickhouse" => {
                // Force ClickHouse by clearing CSV path
                self.csv_data_dir = None;
            }
            _ => {
                anyhow::bail!("Invalid data source '{}', must be 'csv' or 'clickhouse'", source);
            }
        }
        Ok(self)
    }
}

/// Parameter to sweep during backtesting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepParameter {
    /// Parameter name.
    pub name: String,
    /// Starting value (used with end/step for uniform ranges).
    pub start: f64,
    /// Ending value.
    pub end: f64,
    /// Step size.
    pub step: f64,
    /// Explicit values list (overrides start/end/step if non-empty).
    #[serde(default)]
    pub explicit_values: Vec<f64>,
}

impl SweepParameter {
    /// Create a new sweep parameter with uniform range.
    pub fn new(name: &str, start: f64, end: f64, step: f64) -> Self {
        Self {
            name: name.to_string(),
            start,
            end,
            step,
            explicit_values: Vec::new(),
        }
    }

    /// Create a sweep parameter with explicit values.
    pub fn from_values(name: &str, values: Vec<f64>) -> Self {
        let start = values.first().copied().unwrap_or(0.0);
        let end = values.last().copied().unwrap_or(0.0);
        Self {
            name: name.to_string(),
            start,
            end,
            step: 0.0,
            explicit_values: values,
        }
    }

    /// Generate all values for this parameter.
    pub fn values(&self) -> Vec<f64> {
        if !self.explicit_values.is_empty() {
            return self.explicit_values.clone();
        }
        let mut values = Vec::new();
        let mut current = self.start;
        while current <= self.end + self.step * 0.01 {
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
    /// Total fees paid (taker fees).
    pub fees_paid: Decimal,
    /// Total rebates earned (maker rebates).
    pub rebates_earned: Decimal,
    /// Opportunities detected.
    pub opportunities_detected: u64,
    /// Opportunities skipped.
    pub opportunities_skipped: u64,
    /// Win rate (if applicable).
    pub win_rate: Option<Decimal>,
    /// Profit factor (gross profit / gross loss).
    pub profit_factor: Option<f64>,
    /// Sharpe ratio (if applicable).
    pub sharpe_ratio: Option<f64>,
    /// Sortino ratio (like Sharpe, but only penalizes downside volatility).
    pub sortino_ratio: Option<f64>,
    /// Calmar ratio (annualized return / max drawdown).
    pub calmar_ratio: Option<f64>,
    /// Maximum drawdown.
    pub max_drawdown: Option<Decimal>,
    /// Maximum drawdown duration in seconds.
    pub max_drawdown_duration_secs: Option<i64>,
    /// Parameters used (for sweep mode).
    pub parameters: HashMap<String, f64>,
    /// Execution duration.
    pub duration_secs: f64,
    /// Git commit hash at time of backtest.
    pub git_commit: Option<String>,
}

impl BacktestResult {
    /// Create a new backtest result.
    pub fn new(
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        initial_balance: Decimal,
        final_balance: Decimal,
        stats: &SimulatedStats,
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
            // Use metrics.trades_executed since executor stats may not be accessible
            trades_executed: metrics.trades_executed,
            trades_rejected: metrics.trades_failed,
            volume_traded: metrics.volume_usdc,
            fees_paid: stats.fees_paid,
            rebates_earned: stats.rebates_earned,
            opportunities_detected: metrics.opportunities_detected,
            opportunities_skipped: metrics.trades_skipped,
            win_rate: stats.win_rate(),
            profit_factor: stats.profit_factor(),
            sharpe_ratio: stats.sharpe_ratio(),
            sortino_ratio: stats.sortino_ratio(),
            calmar_ratio: stats.calmar_ratio(),
            max_drawdown: stats.max_drawdown_pct(),
            max_drawdown_duration_secs: stats.max_drawdown_duration(),
            parameters: HashMap::new(),
            duration_secs,
            git_commit: std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        String::from_utf8(o.stdout)
                            .ok()
                            .map(|s| s.trim().to_string())
                    } else {
                        None
                    }
                }),
        }
    }

    /// Add a parameter value (for sweep results).
    pub fn with_parameter(mut self, name: &str, value: f64) -> Self {
        self.parameters.insert(name.to_string(), value);
        self
    }

    /// Save backtest results to a directory under `backtests/`.
    ///
    /// Creates `backtests/<timestamp>/` containing:
    /// - `result.json` - full backtest result (machine-readable)
    /// - `report.txt` - human-readable P&L report
    ///
    /// Returns the path to the created directory.
    pub fn save(&self) -> Result<std::path::PathBuf> {
        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let dir = std::path::PathBuf::from("backtests").join(timestamp.to_string());
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create backtest output dir: {:?}", dir))?;

        // Write machine-readable JSON result
        let json = serde_json::to_string_pretty(self)
            .context("Failed to serialize backtest result")?;
        let json_path = dir.join("result.json");
        std::fs::write(&json_path, json)
            .with_context(|| format!("Failed to write {:?}", json_path))?;

        // Write human-readable report
        let report = PnLReport {
            result: self.clone(),
            positions: Vec::new(),
        };
        let report_path = dir.join("report.txt");
        std::fs::write(&report_path, report.to_string_report())
            .with_context(|| format!("Failed to write {:?}", report_path))?;

        Ok(dir)
    }

    /// Save sweep results (multiple runs) to a directory under `backtests/`.
    ///
    /// Creates `backtests/sweep_<timestamp>/` containing:
    /// - `sweep_results.json` - all sweep results
    /// - `best_result.json` - best result by P&L
    /// - `report.txt` - human-readable best result report
    pub fn save_sweep(results: &[BacktestResult], best: &BacktestResult) -> Result<std::path::PathBuf> {
        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let dir = std::path::PathBuf::from("backtests").join(format!("sweep_{}", timestamp));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create sweep output dir: {:?}", dir))?;

        // Write all sweep results
        let json = serde_json::to_string_pretty(results)
            .context("Failed to serialize sweep results")?;
        let json_path = dir.join("sweep_results.json");
        std::fs::write(&json_path, json)
            .with_context(|| format!("Failed to write {:?}", json_path))?;

        // Write best result
        let best_json = serde_json::to_string_pretty(best)
            .context("Failed to serialize best result")?;
        let best_path = dir.join("best_result.json");
        std::fs::write(&best_path, best_json)
            .with_context(|| format!("Failed to write {:?}", best_path))?;

        // Write human-readable report for best
        let report = PnLReport {
            result: best.clone(),
            positions: Vec::new(),
        };
        let report_path = dir.join("report.txt");
        std::fs::write(&report_path, report.to_string_report())
            .with_context(|| format!("Failed to write {:?}", report_path))?;

        Ok(dir)
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
            self.result.start_time.format("%Y-%m-%d %H:%M:%S"),
            self.result.end_time.format("%Y-%m-%d %H:%M:%S")
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

        report.push_str("─── RISK METRICS ──────────────────────────────────────────\n");
        if let Some(win_rate) = self.result.win_rate {
            report.push_str(&format!(
                "Win Rate:             {:.1}%\n",
                win_rate * dec!(100)
            ));
        } else {
            report.push_str("Win Rate:             N/A (no settled markets)\n");
        }
        if let Some(pf) = self.result.profit_factor {
            if pf.is_infinite() {
                report.push_str("Profit Factor:        Inf (no losses)\n");
            } else {
                report.push_str(&format!(
                    "Profit Factor:        {:.2}\n",
                    pf
                ));
            }
        } else {
            report.push_str("Profit Factor:        N/A\n");
        }
        if let Some(sharpe) = self.result.sharpe_ratio {
            report.push_str(&format!(
                "Sharpe Ratio:         {:.2}\n",
                sharpe
            ));
        } else {
            report.push_str("Sharpe Ratio:         N/A (insufficient data)\n");
        }
        if let Some(sortino) = self.result.sortino_ratio {
            report.push_str(&format!(
                "Sortino Ratio:        {:.2}\n",
                sortino
            ));
        } else {
            report.push_str("Sortino Ratio:        N/A\n");
        }
        if let Some(calmar) = self.result.calmar_ratio {
            report.push_str(&format!(
                "Calmar Ratio:         {:.2}\n",
                calmar
            ));
        } else {
            report.push_str("Calmar Ratio:         N/A\n");
        }
        if let Some(max_dd) = self.result.max_drawdown {
            report.push_str(&format!(
                "Max Drawdown:         {:.2}%\n",
                max_dd
            ));
        } else {
            report.push_str("Max Drawdown:         N/A\n");
        }
        if let Some(dd_secs) = self.result.max_drawdown_duration_secs {
            // Format duration nicely
            let mins = dd_secs / 60;
            if mins < 60 {
                report.push_str(&format!(
                    "Max DD Duration:      {} min\n\n",
                    mins
                ));
            } else {
                let hours = mins / 60;
                let remaining_mins = mins % 60;
                report.push_str(&format!(
                    "Max DD Duration:      {}h {}m\n\n",
                    hours, remaining_mins
                ));
            }
        } else {
            report.push_str("Max DD Duration:      N/A\n\n");
        }

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
            "Fees Paid:            ${:.4}\n",
            self.result.fees_paid
        ));
        report.push_str(&format!(
            "Rebates Earned:       ${:.4}\n",
            self.result.rebates_earned
        ));
        report.push_str(&format!(
            "Net Fees:             ${:.4}\n\n",
            self.result.fees_paid - self.result.rebates_earned
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

        if let Some(commit) = &self.result.git_commit {
            report.push_str(&format!("Git Commit: {}\n", commit));
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


/// Standalone backtest task for parallel sweep execution.
///
/// This is a blocking function that runs a complete backtest synchronously.
/// It creates its own tokio runtime internally to run async strategy code.
fn run_backtest_task_blocking(
    events: Arc<Vec<MarketEvent>>,
    config: BacktestModeConfig,
    warmup_prices: Arc<HashMap<CryptoAsset, Vec<Decimal>>>,
) -> Result<BacktestResult> {
    let start_instant = std::time::Instant::now();

    // Create a dedicated runtime for this backtest
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create runtime: {}", e))?;

    // Run the backtest on this runtime
    rt.block_on(async {
        // Create fresh state for this run
        let state = Arc::new(GlobalState::new());

        // Create data source from cached events
        let data_source = VecReplayDataSource::new(events, config.data_source.speed);

        // Create executor
        let mut executor_config = config.executor.clone();
        executor_config.initial_balance = config.initial_balance;
        let executor = SimulatedExecutor::new(executor_config);

        // Create strategy loop (no observability for sweep runs - too expensive)
        // Set is_backtest=true to disable price chasing
        let mut strategy = StrategyLoop::with_engines(
            data_source,
            executor,
            state.clone(),
            config.strategy.clone().with_backtest(true),
            config.engines.clone(),
        );

        // Warm up ATR with pre-loaded prices (matching live/backtest behavior)
        for (asset, prices) in warmup_prices.as_ref() {
            if !prices.is_empty() {
                strategy.warmup_atr(*asset, prices);
            }
        }

        // Enable trading and run
        state.enable_trading();

        let run_result = strategy.run().await;

        // Get final metrics and simulation stats
        let final_balance = strategy.available_balance();
        let metrics = strategy.metrics();
        let simulation_stats = strategy.simulation_stats().unwrap_or_default();

        // Get actual event time range
        let (first_event, last_event) = strategy.event_time_range();
        let actual_start = first_event.unwrap_or(config.data_source.start_time);
        let actual_end = last_event
            .or(first_event)
            .unwrap_or_else(Utc::now);

        // Shutdown strategy
        strategy.shutdown().await;

        // Check for errors
        match run_result {
            Ok(()) => {}
            Err(crate::strategy::StrategyError::Shutdown) => {}
            Err(e) => {
                return Err(anyhow::anyhow!("Strategy error: {}", e));
            }
        }

        // Build result using actual simulation stats
        let duration_secs = start_instant.elapsed().as_secs_f64();

        Ok(BacktestResult::new(
            actual_start,
            actual_end,
            config.initial_balance,
            final_balance,
            &simulation_stats,
            &metrics,
            duration_secs,
        ))
    })
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
    ///
    /// Uses the same StrategyLoop as paper/live modes to ensure backtest
    /// results accurately reflect real trading behavior.
    async fn run_single(&mut self) -> Result<BacktestResult> {
        // Dispatch to concrete implementation based on data source type
        if let Some(ref data_dir) = self.config.csv_data_dir {
            self.run_with_csv_source(data_dir.clone()).await
        } else {
            self.run_with_clickhouse_source().await
        }
    }

    /// Run backtest with CSV data source.
    async fn run_with_csv_source(&mut self, data_dir: String) -> Result<BacktestResult> {
        let start_instant = std::time::Instant::now();

        info!("Starting backtest mode");

        // Reset state for a fresh backtest run.
        // We reset the state's internal values (market_data, control, metrics) rather than
        // creating a new GlobalState, because the signal handler holds a reference to self.state.
        // If we created a new GlobalState, Ctrl+C would set shutdown on the old state.
        self.state.reset();

        // Create CSV data source
        let shutdown_rx = self.shutdown_tx.subscribe();
        info!(
            data_dir = %data_dir,
            speed = self.config.data_source.speed,
            initial_balance = %self.config.initial_balance,
            max_duration = ?self.config.max_duration.map(|d| format!("{}s", d.num_seconds())),
            "Using CSV data source"
        );

        // Load warmup prices for ATR (5 minutes before start)
        let warmup_duration = chrono::Duration::minutes(5);
        let warmup_prices = CsvReplayDataSource::load_warmup_prices(
            &data_dir,
            self.config.data_source.start_time,
            warmup_duration,
            &self.config.data_source.assets,
        );

        let csv_config = CsvReplayConfig {
            data_dir,
            start_time: Some(self.config.data_source.start_time),
            end_time: Some(self.config.data_source.end_time),
            assets: self.config.data_source.assets.clone(),
            speed: self.config.data_source.speed,
            max_duration: self.config.max_duration,
            book_interval_ms: self.config.book_interval_ms,
        };
        let data_source = CsvReplayDataSource::new(csv_config);

        // Create backtest executor (using SimulatedExecutor in backtest mode)
        let mut executor_config = self.config.executor.clone();
        executor_config.initial_balance = self.config.initial_balance;
        let executor = SimulatedExecutor::new(executor_config);

        // Run the common backtest logic with warmup prices
        self.run_backtest_common(data_source, executor, shutdown_rx, start_instant, warmup_prices).await
    }

    /// Run backtest with ClickHouse data source.
    async fn run_with_clickhouse_source(&mut self) -> Result<BacktestResult> {
        let start_instant = std::time::Instant::now();

        info!("Starting backtest mode");

        // Reset state for a fresh backtest run (see run_with_csv_source for full explanation).
        self.state.reset();

        // Create ClickHouse data source
        let shutdown_rx = self.shutdown_tx.subscribe();
        info!(
            start = %self.config.data_source.start_time,
            end = %self.config.data_source.end_time,
            speed = self.config.data_source.speed,
            initial_balance = %self.config.initial_balance,
            "Using ClickHouse data source"
        );

        let data_source = ReplayDataSource::new(self.config.data_source.clone(), self.clickhouse.clone());

        // Create backtest executor (using SimulatedExecutor in backtest mode)
        let mut executor_config = self.config.executor.clone();
        executor_config.initial_balance = self.config.initial_balance;
        let executor = SimulatedExecutor::new(executor_config);

        // Run the common backtest logic (no warmup for ClickHouse - ATR warms up from data)
        let warmup_prices = HashMap::new();
        self.run_backtest_common(data_source, executor, shutdown_rx, start_instant, warmup_prices).await
    }

    /// Common backtest logic that works with any DataSource.
    async fn run_backtest_common<D: DataSource>(
        &mut self,
        data_source: D,
        executor: SimulatedExecutor,
        mut shutdown_rx: broadcast::Receiver<()>,
        start_instant: std::time::Instant,
        warmup_prices: HashMap<CryptoAsset, Vec<Decimal>>,
    ) -> Result<BacktestResult> {
        // Log enabled engines
        let enabled_engines = self.config.engines.enabled_engines();
        info!(
            engines = ?enabled_engines,
            arbitrage = self.config.engines.arbitrage.enabled,
            directional = self.config.engines.directional.enabled,
            maker = self.config.engines.maker.enabled,
            "Trading engines configuration"
        );

        // Set up observability
        let (capture, obs_tasks) = self.setup_observability().await?;

        // Create strategy loop with engines config - uses the SAME strategy as paper/live mode
        // Set is_backtest=true to disable price chasing (simulated executor fills immediately)
        let mut strategy = StrategyLoop::with_engines(
            data_source,
            executor,
            self.state.clone(),
            self.config.strategy.clone().with_backtest(true),
            self.config.engines.clone(),
        );

        // Warm up ATR with prices from before the backtest period
        // This ensures indicators are ready before the first market, matching live behavior
        if !warmup_prices.is_empty() {
            info!("Warming up ATR with {} assets from CSV warmup data", warmup_prices.len());
            for (asset, prices) in &warmup_prices {
                if !prices.is_empty() {
                    strategy.warmup_atr(*asset, prices);
                    info!(
                        "Warmed up ATR for {} with {} prices",
                        asset,
                        prices.len()
                    );
                }
            }
        } else {
            info!("No warmup prices available - ATR will warm up from event data");
        }

        // Create session directory (or use explicit decision_log_path if configured)
        // Sweep runs skip session dir to avoid creating hundreds of directories.
        let session_paths = if let Some(ref path) = self.config.decision_log_path {
            let trades = path.replace("decisions_", "trades_");
            Some((path.clone(), trades))
        } else if !self.config.skip_session_dir {
            let session = super::common::create_session_dir("backtest")
                .expect("Failed to create session directory");
            Some((
                session.decisions.to_str().unwrap_or("decisions.csv").to_string(),
                session.trades.to_str().unwrap_or("trades.csv").to_string(),
            ))
        } else {
            None
        };

        if let Some((ref decisions_path, ref trades_path)) = session_paths {
            // Enable decision logging
            {
                let log_config = crate::strategy::decision_log::DecisionLogConfig {
                    base_order_size: self.config.strategy.sizing_config.base_order_size,
                    min_order_size: self.config.strategy.sizing_config.min_order_size,
                    max_market_exposure: self.config.strategy.sizing_config.max_market_exposure,
                    max_total_exposure: self.config.strategy.sizing_config.max_total_exposure,
                    available_balance: self.config.initial_balance,
                    max_edge_factor: self.config.strategy.max_edge_factor,
                    window_duration_secs: self.config.strategy.window_duration_secs,
                };
                match crate::strategy::decision_log::DecisionLogger::with_config(decisions_path, "backtest", Some(&log_config)) {
                    Ok(logger) => {
                        info!("Decision logging enabled: {}", decisions_path);
                        strategy = strategy.with_decision_logger(logger);
                    }
                    Err(e) => {
                        warn!("Failed to create decision logger at {}: {}", decisions_path, e);
                    }
                }
            }

            // Enable trades logging
            match crate::strategy::trades_log::TradesLogger::new(trades_path, "backtest", self.config.initial_balance) {
                Ok(logger) => {
                    info!("Trades logging enabled: {}", trades_path);
                    strategy = strategy.with_trades_logger(logger);
                }
                Err(e) => {
                    warn!("Failed to create trades logger at {}: {}", trades_path, e);
                }
            }
        }

        // Add observability sender if capture is enabled
        if let Some(ref cap) = capture
            && cap.is_enabled()
        {
            let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
            strategy = strategy.with_observability(tx);

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
        info!("Backtest started - running strategy loop on historical data");

        // Run strategy loop - this is the SAME code path as paper/live mode
        let result = tokio::select! {
            result = strategy.run() => {
                match result {
                    Ok(()) => {
                        info!("Strategy loop completed - all historical data processed");
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
        self.shutdown_strategy(&mut strategy, obs_tasks).await;

        // Get final balance, metrics, and simulation stats
        let final_balance = strategy.available_balance();
        let metrics = strategy.metrics();
        let simulation_stats = strategy.simulation_stats().unwrap_or_default();

        // Get actual event time range (instead of config defaults)
        // This ensures the report shows the real data period, not placeholder config values
        let (first_event, last_event) = strategy.event_time_range();
        let actual_start = first_event.unwrap_or(self.config.data_source.start_time);
        // For end time: prefer actual last event, then first event, then current time
        // (never use far-future config placeholder which would be confusing)
        let actual_end = last_event
            .or(first_event)
            .unwrap_or_else(Utc::now);

        // Generate result using actual simulation stats
        let duration_secs = start_instant.elapsed().as_secs_f64();

        let backtest_result = BacktestResult::new(
            actual_start,
            actual_end,
            self.config.initial_balance,
            final_balance,
            &simulation_stats,
            &metrics,
            duration_secs,
        );

        // Print final report (always visible, regardless of log level)
        let positions: Vec<PositionSummary> = Vec::new(); // Would collect from executor
        let report = PnLReport {
            result: backtest_result.clone(),
            positions,
        };
        println!("\n{}", report.to_string_report());

        // Save results to backtests/ directory
        match backtest_result.save() {
            Ok(dir) => info!("Backtest results saved to {:?}", dir),
            Err(e) => warn!("Failed to save backtest results: {}", e),
        }

        result?;
        Ok(backtest_result)
    }

    /// Run parameter sweep with sequential streaming execution.
    ///
    /// Each run streams CSV from disk independently, using constant memory.
    /// Runs sequentially to avoid loading the entire dataset into memory.
    async fn run_sweep(&mut self) -> Result<BacktestResult> {
        info!("Starting parameter sweep backtest");

        let combinations = self.generate_sweep_combinations();
        let total_runs = combinations.len();

        let data_dir = self.config.csv_data_dir.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Sweep requires CSV data source"))?
            .clone();

        // Load warmup prices once (small)
        let warmup_duration = chrono::Duration::minutes(5);
        let prices = CsvReplayDataSource::load_warmup_prices(
            &data_dir,
            self.config.data_source.start_time,
            warmup_duration,
            &self.config.data_source.assets,
        );
        let warmup_prices = Arc::new(prices);

        // Determine number of workers
        let num_workers = if self.config.sweep_parallel_workers == 0 {
            std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(4)
        } else {
            self.config.sweep_parallel_workers
        };

        eprintln!(
            "Starting sweep: {} parameter combinations, {} parallel workers",
            total_runs, num_workers
        );

        // Load all events into memory ONCE and share across workers via Arc.
        // This avoids 5x disk I/O contention from parallel CSV reads.
        eprintln!("  Loading events into memory...");
        let load_start = std::time::Instant::now();
        let csv_config = CsvReplayConfig {
            data_dir: data_dir.clone(),
            start_time: Some(self.config.data_source.start_time),
            end_time: Some(self.config.data_source.end_time),
            assets: self.config.data_source.assets.clone(),
            speed: 0.0,
            max_duration: self.config.max_duration,
            book_interval_ms: self.config.book_interval_ms,
        };
        let events = CsvReplayDataSource::load_all_events(csv_config)
            .map_err(|e| anyhow::anyhow!("Failed to load events: {}", e))?;
        let events = Arc::new(events);
        eprintln!("  Loaded {} events in {:.1}s", events.len(), load_start.elapsed().as_secs_f64());

        info!(
            "Running {} parameter combinations with {} parallel workers (shared memory)",
            total_runs, num_workers
        );

        // Prepare all run configs
        let mut run_configs: Vec<(usize, HashMap<String, f64>, BacktestModeConfig)> = Vec::new();
        for (run_idx, params) in combinations.into_iter().enumerate() {
            let mut run_config = self.config.clone();
            run_config.decision_log_path = None;
            run_config.skip_session_dir = true;
            for (name, value) in &params {
                apply_parameter(&mut run_config, name, *value);
            }
            run_configs.push((run_idx, params, run_config));
        }

        // Spawn threads that share the same in-memory event data.
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let active_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();

        for (run_idx, params, run_config) in run_configs {
            let events = Arc::clone(&events);
            let warmup = Arc::clone(&warmup_prices);
            let result_tx = result_tx.clone();
            let active_count = Arc::clone(&active_count);
            let total = total_runs;
            let max_workers = num_workers;

            let handle = std::thread::spawn(move || {
                // Spin-wait to limit concurrency
                loop {
                    let current = active_count.load(std::sync::atomic::Ordering::SeqCst);
                    if current < max_workers
                        && active_count
                            .compare_exchange(
                                current,
                                current + 1,
                                std::sync::atomic::Ordering::SeqCst,
                                std::sync::atomic::Ordering::SeqCst,
                            )
                            .is_ok()
                    {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }

                let count = active_count.load(std::sync::atomic::Ordering::SeqCst);
                eprintln!("  Launching run {}/{} (active: {})...", run_idx + 1, total, count);
                info!("Sweep run {}/{} (active: {}): {:?}", run_idx + 1, total, count, params);

                let result = run_backtest_task_blocking(events, run_config, warmup);

                if let Err(ref e) = result {
                    eprintln!("  Run {}/{} ERRORED: {:#}", run_idx + 1, total, e);
                }

                active_count.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);

                let _ = result_tx.send((run_idx, params, result));
            });

            handles.push(handle);
        }

        drop(result_tx);

        // Collect results as they complete
        let mut results: Vec<(usize, BacktestResult)> = Vec::new();
        let mut completed = 0usize;
        let sweep_start = std::time::Instant::now();

        for (run_idx, params, result) in result_rx {
            completed += 1;
            match result {
                Ok(mut res) => {
                    for (name, value) in &params {
                        res = res.with_parameter(name, *value);
                    }
                    let elapsed = sweep_start.elapsed();
                    let per_run = elapsed / completed as u32;
                    let remaining = per_run * (total_runs - completed) as u32;
                    eprintln!(
                        "[{}/{}] Run {}: Return={:.1}%, DD={:.1}%, P&L=${:.0} (elapsed: {}m, ~{}m remaining)",
                        completed, total_runs, run_idx + 1,
                        res.return_pct, res.max_drawdown.unwrap_or_default(),
                        res.total_pnl,
                        elapsed.as_secs() / 60,
                        remaining.as_secs() / 60,
                    );
                    info!(
                        "Run {}/{}: P&L=${:.2}, Return={:.2}%, DD={:.2}%",
                        run_idx + 1, total_runs,
                        res.total_pnl, res.return_pct,
                        res.max_drawdown.unwrap_or_default()
                    );
                    results.push((run_idx, res));
                }
                Err(e) => {
                    eprintln!("[{}/{}] Run {} FAILED: {}", completed, total_runs, run_idx + 1, e);
                    warn!("Sweep run {}/{} failed: {}", run_idx + 1, total_runs, e);
                }
            }
        }

        for handle in handles {
            let _ = handle.join();
        }

        results.sort_by_key(|(idx, _)| *idx);

        // Find best result and collect all results
        let mut best_result: Option<BacktestResult> = None;
        let mut best_pnl = Decimal::MIN;

        for (_, res) in results {
            if res.total_pnl > best_pnl {
                best_pnl = res.total_pnl;
                best_result = Some(res.clone());
            }
            self.sweep_results.push(res);
        }

        // Save sweep results to backtests/ directory
        if let Some(ref best) = best_result {
            match BacktestResult::save_sweep(&self.sweep_results, best) {
                Ok(dir) => info!("Sweep results saved to {:?}", dir),
                Err(e) => warn!("Failed to save sweep results: {}", e),
            }
        }

        // Return best result or error
        if best_result.is_none() {
            eprintln!("ERROR: All {} sweep runs failed! No results collected.", total_runs);
        }
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

    /// Perform graceful shutdown of strategy loop.
    async fn shutdown_strategy<D: DataSource, E: crate::executor::Executor>(
        &self,
        strategy: &mut StrategyLoop<D, E>,
        obs_tasks: Vec<tokio::task::JoinHandle<()>>,
    ) {
        info!("Beginning backtest shutdown");

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

        info!("Backtest shutdown complete");
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
    /// Request shutdown.
    pub fn request_shutdown(&self) {
        info!("Backtest shutdown requested");
        self.state.control.request_shutdown();
        let _ = self.shutdown_tx.send(());
    }
}

/// Parse date range from BacktestConfig.
/// If dates are not specified, uses a wide range to include all data.
/// Returns (start, end, max_duration) where max_duration is set when
/// duration is specified without a start_date (deferred cutoff).
fn parse_date_range(config: &BacktestConfig) -> Result<(DateTime<Utc>, DateTime<Utc>, Option<chrono::Duration>)> {
    // Parse duration if provided
    let parsed_duration = if let Some(ref dur_str) = config.duration {
        Some(parse_duration(dur_str).context("Invalid duration")?)
    } else {
        None
    };

    let start = if let Some(ref date_str) = config.start_date {
        parse_date(date_str).context("Invalid start_date")?
    } else {
        // Default to far past to include all data
        DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    };

    let (end, max_duration) = if let Some(ref date_str) = config.end_date {
        // Explicit end_date takes priority
        let end = parse_date(date_str)
            .context("Invalid end_date")?
            + chrono::Duration::hours(23)
            + chrono::Duration::minutes(59)
            + chrono::Duration::seconds(59);
        (end, None)
    } else if let (Some(dur), true) = (parsed_duration, config.start_date.is_some()) {
        // start_date + duration: compute end upfront
        (start + dur, None)
    } else if let Some(dur) = parsed_duration {
        // duration only (no start_date): defer cutoff to data source
        let end = DateTime::parse_from_rfc3339("2030-12-31T23:59:59Z")
            .unwrap()
            .with_timezone(&Utc);
        (end, Some(dur))
    } else {
        // No end_date, no duration: include all data
        let end = DateTime::parse_from_rfc3339("2030-12-31T23:59:59Z")
            .unwrap()
            .with_timezone(&Utc);
        (end, None)
    };

    if start >= end {
        anyhow::bail!("start_date must be before end_date");
    }

    Ok((start, end, max_duration))
}

/// Parse a human-readable duration string (e.g., "30m", "2h", "1d").
fn parse_duration(s: &str) -> Result<chrono::Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("Empty duration string");
    }

    let (num_str, suffix) = s.split_at(s.len() - 1);
    let value: i64 = num_str.parse()
        .with_context(|| format!("Invalid duration number: '{}'", num_str))?;

    if value <= 0 {
        anyhow::bail!("Duration must be positive, got: {}", value);
    }

    match suffix {
        "m" => Ok(chrono::Duration::minutes(value)),
        "h" => Ok(chrono::Duration::hours(value)),
        "d" => Ok(chrono::Duration::days(value)),
        _ => anyhow::bail!("Unknown duration suffix '{}'. Use 'm' (minutes), 'h' (hours), or 'd' (days)", suffix),
    }
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
        // Arbitrage thresholds
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
        // Sizing
        "max_market_exposure" => {
            config.strategy.sizing_config.max_market_exposure =
                Decimal::from_f64_retain(value).unwrap_or_default();
        }
        "base_order_size" => {
            config.strategy.sizing_config.base_order_size =
                Decimal::from_f64_retain(value).unwrap_or_default();
        }
        "min_fill_ratio" => {
            config.executor.min_fill_ratio = Decimal::from_f64_retain(value).unwrap_or_default();
        }
        // Confidence calculation params
        "time_conf_floor" => {
            config.strategy.time_conf_floor =
                Decimal::from_f64_retain(value).unwrap_or_default();
        }
        "dist_conf_floor" => {
            config.strategy.dist_conf_floor =
                Decimal::from_f64_retain(value).unwrap_or_default();
        }
        "dist_conf_per_atr" => {
            config.strategy.dist_conf_per_atr =
                Decimal::from_f64_retain(value).unwrap_or_default();
        }
        // Edge requirement (quality filter)
        "max_edge_factor" => {
            config.strategy.max_edge_factor =
                Decimal::from_f64_retain(value).unwrap_or_default();
        }
        // Minimum share price (OTM filter)
        "min_share_price" => {
            config.engines.directional.min_share_price =
                Some(Decimal::from_f64_retain(value).unwrap_or_default());
        }
        // HTF trend params
        "htf_trend_confirm_boost" => {
            let v = Decimal::from_f64_retain(value).unwrap_or_default();
            config.strategy.htf_trend_confirm_boost = v;
        }
        "htf_trend_oppose_cut" => {
            let v = Decimal::from_f64_retain(value).unwrap_or_default();
            config.strategy.htf_trend_oppose_cut = v;
        }
        // Maximum share price (risk/reward filter)
        "max_share_price" => {
            config.engines.directional.max_share_price =
                Some(Decimal::from_f64_retain(value).unwrap_or_default());
        }
        // Kelly fraction (position sizing)
        "kelly_fraction" => {
            let v = Decimal::from_f64_retain(value).unwrap_or_default();
            config.engines.directional.kelly_fraction = Some(v);
            config.strategy.kelly_fraction = v;
        }
        // Early exit on signal reversal
        "early_exit_enabled" => {
            let enabled = value > 0.5;
            config.engines.directional.early_exit_enabled = Some(enabled);
            config.strategy.early_exit_enabled = enabled;
        }
        // Signal thresholds
        "strong_threshold_late_bps" => {
            config.engines.directional.strong_threshold_late_bps = value as u32;
        }
        "lean_threshold_late_bps" => {
            config.engines.directional.lean_threshold_late_bps = value as u32;
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
        let stats = SimulatedStats {
            orders_placed: 100,
            orders_filled: 90,
            orders_partial: 5,
            orders_rejected: 5,
            volume_traded: dec!(5000),
            fees_paid: dec!(5),
            ..Default::default()
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
            allocated_balance: dec!(10000),
            current_balance: dec!(10250),
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
            rebates_earned: Decimal::ZERO,
            opportunities_detected: 100,
            opportunities_skipped: 5,
            win_rate: None,
            profit_factor: None,
            sharpe_ratio: None,
            sortino_ratio: None,
            calmar_ratio: None,
            max_drawdown: None,
            max_drawdown_duration_secs: None,
            parameters: HashMap::new(),
            duration_secs: 60.5,
            git_commit: Some("abc1234".to_string()),
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
    fn test_parse_duration_minutes() {
        let dur = parse_duration("30m").unwrap();
        assert_eq!(dur.num_minutes(), 30);
    }

    #[test]
    fn test_parse_duration_hours() {
        let dur = parse_duration("2h").unwrap();
        assert_eq!(dur.num_hours(), 2);
        assert_eq!(dur.num_minutes(), 120);
    }

    #[test]
    fn test_parse_duration_days() {
        let dur = parse_duration("1d").unwrap();
        assert_eq!(dur.num_days(), 1);
    }

    #[test]
    fn test_parse_duration_invalid() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("2x").is_err());
        assert!(parse_duration("0h").is_err());
        assert!(parse_duration("-1h").is_err());
    }

    #[test]
    fn test_parse_date_range_duration_only() {
        let config = BacktestConfig {
            duration: Some("2h".to_string()),
            ..Default::default()
        };
        let (start, _end, max_dur) = parse_date_range(&config).unwrap();
        // No start_date: should get wide range + deferred max_duration
        assert!(max_dur.is_some());
        assert_eq!(max_dur.unwrap().num_hours(), 2);
        assert_eq!(start.format("%Y").to_string(), "2020"); // far past default
    }

    #[test]
    fn test_parse_date_range_start_plus_duration() {
        let config = BacktestConfig {
            start_date: Some("2026-02-01".to_string()),
            duration: Some("4h".to_string()),
            ..Default::default()
        };
        let (start, end, max_dur) = parse_date_range(&config).unwrap();
        // start_date + duration: end computed upfront, no deferred duration
        assert!(max_dur.is_none());
        assert_eq!(start.format("%Y-%m-%d").to_string(), "2026-02-01");
        assert_eq!((end - start).num_hours(), 4);
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
