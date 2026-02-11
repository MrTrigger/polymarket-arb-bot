//! CSV output for collected data.
//!
//! Writes data to CSV files organized in subfolders by collection parameters:
//! - `data/{start_date}_{end_date}_{interval}/binance_spot_prices.csv`
//! - `data/{start_date}_{end_date}_{interval}/polymarket_orderbook_snapshots.csv`
//! - etc.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use poly_common::{AlignedPricePair, MarketWindow, OrderBookDelta, OrderBookSnapshot, PriceHistory, SpotPrice};
use serde::Serialize;

/// CSV file names (descriptive of what they contain and their source).
const BINANCE_SPOT_PRICES_FILE: &str = "binance_spot_prices.csv";
const POLYMARKET_SNAPSHOTS_FILE: &str = "polymarket_orderbook_snapshots.csv";
const POLYMARKET_DELTAS_FILE: &str = "polymarket_orderbook_deltas.csv";
const POLYMARKET_WINDOWS_FILE: &str = "polymarket_market_windows.csv";
const POLYMARKET_PRICE_HISTORY_FILE: &str = "polymarket_price_history.csv";
const POLYMARKET_ALIGNED_PRICES_FILE: &str = "polymarket_aligned_prices.csv";
const CHAINLINK_PRICES_FILE: &str = "chainlink_prices.csv";

/// Information for the manifest file.
pub struct ManifestInfo {
    pub collection_start: DateTime<Utc>,
    pub collection_end: DateTime<Utc>,
    pub assets: Vec<String>,
    pub timeframes: Vec<String>,
}

/// JSON structure written to manifest.json.
#[derive(Serialize)]
struct ManifestJson {
    collection_start: String,
    collection_end: String,
    duration_secs: i64,
    assets: Vec<String>,
    timeframes: Vec<String>,
    row_counts: ManifestRowCounts,
}

#[derive(Serialize)]
struct ManifestRowCounts {
    spot_prices: u64,
    orderbook_snapshots: u64,
    orderbook_deltas: u64,
    market_windows: u64,
    price_history: u64,
    aligned_prices: u64,
    chainlink_prices: u64,
}

/// CSV writer for collected data.
pub struct CsvWriter {
    output_dir: PathBuf,
    /// Writers are lazily initialized and wrapped in Mutex for thread safety
    spot_prices: Mutex<Option<csv::Writer<File>>>,
    snapshots: Mutex<Option<csv::Writer<File>>>,
    deltas: Mutex<Option<csv::Writer<File>>>,
    price_history: Mutex<Option<csv::Writer<File>>>,
    aligned_prices: Mutex<Option<csv::Writer<File>>>,
    chainlink_prices: Mutex<Option<csv::Writer<File>>>,
    market_windows: Mutex<Option<csv::Writer<File>>>,
    /// Market windows already written to CSV (dedup by event_id).
    written_windows: Mutex<HashSet<String>>,
    /// Market windows buffered in memory for finalize (L2-filtered rewrite).
    pending_windows: Mutex<HashMap<String, MarketWindow>>,
    /// Event IDs that have received at least one orderbook snapshot.
    seen_orderbook_events: Mutex<HashSet<String>>,
    /// Row counters for manifest
    spot_price_count: AtomicU64,
    snapshot_count: AtomicU64,
    delta_count: AtomicU64,
    window_count: AtomicU64,
    price_history_count: AtomicU64,
    aligned_price_count: AtomicU64,
    chainlink_price_count: AtomicU64,
}

