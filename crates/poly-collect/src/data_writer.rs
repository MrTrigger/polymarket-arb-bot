//! Data writer abstraction for collected data.
//!
//! Supports writing to ClickHouse, CSV, or both based on configuration.
//! When mode is "both", CSV writes continue even if ClickHouse fails.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use chrono::NaiveDate;
use poly_common::{ClickHouseClient, MarketWindow, OrderBookDelta, OrderBookSnapshot, PriceHistory, SpotPrice};
use tracing::warn;

use crate::config::OutputMode;
use crate::csv_writer::{CsvWriter, ManifestInfo};

/// Collection parameters for organizing CSV output.
#[derive(Debug, Clone)]
pub struct CollectionParams {
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub interval: String,
}

/// Data writer that supports multiple output destinations.
pub struct DataWriter {
    clickhouse: Option<Arc<ClickHouseClient>>,
    csv: Option<Arc<CsvWriter>>,
    /// If true, continue with other outputs when one fails
    best_effort: bool,
}

impl DataWriter {
    /// Creates a new DataWriter based on the output mode.
    /// For live mode without specific collection parameters.
    pub fn new(mode: &OutputMode, clickhouse: Option<Arc<ClickHouseClient>>) -> Result<Self> {
        Self::with_params(mode, clickhouse, None)
    }

    /// Creates a new DataWriter with collection parameters for organizing CSV output.
    /// CSV files will be placed in a subfolder: `{base_dir}/{start}_{end}_{interval}/`
    pub fn with_params(
        mode: &OutputMode,
        clickhouse: Option<Arc<ClickHouseClient>>,
        params: Option<CollectionParams>,
    ) -> Result<Self> {
        let csv = match mode {
            OutputMode::Csv { output_dir } | OutputMode::Both { output_dir } => {
                let writer = if let Some(ref p) = params {
                    CsvWriter::with_collection_params(
                        output_dir.clone(),
                        p.start_date,
                        p.end_date,
                        &p.interval,
                    )?
                } else {
                    CsvWriter::new(output_dir.clone())?
                };
                Some(Arc::new(writer))
            }
            OutputMode::ClickHouse => None,
        };

        let clickhouse = match mode {
            OutputMode::ClickHouse | OutputMode::Both { .. } => clickhouse,
            OutputMode::Csv { .. } => None,
        };

        // In "both" mode, use best-effort (continue if one output fails)
        let best_effort = matches!(mode, OutputMode::Both { .. });

        Ok(Self {
            clickhouse,
            csv,
            best_effort,
        })
    }

    /// Returns true if ClickHouse output is enabled.
    pub fn has_clickhouse(&self) -> bool {
        self.clickhouse.is_some()
    }

    /// Returns true if CSV output is enabled.
    pub fn has_csv(&self) -> bool {
        self.csv.is_some()
    }

    /// Returns the ClickHouse client if available.
    pub fn clickhouse(&self) -> Option<&Arc<ClickHouseClient>> {
        self.clickhouse.as_ref()
    }

    /// Returns the CSV writer if available.
    pub fn csv(&self) -> Option<&Arc<CsvWriter>> {
        self.csv.as_ref()
    }

    /// Returns the CSV output directory if CSV is enabled.
    pub fn csv_dir(&self) -> Option<PathBuf> {
        self.csv.as_ref().map(|c| c.output_dir().to_path_buf())
    }

    /// Writes spot prices to all enabled outputs.
    pub async fn write_spot_prices(&self, prices: &[SpotPrice]) -> Result<()> {
        if prices.is_empty() {
            return Ok(());
        }

        let mut last_error: Option<anyhow::Error> = None;

        // Write to ClickHouse
        if let Some(ref ch) = self.clickhouse {
            if let Err(e) = ch.insert_spot_prices(prices).await {
                if self.best_effort {
                    warn!("ClickHouse write failed (continuing with CSV): {}", e);
                } else {
                    return Err(e.into());
                }
                last_error = Some(e.into());
            }
        }

        // Write to CSV
        if let Some(ref csv) = self.csv {
            if let Err(e) = csv.write_spot_prices(prices) {
                if self.best_effort && last_error.is_none() {
                    warn!("CSV write failed: {}", e);
                }
                last_error = Some(e);
            }
        }

        // In best_effort mode, only fail if all outputs failed
        if self.best_effort {
            Ok(())
        } else if let Some(e) = last_error {
            Err(e)
        } else {
            Ok(())
        }
    }

