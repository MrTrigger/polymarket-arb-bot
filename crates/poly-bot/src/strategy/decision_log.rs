//! Decision logging for comparing live vs backtest behavior.
//!
//! Writes all trading decisions to a CSV file with all relevant inputs
//! so we can diff live vs backtest runs.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use poly_common::CryptoAsset;

use super::position::SkipReason;
use super::signal::Signal;

/// A single trading decision with all inputs that led to it.
#[derive(Debug, Clone)]
pub struct DecisionRecord {
    /// Timestamp of the decision.
    pub timestamp: DateTime<Utc>,
    /// Mode: "live", "paper", or "backtest".
    pub mode: String,
    /// Market event ID.
    pub event_id: String,
    /// Asset being traded.
    pub asset: CryptoAsset,
    /// Current spot price.
    pub spot_price: Decimal,
    /// Strike price of the market.
    pub strike_price: Decimal,
    /// Distance from strike (as ratio).
    pub distance: Decimal,
    /// Distance in dollars.
    pub distance_dollars: Decimal,
    /// ATR value.
    pub atr: Decimal,
    /// Distance as ATR multiple.
    pub atr_multiple: Decimal,
    /// Seconds remaining in window.
    pub seconds_remaining: i64,
    /// Current phase (Early/Build/Core/Final).
    pub phase: String,
    /// Signal (StrongUp/LeanUp/Neutral/LeanDown/StrongDown).
    pub signal: Signal,
    /// YES token ask price.
    pub yes_price: Decimal,
    /// NO token ask price.
    pub no_price: Decimal,
    /// Base confidence (before pivot adjustment).
    pub base_confidence: Decimal,
    /// Pivot multiplier applied.
    pub pivot_multiplier: Decimal,
    /// Final confidence (after pivot).
    pub confidence: Decimal,
    /// Expected value (confidence - favorable_price).
    pub ev: Decimal,
    /// Minimum edge required.
    pub min_edge: Decimal,
    /// Favorable price for the signal direction.
    pub favorable_price: Decimal,
    /// Decision outcome.
    pub action: String,
    /// Skip reason if skipped.
    pub skip_reason: Option<String>,
    /// Trade size if executed.
    pub trade_size: Option<Decimal>,
    /// UP allocation ratio.
    pub up_ratio: Option<Decimal>,
}

impl DecisionRecord {
    /// Convert to CSV row.
    pub fn to_csv_row(&self) -> String {
        format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            self.timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
            self.mode,
            self.event_id,
            self.asset,
            self.spot_price,
            self.strike_price,
            self.distance,
            self.distance_dollars,
            self.atr,
            self.atr_multiple,
            self.seconds_remaining,
            self.phase,
            format!("{:?}", self.signal),
            self.yes_price,
            self.no_price,
            self.base_confidence,
            self.pivot_multiplier,
            self.confidence,
            self.ev,
            self.min_edge,
            self.favorable_price,
            self.action,
            self.skip_reason.as_deref().unwrap_or(""),
            self.trade_size.map(|s| s.to_string()).unwrap_or_default(),
            self.up_ratio.map(|r| r.to_string()).unwrap_or_default(),
        )
    }

    /// CSV header row.
    pub fn csv_header() -> &'static str {
        "timestamp,mode,event_id,asset,spot_price,strike_price,distance,distance_dollars,atr,atr_multiple,seconds_remaining,phase,signal,yes_price,no_price,base_confidence,pivot_multiplier,confidence,ev,min_edge,favorable_price,action,skip_reason,trade_size,up_ratio\n"
    }
}

/// Configuration snapshot for decision log header.
#[derive(Debug, Clone)]
pub struct DecisionLogConfig {
    pub base_order_size: Decimal,
    pub min_order_size: Decimal,
    pub max_market_exposure: Decimal,
    pub max_total_exposure: Decimal,
    pub available_balance: Decimal,
    pub max_edge_factor: Decimal,
    pub window_duration_secs: i64,
    pub strong_up_ratio: Decimal,
    pub lean_up_ratio: Decimal,
}

/// Logger that writes decisions to CSV.
pub struct DecisionLogger {
    file: Mutex<File>,
    mode: String,
}

impl DecisionLogger {
    /// Create a new decision logger.
    pub fn new(path: impl AsRef<Path>, mode: &str) -> std::io::Result<Self> {
        Self::with_config(path, mode, None)
    }

    /// Create a new decision logger with config header.
    pub fn with_config(
        path: impl AsRef<Path>,
        mode: &str,
        config: Option<&DecisionLogConfig>,
    ) -> std::io::Result<Self> {
        let path = path.as_ref();
        let file_exists = path.exists();

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        let logger = Self {
            file: Mutex::new(file),
            mode: mode.to_string(),
        };

        // Write config header and CSV header if new file
        if !file_exists {
            if let Some(cfg) = config {
                logger.write_config_header(cfg)?;
            }
            logger.write_header()?;
        }

        Ok(logger)
    }

