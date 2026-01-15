//! CSV output for collected data.
//!
//! Writes data to CSV files organized in subfolders by collection parameters:
//! - `data/{start_date}_{end_date}_{interval}/binance_spot_prices.csv`
//! - `data/{start_date}_{end_date}_{interval}/polymarket_orderbook_snapshots.csv`
//! - etc.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use poly_common::{MarketWindow, OrderBookDelta, OrderBookSnapshot, PriceHistory, SpotPrice};

/// CSV file names (descriptive of what they contain and their source).
const BINANCE_SPOT_PRICES_FILE: &str = "binance_spot_prices.csv";
const POLYMARKET_SNAPSHOTS_FILE: &str = "polymarket_orderbook_snapshots.csv";
const POLYMARKET_DELTAS_FILE: &str = "polymarket_orderbook_deltas.csv";
const POLYMARKET_WINDOWS_FILE: &str = "polymarket_market_windows.csv";
const POLYMARKET_PRICE_HISTORY_FILE: &str = "polymarket_price_history.csv";

/// CSV writer for collected data.
pub struct CsvWriter {
    output_dir: PathBuf,
    /// Writers are lazily initialized and wrapped in Mutex for thread safety
    spot_prices: Mutex<Option<csv::Writer<File>>>,
    snapshots: Mutex<Option<csv::Writer<File>>>,
    deltas: Mutex<Option<csv::Writer<File>>>,
    market_windows: Mutex<Option<csv::Writer<File>>>,
    price_history: Mutex<Option<csv::Writer<File>>>,
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
            market_windows: Mutex::new(None),
            price_history: Mutex::new(None),
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

        Ok(())
    }

    /// Writes Polymarket order book snapshots to CSV.
    pub fn write_snapshots(&self, snapshots: &[OrderBookSnapshot]) -> Result<()> {
        if snapshots.is_empty() {
            return Ok(());
        }

        let path = self.output_dir.join(POLYMARKET_SNAPSHOTS_FILE);
        Self::get_or_create_writer(&self.snapshots, &path)?;

        let mut guard = self.snapshots.lock().unwrap();
        let writer = guard.as_mut().unwrap();

        for snapshot in snapshots {
            writer.serialize(snapshot)?;
        }
        writer.flush()?;

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

        Ok(())
    }

    /// Writes Polymarket market windows to CSV.
    pub fn write_market_windows(&self, windows: &[MarketWindow]) -> Result<()> {
        if windows.is_empty() {
            return Ok(());
        }

        let path = self.output_dir.join(POLYMARKET_WINDOWS_FILE);
        Self::get_or_create_writer(&self.market_windows, &path)?;

        let mut guard = self.market_windows.lock().unwrap();
        let writer = guard.as_mut().unwrap();

        for window in windows {
            writer.serialize(window)?;
        }
        writer.flush()?;

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

        Ok(())
    }

    /// Flushes all writers.
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
        if let Some(ref mut writer) = *self.market_windows.lock().unwrap() {
            writer.flush()?;
        }
        if let Some(ref mut writer) = *self.price_history.lock().unwrap() {
            writer.flush()?;
        }
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