impl CsvWriter {
    /// Creates a new CSV writer with the given output directory.
    pub fn new(output_dir: PathBuf) -> Result<Self> {
        // Create output directory if it doesn't exist
        fs::create_dir_all(&output_dir)
            .with_context(|| format!("Failed to create output directory: {:?}", output_dir))?;

        Ok(Self {
            output_dir,
            spot_prices: Mutex::new(None),
            snapshots: Mutex::new(None),
            deltas: Mutex::new(None),
            price_history: Mutex::new(None),
            aligned_prices: Mutex::new(None),
            chainlink_prices: Mutex::new(None),
            market_windows: Mutex::new(None),
            written_windows: Mutex::new(HashSet::new()),
            pending_windows: Mutex::new(HashMap::new()),
            seen_orderbook_events: Mutex::new(HashSet::new()),
            spot_price_count: AtomicU64::new(0),
            snapshot_count: AtomicU64::new(0),
            delta_count: AtomicU64::new(0),
            window_count: AtomicU64::new(0),
            price_history_count: AtomicU64::new(0),
            aligned_price_count: AtomicU64::new(0),
            chainlink_price_count: AtomicU64::new(0),
        })
    }

    /// Creates a new CSV writer with a subfolder based on collection parameters.
    /// Subfolder format: `{base_dir}/{start_date}_{end_date}_{interval}/`
    pub fn with_collection_params(
        base_dir: PathBuf,
        start_date: NaiveDate,
        end_date: NaiveDate,
        interval: &str,
    ) -> Result<Self> {
        let subfolder = format!(
            "{}_{}_{}",
            start_date.format("%Y-%m-%d"),
            end_date.format("%Y-%m-%d"),
            interval
        );
        let output_dir = base_dir.join(subfolder);
        Self::new(output_dir)
    }

    /// Returns the output directory.
    pub fn output_dir(&self) -> &Path {
        &self.output_dir
    }