    fn write_config_header(&self, config: &DecisionLogConfig) -> std::io::Result<()> {
        let mut file = self.file.lock().unwrap();
        writeln!(file, "# Decision Log Configuration")?;
        writeln!(file, "# mode: {}", self.mode)?;
        writeln!(file, "# generated: {}", Utc::now().format("%Y-%m-%dT%H:%M:%SZ"))?;
        writeln!(file, "# base_order_size: {}", config.base_order_size)?;
        writeln!(file, "# min_order_size: {}", config.min_order_size)?;
        writeln!(file, "# max_market_exposure: {}", config.max_market_exposure)?;
        writeln!(file, "# max_total_exposure: {}", config.max_total_exposure)?;
        writeln!(file, "# available_balance: {}", config.available_balance)?;
        writeln!(file, "# max_edge_factor: {}", config.max_edge_factor)?;
        writeln!(file, "# window_duration_secs: {}", config.window_duration_secs)?;
        writeln!(file, "# strong_up_ratio: {}", config.strong_up_ratio)?;
        writeln!(file, "# lean_up_ratio: {}", config.lean_up_ratio)?;
        writeln!(file, "#")
    }

    fn write_header(&self) -> std::io::Result<()> {
        let mut file = self.file.lock().unwrap();
        file.write_all(DecisionRecord::csv_header().as_bytes())
    }

    /// Log a decision.
    pub fn log(&self, mut record: DecisionRecord) {
        record.mode = self.mode.clone();
        let row = record.to_csv_row();

        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(row.as_bytes());
        }
    }

    /// Log a skip decision.
    #[allow(clippy::too_many_arguments)]
    pub fn log_skip(
        &self,
        timestamp: DateTime<Utc>,
        event_id: &str,
        asset: CryptoAsset,
        spot_price: Decimal,
        strike_price: Decimal,
        distance: Decimal,
        atr: Decimal,
        seconds_remaining: i64,
        phase: &str,
        signal: Signal,
        yes_price: Decimal,
        no_price: Decimal,
        base_confidence: Decimal,
        pivot_multiplier: Decimal,
        confidence: Decimal,
        ev: Decimal,
        min_edge: Decimal,
        favorable_price: Decimal,
        reason: SkipReason,
    ) {
        let distance_dollars = distance * strike_price;
        let atr_multiple = if atr > Decimal::ZERO {
            distance_dollars.abs() / atr
        } else {
            Decimal::ZERO
        };

        let record = DecisionRecord {
            timestamp,
            mode: self.mode.clone(),
            event_id: event_id.to_string(),
            asset,
            spot_price,
            strike_price,
            distance,
            distance_dollars,
            atr,
            atr_multiple,
            seconds_remaining,
            phase: phase.to_string(),
            signal,
            yes_price,
            no_price,
            base_confidence,
            pivot_multiplier,
            confidence,
            ev,
            min_edge,
            favorable_price,
            action: "SKIP".to_string(),
            skip_reason: Some(format!("{:?}", reason)),
            trade_size: None,
            up_ratio: None,
        };

        self.log(record);
    }

    /// Log an execute decision.
    #[allow(clippy::too_many_arguments)]
    pub fn log_execute(
        &self,
        timestamp: DateTime<Utc>,
        event_id: &str,
        asset: CryptoAsset,
        spot_price: Decimal,
        strike_price: Decimal,
        distance: Decimal,
        atr: Decimal,
        seconds_remaining: i64,
        phase: &str,
        signal: Signal,
        yes_price: Decimal,
        no_price: Decimal,
        base_confidence: Decimal,
        pivot_multiplier: Decimal,
        confidence: Decimal,
        ev: Decimal,
        min_edge: Decimal,
        favorable_price: Decimal,
        trade_size: Decimal,
        up_ratio: Decimal,
    ) {
        let distance_dollars = distance * strike_price;
        let atr_multiple = if atr > Decimal::ZERO {
            distance_dollars.abs() / atr
        } else {
            Decimal::ZERO
        };

        let record = DecisionRecord {
            timestamp,
            mode: self.mode.clone(),
            event_id: event_id.to_string(),
            asset,
            spot_price,
            strike_price,
            distance,
            distance_dollars,
            atr,
            atr_multiple,
            seconds_remaining,
            phase: phase.to_string(),
            signal,
            yes_price,
            no_price,
            base_confidence,
            pivot_multiplier,
            confidence,
            ev,
            min_edge,
            favorable_price,
            action: "EXECUTE".to_string(),
            skip_reason: None,
            trade_size: Some(trade_size),
            up_ratio: Some(up_ratio),
        };

        self.log(record);
    }
}