    /// Writes order book snapshots to all enabled outputs.
    pub async fn write_snapshots(&self, snapshots: &[OrderBookSnapshot]) -> Result<()> {
        if snapshots.is_empty() {
            return Ok(());
        }

        let mut last_error: Option<anyhow::Error> = None;

        if let Some(ref ch) = self.clickhouse {
            if let Err(e) = ch.insert_snapshots(snapshots).await {
                if self.best_effort {
                    warn!("ClickHouse write failed (continuing with CSV): {}", e);
                } else {
                    return Err(e.into());
                }
                last_error = Some(e.into());
            }
        }

        if let Some(ref csv) = self.csv {
            if let Err(e) = csv.write_snapshots(snapshots) {
                if self.best_effort && last_error.is_none() {
                    warn!("CSV write failed: {}", e);
                }
                last_error = Some(e);
            }
        }

        if self.best_effort {
            Ok(())
        } else if let Some(e) = last_error {
            Err(e)
        } else {
            Ok(())
        }
    }

    /// Writes order book deltas to all enabled outputs.
    pub async fn write_deltas(&self, deltas: &[OrderBookDelta]) -> Result<()> {
        if deltas.is_empty() {
            return Ok(());
        }

        let mut last_error: Option<anyhow::Error> = None;

        if let Some(ref ch) = self.clickhouse {
            if let Err(e) = ch.insert_deltas(deltas).await {
                if self.best_effort {
                    warn!("ClickHouse write failed (continuing with CSV): {}", e);
                } else {
                    return Err(e.into());
                }
                last_error = Some(e.into());
            }
        }

        if let Some(ref csv) = self.csv {
            if let Err(e) = csv.write_deltas(deltas) {
                if self.best_effort && last_error.is_none() {
                    warn!("CSV write failed: {}", e);
                }
                last_error = Some(e);
            }
        }

        if self.best_effort {
            Ok(())
        } else if let Some(e) = last_error {
            Err(e)
        } else {
            Ok(())
        }
    }

    /// Writes market windows to all enabled outputs.
    pub async fn write_market_windows(&self, windows: &[MarketWindow]) -> Result<()> {
        if windows.is_empty() {
            return Ok(());
        }

        let mut last_error: Option<anyhow::Error> = None;

        if let Some(ref ch) = self.clickhouse {
            if let Err(e) = ch.insert_market_windows(windows).await {
                if self.best_effort {
                    warn!("ClickHouse write failed (continuing with CSV): {}", e);
                } else {
                    return Err(e.into());
                }
                last_error = Some(e.into());
            }
        }

        if let Some(ref csv) = self.csv {
            if let Err(e) = csv.write_market_windows(windows) {
                if self.best_effort && last_error.is_none() {
                    warn!("CSV write failed: {}", e);
                }
                last_error = Some(e);
            }
        }

        if self.best_effort {
            Ok(())
        } else if let Some(e) = last_error {
            Err(e)
        } else {
            Ok(())
        }
    }

    /// Writes price history to all enabled outputs.
    pub async fn write_price_history(&self, prices: &[PriceHistory]) -> Result<()> {
        if prices.is_empty() {
            return Ok(());
        }

        let mut last_error: Option<anyhow::Error> = None;

        if let Some(ref ch) = self.clickhouse {
            if let Err(e) = ch.insert_price_history(prices).await {
                if self.best_effort {
                    warn!("ClickHouse write failed (continuing with CSV): {}", e);
                } else {
                    return Err(e.into());
                }
                last_error = Some(e.into());
            }
        }

        if let Some(ref csv) = self.csv {
            if let Err(e) = csv.write_price_history(prices) {
                if self.best_effort && last_error.is_none() {
                    warn!("CSV write failed: {}", e);
                }
                last_error = Some(e);
            }
        }

        if self.best_effort {
            Ok(())
        } else if let Some(e) = last_error {
            Err(e)
        } else {
            Ok(())
        }
    }

    /// Writes aligned YES/NO price pairs to all enabled outputs.
    pub async fn write_aligned_prices(&self, prices: &[poly_common::AlignedPricePair]) -> Result<()> {
        if prices.is_empty() {
            return Ok(());
        }

        let mut last_error: Option<anyhow::Error> = None;

        // For now, only write to CSV (no ClickHouse support for aligned prices yet)
        if let Some(ref csv) = self.csv {
            if let Err(e) = csv.write_aligned_prices(prices) {
                if self.best_effort {
                    warn!("CSV write failed for aligned prices: {}", e);
                }
                last_error = Some(e);
            }
        }

        if self.best_effort {
            Ok(())
        } else if let Some(e) = last_error {
            Err(e)
        } else {
            Ok(())
        }
    }

    /// Flushes all outputs.
    pub fn flush(&self) -> Result<()> {
        if let Some(ref csv) = self.csv {
            csv.flush_all()?;
        }
        Ok(())
    }

    /// Finalizes market windows: writes only windows with L2 data to CSV.
    /// Call before write_manifest on shutdown.
    pub fn finalize_market_windows(&self) -> Result<()> {
        if let Some(ref csv) = self.csv {
            csv.finalize_market_windows()?;
        }
        Ok(())
    }

    /// Writes a manifest.json describing the collected dataset.
    pub fn write_manifest(&self, info: ManifestInfo) -> Result<()> {
        if let Some(ref csv) = self.csv {
            csv.write_manifest(info)?;
        }
        Ok(())
    }
}