    /// Gets or creates a CSV writer for the given file.
    fn get_or_create_writer(
        writer_mutex: &Mutex<Option<csv::Writer<File>>>,
        path: &Path,
    ) -> Result<()> {
        let mut guard = writer_mutex.lock().unwrap();
        if guard.is_none() {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("Failed to open CSV file: {:?}", path))?;

            // Check if file is empty (new file) to write headers
            let needs_headers = file.metadata()?.len() == 0;
            let writer = csv::WriterBuilder::new()
                .has_headers(needs_headers)
                .from_writer(file);

            *guard = Some(writer);
        }
        Ok(())
    }

    /// Writes Binance spot prices to CSV.
    pub fn write_spot_prices(&self, prices: &[SpotPrice]) -> Result<()> {
        if prices.is_empty() {
            return Ok(());
        }

        let path = self.output_dir.join(BINANCE_SPOT_PRICES_FILE);
        Self::get_or_create_writer(&self.spot_prices, &path)?;

        let mut guard = self.spot_prices.lock().unwrap();
        let writer = guard.as_mut().unwrap();

        for price in prices {
            writer.serialize(price)?;
        }
        writer.flush()?;
        self.spot_price_count.fetch_add(prices.len() as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Writes Polymarket order book snapshots to CSV.
    /// Also records event_ids so we know which markets have L2 data.
    pub fn write_snapshots(&self, snapshots: &[OrderBookSnapshot]) -> Result<()> {
        if snapshots.is_empty() {
            return Ok(());
        }

        // Track which events have L2 data
        {
            let mut seen = self.seen_orderbook_events.lock().unwrap();
            for snapshot in snapshots {
                seen.insert(snapshot.event_id.clone());
            }
        }

        let path = self.output_dir.join(POLYMARKET_SNAPSHOTS_FILE);
        Self::get_or_create_writer(&self.snapshots, &path)?;

        let mut guard = self.snapshots.lock().unwrap();
        let writer = guard.as_mut().unwrap();

        for snapshot in snapshots {
            writer.serialize(snapshot)?;
        }
        writer.flush()?;
        self.snapshot_count.fetch_add(snapshots.len() as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Writes Polymarket order book deltas to CSV.
    pub fn write_deltas(&self, deltas: &[OrderBookDelta]) -> Result<()> {
        if deltas.is_empty() {
            return Ok(());
        }

        let path = self.output_dir.join(POLYMARKET_DELTAS_FILE);
        Self::get_or_create_writer(&self.deltas, &path)?;

        let mut guard = self.deltas.lock().unwrap();
        let writer = guard.as_mut().unwrap();

        for delta in deltas {
            writer.serialize(delta)?;
        }
        writer.flush()?;
        self.delta_count.fetch_add(deltas.len() as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Buffers market windows in memory. They are only written to CSV
    /// on `finalize()`, filtered to events that have L2 orderbook data.
    pub fn write_market_windows(&self, windows: &[MarketWindow]) -> Result<()> {
        if windows.is_empty() {
            return Ok(());
        }

        // Buffer for finalize (L2-filtered rewrite)
        let mut pending = self.pending_windows.lock().unwrap();
        for window in windows {
            pending.insert(window.event_id.clone(), window.clone());
        }
        drop(pending);

        // Write new (unseen) windows to CSV immediately so file is always available
        let mut written = self.written_windows.lock().unwrap();
        let new_windows: Vec<&MarketWindow> = windows
            .iter()
            .filter(|w| !written.contains(&w.event_id))
            .collect();

        if new_windows.is_empty() {
            return Ok(());
        }

        let path = self.output_dir.join(POLYMARKET_WINDOWS_FILE);
        Self::get_or_create_writer(&self.market_windows, &path)?;

        let mut guard = self.market_windows.lock().unwrap();
        let writer = guard.as_mut().unwrap();

        for window in &new_windows {
            writer.serialize(window)?;
            written.insert(window.event_id.clone());
        }
        writer.flush()?;
        self.window_count.fetch_add(new_windows.len() as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Writes Polymarket price history to CSV.
    pub fn write_price_history(&self, prices: &[PriceHistory]) -> Result<()> {
        if prices.is_empty() {
            return Ok(());
        }

        let path = self.output_dir.join(POLYMARKET_PRICE_HISTORY_FILE);
        Self::get_or_create_writer(&self.price_history, &path)?;

        let mut guard = self.price_history.lock().unwrap();
        let writer = guard.as_mut().unwrap();

        for price in prices {
            writer.serialize(price)?;
        }
        writer.flush()?;
        self.price_history_count.fetch_add(prices.len() as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Writes aligned YES/NO price pairs to CSV.
    /// Each row contains both prices at the same timestamp with arb sanity check.
    pub fn write_aligned_prices(&self, prices: &[AlignedPricePair]) -> Result<()> {
        if prices.is_empty() {
            return Ok(());
        }

        let path = self.output_dir.join(POLYMARKET_ALIGNED_PRICES_FILE);
        Self::get_or_create_writer(&self.aligned_prices, &path)?;

        let mut guard = self.aligned_prices.lock().unwrap();
        let writer = guard.as_mut().unwrap();

        for price in prices {
            writer.serialize(price)?;
        }
        writer.flush()?;
        self.aligned_price_count.fetch_add(prices.len() as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Writes Chainlink oracle prices to CSV.
    pub fn write_chainlink_prices(&self, prices: &[crate::rtds::ChainlinkPriceRecord]) -> Result<()> {
        if prices.is_empty() {
            return Ok(());
        }

        let path = self.output_dir.join(CHAINLINK_PRICES_FILE);
        Self::get_or_create_writer(&self.chainlink_prices, &path)?;

        let mut guard = self.chainlink_prices.lock().unwrap();
        let writer = guard.as_mut().unwrap();

        for price in prices {
            writer.serialize(price)?;
        }
        writer.flush()?;
        self.chainlink_price_count.fetch_add(prices.len() as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Flushes all active writers (does NOT finalize market windows).
    pub fn flush_all(&self) -> Result<()> {
        if let Some(ref mut writer) = *self.spot_prices.lock().unwrap() {
            writer.flush()?;
        }
        if let Some(ref mut writer) = *self.snapshots.lock().unwrap() {
            writer.flush()?;
        }
        if let Some(ref mut writer) = *self.deltas.lock().unwrap() {
            writer.flush()?;
        }
        if let Some(ref mut writer) = *self.price_history.lock().unwrap() {
            writer.flush()?;
        }
        if let Some(ref mut writer) = *self.aligned_prices.lock().unwrap() {
            writer.flush()?;
        }
        if let Some(ref mut writer) = *self.chainlink_prices.lock().unwrap() {
            writer.flush()?;
        }
        if let Some(ref mut writer) = *self.market_windows.lock().unwrap() {
            writer.flush()?;
        }
        Ok(())
    }

    /// Writes market windows to CSV, filtered to only those with L2 orderbook data.
    /// Call this once on shutdown, before writing the manifest.
    pub fn finalize_market_windows(&self) -> Result<()> {
        let pending = self.pending_windows.lock().unwrap();
        let seen = self.seen_orderbook_events.lock().unwrap();

        let mut matched: Vec<&MarketWindow> = pending
            .values()
            .filter(|w| seen.contains(&w.event_id))
            .collect();
        matched.sort_by_key(|w| w.window_start);

        let total = pending.len();
        let kept = matched.len();
        let dropped = total - kept;

        if dropped > 0 {
            tracing::info!(
                "Market windows: {} total, {} with L2 data, {} dropped (no orderbook data)",
                total, kept, dropped
            );
        }

        if matched.is_empty() {
            return Ok(());
        }

        let path = self.output_dir.join(POLYMARKET_WINDOWS_FILE);
        // Overwrite (not append) since we write all windows at once
        let file = File::create(&path)
            .with_context(|| format!("Failed to create {:?}", path))?;
        let mut writer = csv::Writer::from_writer(file);

        for window in &matched {
            writer.serialize(window)?;
        }
        writer.flush()?;
        self.window_count.store(kept as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Writes a manifest.json describing the collected dataset.
    pub fn write_manifest(&self, info: ManifestInfo) -> Result<()> {
        let duration_secs = (info.collection_end - info.collection_start).num_seconds();

        let manifest = ManifestJson {
            collection_start: info.collection_start.to_rfc3339(),
            collection_end: info.collection_end.to_rfc3339(),
            duration_secs,
            assets: info.assets,
            timeframes: info.timeframes,
            row_counts: ManifestRowCounts {
                spot_prices: self.spot_price_count.load(Ordering::Relaxed),
                orderbook_snapshots: self.snapshot_count.load(Ordering::Relaxed),
                orderbook_deltas: self.delta_count.load(Ordering::Relaxed),
                market_windows: self.window_count.load(Ordering::Relaxed),
                price_history: self.price_history_count.load(Ordering::Relaxed),
                aligned_prices: self.aligned_price_count.load(Ordering::Relaxed),
                chainlink_prices: self.chainlink_price_count.load(Ordering::Relaxed),
            },
        };

        let json = serde_json::to_string_pretty(&manifest)
            .context("Failed to serialize manifest")?;

        let path = self.output_dir.join("manifest.json");
        fs::write(&path, json)
            .with_context(|| format!("Failed to write manifest: {:?}", path))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;
    use std::fs;

    #[test]
    fn test_csv_writer_creation() {
        let temp_dir = std::env::temp_dir().join("test_csv_writer");
        let _ = fs::remove_dir_all(&temp_dir);

        let _writer = CsvWriter::new(temp_dir.clone()).unwrap();
        assert!(temp_dir.exists());

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_write_spot_prices() {
        let temp_dir = std::env::temp_dir().join("test_csv_spot");
        let _ = fs::remove_dir_all(&temp_dir);

        let writer = CsvWriter::new(temp_dir.clone()).unwrap();

        let prices = vec![SpotPrice {
            asset: "BTC".to_string(),
            price: dec!(100000.50),
            timestamp: Utc::now(),
            quantity: dec!(0.1),
        }];

        writer.write_spot_prices(&prices).unwrap();

        let csv_path = temp_dir.join(BINANCE_SPOT_PRICES_FILE);
        assert!(csv_path.exists());

        // Cleanup
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
