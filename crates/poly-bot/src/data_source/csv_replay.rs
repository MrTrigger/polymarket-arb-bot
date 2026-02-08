//! CSV-based replay data source for backtesting.
//!
//! Streams historical data from CSV files using a k-way merge, keeping only
//! ~1MB in memory regardless of dataset size. This replaces the previous
//! approach of loading all rows into a BinaryHeap which OOM-killed on large
//! datasets (~14GB / 92M rows).

use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::cmp::Ordering;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::{debug, info, warn};

use poly_common::types::CryptoAsset;

use super::{
    BookSnapshotEvent, BookSnapshotPairEvent, DataSource, DataSourceError, MarketEvent,
    SpotPriceEvent, WindowCloseEvent, WindowOpenEvent,
};
use crate::types::PriceLevel;

/// Configuration for the CSV replay data source.
#[derive(Debug, Clone)]
pub struct CsvReplayConfig {
    /// Directory containing the CSV files.
    pub data_dir: String,
    /// Start of replay period (filter).
    pub start_time: Option<DateTime<Utc>>,
    /// End of replay period (filter).
    pub end_time: Option<DateTime<Utc>>,
    /// Assets to filter (empty = all).
    pub assets: Vec<CryptoAsset>,
    /// Replay speed multiplier (1.0 = real-time, 0.0 = max speed).
    pub speed: f64,
    /// Maximum duration from first event. When set, replay stops after
    /// this much time has elapsed from the first event's timestamp.
    pub max_duration: Option<chrono::Duration>,
    /// Minimum interval between orderbook snapshots per event (milliseconds).
    /// E.g. 1000 = keep at most one snapshot per second per event.
    /// 0 or None = no downsampling (use all data).
    pub book_interval_ms: Option<u64>,
}

impl Default for CsvReplayConfig {
    fn default() -> Self {
        Self {
            data_dir: "data".to_string(),
            start_time: None,
            end_time: None,
            assets: vec![
                CryptoAsset::Btc,
                CryptoAsset::Eth,
                CryptoAsset::Sol,
            ],
            speed: 0.0, // Max speed by default for backtesting
            max_duration: None,
            book_interval_ms: None,
        }
    }
}

// ---------------------------------------------------------------------------
// CSV row types
// ---------------------------------------------------------------------------

/// CSV row for spot prices.
#[derive(Debug, Clone, Deserialize)]
struct SpotPriceRow {
    asset: String,
    #[serde(with = "rust_decimal::serde::str")]
    price: Decimal,
    timestamp: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    quantity: Decimal,
}

/// CSV row for market windows.
#[derive(Debug, Clone, Deserialize)]
struct MarketWindowRow {
    event_id: String,
    condition_id: String,
    asset: String,
    yes_token_id: String,
    no_token_id: String,
    #[serde(with = "rust_decimal::serde::str")]
    strike_price: Decimal,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    #[allow(dead_code)]
    discovered_at: DateTime<Utc>,
}

/// CSV row for price history (legacy format).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct PriceHistoryRow {
    token_id: String,
    timestamp: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    price: Decimal,
}

/// CSV row for aligned price pairs (new format with arb sanity check).
#[derive(Debug, Clone, Deserialize)]
struct AlignedPriceRow {
    event_id: String,
    yes_token_id: String,
    no_token_id: String,
    timestamp: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    yes_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    no_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[allow(dead_code)]
    arb: Decimal,
}

/// CSV row for orderbook snapshots (live collected format).
#[derive(Debug, Clone, Deserialize)]
struct OrderBookSnapshotRow {
    token_id: String,
    event_id: String,
    timestamp: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    best_bid: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    best_bid_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    best_ask: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    best_ask_size: Decimal,
    #[allow(dead_code)]
    spread_bps: i32,
    #[serde(with = "rust_decimal::serde::str")]
    #[allow(dead_code)]
    bid_depth: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    #[allow(dead_code)]
    ask_depth: Decimal,
}

/// Mapping of event_id to (yes_token_id, no_token_id)
type EventTokenMap = HashMap<String, (String, String)>;

// ---------------------------------------------------------------------------
// Tiebreak priority: at the same timestamp, WindowOpen < data < WindowClose
// ---------------------------------------------------------------------------

fn event_priority(event: &MarketEvent) -> u8 {
    match event {
        MarketEvent::WindowOpen(_) => 0,
        MarketEvent::SpotPrice(_) => 1,
        MarketEvent::BookSnapshot(_) | MarketEvent::BookSnapshotPair(_) => 1,
        MarketEvent::BookDelta(_) => 1,
        MarketEvent::Fill(_) => 1,
        MarketEvent::WindowClose(_) => 2,
        MarketEvent::Heartbeat(_) => 1,
    }
}

// ---------------------------------------------------------------------------
// Merge entry: one peeked event per stream source, used in the k-way heap
// ---------------------------------------------------------------------------

struct MergeEntry {
    timestamp: DateTime<Utc>,
    priority: u8,
    source_idx: usize,
    event: MarketEvent,
}

impl Eq for MergeEntry {}

impl PartialEq for MergeEntry {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp == other.timestamp && self.priority == other.priority
    }
}

impl Ord for MergeEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse for min-heap (earliest timestamp first, then lowest priority)
        other.timestamp.cmp(&self.timestamp)
            .then_with(|| other.priority.cmp(&self.priority))
    }
}

impl PartialOrd for MergeEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------
// Stream sources: each produces events on demand from CSV or in-memory data
// ---------------------------------------------------------------------------

/// Market windows: small enough to load entirely into memory (~500KB for ~860 rows).
/// Produces WindowOpen and WindowClose events sorted by timestamp.
struct WindowSource {
    events: VecDeque<(DateTime<Utc>, MarketEvent)>,
}

impl WindowSource {
    fn next_event(&mut self) -> Option<MarketEvent> {
        self.events.pop_front().map(|(_ts, ev)| ev)
    }
}

/// Spot prices: streamed row-by-row from CSV.
struct SpotPriceSource {
    reader: csv::DeserializeRecordsIntoIter<BufReader<File>, SpotPriceRow>,
    config_assets: Vec<CryptoAsset>,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
    exhausted: bool,
}

impl SpotPriceSource {
    fn next_event(&mut self) -> Option<MarketEvent> {
        if self.exhausted {
            return None;
        }
        loop {
            match self.reader.next() {
                Some(Ok(row)) => {
                    let asset = match parse_asset(&row.asset) {
                        Some(a) => a,
                        None => continue,
                    };
                    if !self.config_assets.is_empty() && !self.config_assets.contains(&asset) {
                        continue;
                    }
                    if let Some(start) = self.start_time
                        && row.timestamp < start { continue; }
                    if let Some(end) = self.end_time
                        && row.timestamp > end { continue; }
                    return Some(MarketEvent::SpotPrice(SpotPriceEvent {
                        asset,
                        price: row.price,
                        quantity: row.quantity,
                        timestamp: row.timestamp,
                    }));
                }
                Some(Err(e)) => {
                    warn!("Spot price CSV parse error (skipping row): {}", e);
                    continue;
                }
                None => {
                    self.exhausted = true;
                    return None;
                }
            }
        }
    }

}

/// Orderbook snapshots: streamed row-by-row, pairing YES/NO on the fly.
struct OrderbookSource {
    reader: csv::DeserializeRecordsIntoIter<BufReader<File>, OrderBookSnapshotRow>,
    event_tokens: EventTokenMap,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
    /// Buffer for unpaired rows: (event_id, timestamp) -> row
    pending: HashMap<(String, DateTime<Utc>), OrderBookSnapshotRow>,
    exhausted: bool,
    unpaired_count: usize,
    /// Minimum interval for downsampling (None = no downsampling).
    min_interval: Option<chrono::Duration>,
    /// Last emitted timestamp per event_id (for downsampling).
    last_emitted: HashMap<String, DateTime<Utc>>,
}

impl OrderbookSource {
    fn next_event(&mut self) -> Option<MarketEvent> {
        if self.exhausted {
            return None;
        }
        loop {
            match self.reader.next() {
                Some(Ok(row)) => {
                    // Time filter
                    if let Some(start) = self.start_time
                        && row.timestamp < start { continue; }
                    if let Some(end) = self.end_time
                        && row.timestamp > end { continue; }
                    // Event filter
                    let Some((yes_token, _no_token)) = self.event_tokens.get(&row.event_id) else {
                        continue;
                    };
                    let yes_token = yes_token.clone();

                    // Downsample: skip if too close to last emitted for this event
                    if let Some(interval) = self.min_interval {
                        if let Some(last) = self.last_emitted.get(&row.event_id) {
                            if row.timestamp - *last < interval {
                                continue;
                            }
                        }
                    }

                    let key = (row.event_id.clone(), row.timestamp);

                    if let Some(partner) = self.pending.remove(&key) {
                        // Pair found - determine YES vs NO
                        let (yes_row, no_row) = if row.token_id == yes_token {
                            (row, partner)
                        } else if partner.token_id == yes_token {
                            (partner, row)
                        } else {
                            continue;
                        };

                        let event_id = yes_row.event_id.clone();
                        let ts = yes_row.timestamp;
                        let event = make_orderbook_pair_event(yes_row, no_row);
                        if self.min_interval.is_some() {
                            self.last_emitted.insert(event_id, ts);
                        }
                        return Some(event);
                    } else {
                        self.pending.insert(key, row);
                    }
                }
                Some(Err(e)) => {
                    warn!("Orderbook CSV parse error (skipping row): {}", e);
                    continue;
                }
                None => {
                    self.exhausted = true;
                    self.unpaired_count = self.pending.len();
                    if self.unpaired_count > 10 {
                        warn!("{} orderbook snapshots had no YES/NO partner", self.unpaired_count);
                    }
                    return None;
                }
            }
        }
    }

}

/// Aligned prices: streamed row-by-row (already paired YES/NO in CSV).
struct AlignedPriceSource {
    reader: csv::DeserializeRecordsIntoIter<BufReader<File>, AlignedPriceRow>,
    event_tokens: EventTokenMap,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
    exhausted: bool,
}

impl AlignedPriceSource {
    fn next_event(&mut self) -> Option<MarketEvent> {
        if self.exhausted {
            return None;
        }
        loop {
            match self.reader.next() {
                Some(Ok(row)) => {
                    if !self.event_tokens.contains_key(&row.event_id) {
                        continue;
                    }
                    if let Some(start) = self.start_time
                        && row.timestamp < start { continue; }
                    if let Some(end) = self.end_time
                        && row.timestamp > end { continue; }
                    return Some(make_aligned_pair_event(row));
                }
                Some(Err(e)) => {
                    warn!("Aligned price CSV parse error (skipping row): {}", e);
                    continue;
                }
                None => {
                    self.exhausted = true;
                    return None;
                }
            }
        }
    }

}

// ---------------------------------------------------------------------------
// Helper: convert rows to MarketEvent
// ---------------------------------------------------------------------------

fn make_orderbook_pair_event(yes_row: OrderBookSnapshotRow, no_row: OrderBookSnapshotRow) -> MarketEvent {
    let timestamp = yes_row.timestamp;
    let event_id = yes_row.event_id.clone();

    let yes_snapshot = BookSnapshotEvent {
        token_id: yes_row.token_id,
        event_id: yes_row.event_id,
        bids: if yes_row.best_bid > Decimal::ZERO {
            vec![PriceLevel::new(yes_row.best_bid, yes_row.best_bid_size)]
        } else {
            vec![]
        },
        asks: if yes_row.best_ask < Decimal::ONE {
            vec![PriceLevel::new(yes_row.best_ask, yes_row.best_ask_size)]
        } else {
            vec![]
        },
        timestamp,
    };

    let no_snapshot = BookSnapshotEvent {
        token_id: no_row.token_id,
        event_id: no_row.event_id,
        bids: if no_row.best_bid > Decimal::ZERO {
            vec![PriceLevel::new(no_row.best_bid, no_row.best_bid_size)]
        } else {
            vec![]
        },
        asks: if no_row.best_ask < Decimal::ONE {
            vec![PriceLevel::new(no_row.best_ask, no_row.best_ask_size)]
        } else {
            vec![]
        },
        timestamp,
    };

    MarketEvent::BookSnapshotPair(BookSnapshotPairEvent {
        event_id,
        yes_snapshot,
        no_snapshot,
        timestamp,
    })
}

fn make_aligned_pair_event(row: AlignedPriceRow) -> MarketEvent {
    let spread = Decimal::new(1, 2); // 0.01 spread
    let half_spread = spread / Decimal::TWO;
    let default_size = Decimal::new(1000, 0);

    let yes_bid = (row.yes_price - half_spread).max(Decimal::ZERO);
    let yes_ask = (row.yes_price + half_spread).min(Decimal::ONE);
    let yes_snapshot = BookSnapshotEvent {
        token_id: row.yes_token_id.clone(),
        event_id: row.event_id.clone(),
        bids: if yes_bid > Decimal::ZERO { vec![PriceLevel::new(yes_bid, default_size)] } else { vec![] },
        asks: if yes_ask < Decimal::ONE { vec![PriceLevel::new(yes_ask, default_size)] } else { vec![] },
        timestamp: row.timestamp,
    };

    let no_bid = (row.no_price - half_spread).max(Decimal::ZERO);
    let no_ask = (row.no_price + half_spread).min(Decimal::ONE);
    let no_snapshot = BookSnapshotEvent {
        token_id: row.no_token_id,
        event_id: row.event_id.clone(),
        bids: if no_bid > Decimal::ZERO { vec![PriceLevel::new(no_bid, default_size)] } else { vec![] },
        asks: if no_ask < Decimal::ONE { vec![PriceLevel::new(no_ask, default_size)] } else { vec![] },
        timestamp: row.timestamp,
    };

    MarketEvent::BookSnapshotPair(BookSnapshotPairEvent {
        event_id: row.event_id,
        yes_snapshot,
        no_snapshot,
        timestamp: row.timestamp,
    })
}

// ---------------------------------------------------------------------------
// CsvReplayDataSource: streaming k-way merge
// ---------------------------------------------------------------------------

/// Index constants for source types in the sources array.
const SOURCE_WINDOWS: usize = 0;
const SOURCE_SPOT: usize = 1;
const SOURCE_BOOK: usize = 2; // orderbook snapshots OR aligned prices

/// CSV-based replay data source.
///
/// Streams events using a k-way merge with ~1MB memory overhead regardless
/// of dataset size.
pub struct CsvReplayDataSource {
    config: CsvReplayConfig,
    /// Streaming sources (up to 3: windows, spot prices, book data).
    window_source: Option<WindowSource>,
    spot_source: Option<SpotPriceSource>,
    book_source: Option<BookSource>,
    /// Tiny merge heap (at most 3 entries, one per source).
    merge_heap: BinaryHeap<MergeEntry>,
    /// Current replay time.
    current_time: Option<DateTime<Utc>>,
    /// Whether streaming has been initialized.
    initialized: bool,
    /// Whether replay is complete.
    is_exhausted: bool,
    /// Event counter for diagnostics.
    event_count: u64,
    /// Timestamp of the first event (for max_duration enforcement).
    first_event_time: Option<DateTime<Utc>>,
    /// Maximum duration from first event before stopping.
    max_duration: Option<chrono::Duration>,
}

/// Either orderbook snapshots or aligned prices (mutually exclusive).
enum BookSource {
    Orderbook(OrderbookSource),
    Aligned(AlignedPriceSource),
}

impl BookSource {
    fn next_event(&mut self) -> Option<MarketEvent> {
        match self {
            BookSource::Orderbook(s) => s.next_event(),
            BookSource::Aligned(s) => s.next_event(),
        }
    }
}

impl CsvReplayDataSource {
    /// Creates a new CSV replay data source.
    pub fn new(config: CsvReplayConfig) -> Self {
        let max_duration = config.max_duration;
        Self {
            config,
            window_source: None,
            spot_source: None,
            book_source: None,
            merge_heap: BinaryHeap::with_capacity(4),
            current_time: None,
            initialized: false,
            is_exhausted: false,
            event_count: 0,
            first_event_time: None,
            max_duration,
        }
    }

    /// Initialize streaming: open CSV readers, load windows, prime merge heap.
    fn init_streaming(&mut self) -> Result<(), DataSourceError> {
        let data_dir_str = self.config.data_dir.clone();
        let data_dir = Path::new(&data_dir_str);

        if !data_dir.exists() {
            return Err(DataSourceError::Connection(format!(
                "Data directory not found: {}",
                data_dir_str
            )));
        }

        info!("Initializing streaming CSV replay from {}", data_dir_str);

        // 1. Load market windows entirely (small: ~860 rows, ~500KB)
        let (window_events, event_tokens) = load_market_windows(
            data_dir,
            &self.config.assets,
            self.config.start_time,
            self.config.end_time,
        )?;
        let window_count = window_events.len() / 2; // open + close per window
        info!("Loaded {} market windows ({} events)", window_count, window_events.len());

        if !window_events.is_empty() {
            self.window_source = Some(WindowSource {
                events: VecDeque::from(window_events),
            });
        }

        // 2. Open spot prices CSV reader (streaming)
        let spot_path = data_dir.join("binance_spot_prices.csv");
        if spot_path.exists() {
            let file = File::open(&spot_path)
                .map_err(|e| DataSourceError::Parse(format!("Failed to open {:?}: {}", spot_path, e)))?;
            let buf_reader = BufReader::with_capacity(64 * 1024, file);
            let reader = csv::Reader::from_reader(buf_reader);
            self.spot_source = Some(SpotPriceSource {
                reader: reader.into_deserialize(),
                config_assets: self.config.assets.clone(),
                start_time: self.config.start_time,
                end_time: self.config.end_time,
                exhausted: false,
            });
        } else {
            warn!("Spot prices file not found: {:?}", spot_path);
        }

        // 3. Open book data CSV reader (orderbook snapshots preferred, fallback to aligned prices)
        let ob_path = data_dir.join("polymarket_orderbook_snapshots.csv");
        let aligned_path = data_dir.join("polymarket_aligned_prices.csv");

        if ob_path.exists() {
            let file = File::open(&ob_path)
                .map_err(|e| DataSourceError::Parse(format!("Failed to open {:?}: {}", ob_path, e)))?;
            let buf_reader = BufReader::with_capacity(64 * 1024, file);
            let reader = csv::Reader::from_reader(buf_reader);
            let min_interval = self.config.book_interval_ms
                .filter(|&ms| ms > 0)
                .map(|ms| chrono::Duration::milliseconds(ms as i64));
            self.book_source = Some(BookSource::Orderbook(OrderbookSource {
                reader: reader.into_deserialize(),
                event_tokens,
                start_time: self.config.start_time,
                end_time: self.config.end_time,
                pending: HashMap::new(),
                exhausted: false,
                unpaired_count: 0,
                min_interval,
                last_emitted: HashMap::new(),
            }));
        } else if aligned_path.exists() {
            let file = File::open(&aligned_path)
                .map_err(|e| DataSourceError::Parse(format!("Failed to open {:?}: {}", aligned_path, e)))?;
            let buf_reader = BufReader::with_capacity(64 * 1024, file);
            let reader = csv::Reader::from_reader(buf_reader);
            self.book_source = Some(BookSource::Aligned(AlignedPriceSource {
                reader: reader.into_deserialize(),
                event_tokens,
                start_time: self.config.start_time,
                end_time: self.config.end_time,
                exhausted: false,
            }));
        } else {
            debug!("No book data files found (orderbook snapshots or aligned prices)");
        }

        // 4. Prime the merge heap: get first event from each source
        self.prime_source(SOURCE_WINDOWS);
        self.prime_source(SOURCE_SPOT);
        self.prime_source(SOURCE_BOOK);

        self.initialized = true;
        info!("Streaming CSV replay initialized (merge heap size: {})", self.merge_heap.len());

        Ok(())
    }

    /// Pull the next event from the given source and push it onto the merge heap.
    fn prime_source(&mut self, source_idx: usize) {
        let event = match source_idx {
            SOURCE_WINDOWS => self.window_source.as_mut().and_then(|s| s.next_event()),
            SOURCE_SPOT => self.spot_source.as_mut().and_then(|s| s.next_event()),
            SOURCE_BOOK => self.book_source.as_mut().and_then(|s| s.next_event()),
            _ => None,
        };

        if let Some(ev) = event {
            let ts = ev.timestamp();
            let prio = event_priority(&ev);
            self.merge_heap.push(MergeEntry {
                timestamp: ts,
                priority: prio,
                source_idx,
                event: ev,
            });
        } else {
            let source_name = match source_idx {
                SOURCE_WINDOWS => "windows",
                SOURCE_SPOT => "spot",
                SOURCE_BOOK => "book",
                _ => "unknown",
            };
            eprintln!("  [CSV] Source '{}' exhausted (heap size: {})", source_name, self.merge_heap.len());
        }
    }

    /// Pop the next event from the merge heap and refill from that source.
    fn pop_next(&mut self) -> Option<MarketEvent> {
        let entry = match self.merge_heap.pop() {
            Some(e) => e,
            None => {
                eprintln!("  [CSV] Merge heap empty — stream finished after {} events", self.event_count);
                return None;
            }
        };

        // Enforce max_duration: record first event time and check elapsed
        if let Some(max_dur) = self.max_duration {
            let event_ts = entry.timestamp;
            if self.first_event_time.is_none() {
                self.first_event_time = Some(event_ts);
                info!("Duration limit: first event at {}, will stop after {}s", event_ts, max_dur.num_seconds());
            }
            if let Some(first_ts) = self.first_event_time
                && event_ts - first_ts > max_dur
            {
                info!("Duration limit reached ({:.0}s elapsed), stopping replay",
                    (event_ts - first_ts).num_seconds());
                self.is_exhausted = true;
                self.merge_heap.clear();
                return None;
            }
        }

        // Refill from the source that just produced this event
        self.prime_source(entry.source_idx);
        self.event_count += 1;
        if self.event_count % 500_000 == 0 {
            eprintln!("  [CSV] {}M events processed, ts={}", self.event_count / 1_000_000, entry.timestamp);
        }
        Some(entry.event)
    }
}

#[async_trait]
impl DataSource for CsvReplayDataSource {
    async fn next_event(&mut self) -> Result<Option<MarketEvent>, DataSourceError> {
        // Initialize streaming on first call
        if !self.initialized {
            self.init_streaming()?;
        }

        match self.pop_next() {
            Some(event) => {
                // Apply speed control if configured
                if self.config.speed > 0.0
                    && let Some(prev_time) = self.current_time
                {
                    let delta = event.timestamp() - prev_time;
                    if delta.num_milliseconds() > 0 {
                        let sleep_ms = (delta.num_milliseconds() as f64 / self.config.speed) as u64;
                        if sleep_ms > 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                        }
                    }
                }

                self.current_time = Some(event.timestamp());
                Ok(Some(event))
            }
            None => {
                eprintln!("  [CSV] Data source exhausted after {} events, last_ts={:?}",
                    self.event_count, self.current_time);
                self.is_exhausted = true;
                Ok(None)
            }
        }
    }

    fn has_more(&self) -> bool {
        if self.is_exhausted {
            return false;
        }
        if !self.initialized {
            return true; // Haven't started yet
        }
        !self.merge_heap.is_empty()
    }

    fn current_time(&self) -> Option<DateTime<Utc>> {
        self.current_time
    }

    async fn shutdown(&mut self) {
        self.is_exhausted = true;
        self.merge_heap.clear();
        // Drop sources to close file handles
        self.window_source = None;
        self.spot_source = None;
        self.book_source = None;
    }
}

impl CsvReplayDataSource {
    /// Load all events from CSV files and return them in chronological order.
    ///
    /// This is useful for caching events when running multiple backtests
    /// (e.g., parameter sweeps) to avoid reloading CSV files each time.
    /// Uses streaming internally so no duplicate parsing logic, but still
    /// collects everything into a Vec for Arc sharing.
    pub fn load_all_events(config: CsvReplayConfig) -> Result<Vec<MarketEvent>, DataSourceError> {
        let mut source = CsvReplayDataSource::new(config);
        source.init_streaming()?;

        // Drain the merge heap - events come out in chronological order
        let mut events = Vec::new();
        while let Some(event) = source.pop_next() {
            events.push(event);
        }

        info!("Loaded {} events from CSV files", events.len());
        Ok(events)
    }

    /// Load warmup prices from CSV for ATR initialization.
    ///
    /// Returns spot prices from the CSV file that are BEFORE the start_time,
    /// grouped by asset. This allows warming up indicators before the backtest
    /// starts, ensuring consistent behavior with live trading.
    pub fn load_warmup_prices(
        data_dir: &str,
        start_time: DateTime<Utc>,
        warmup_duration: chrono::Duration,
        assets: &[CryptoAsset],
    ) -> HashMap<CryptoAsset, Vec<Decimal>> {
        let path = Path::new(data_dir).join("binance_spot_prices.csv");
        if !path.exists() {
            warn!("Spot prices file not found for warmup: {:?}", path);
            return HashMap::new();
        }

        let warmup_start = start_time - warmup_duration;
        let mut result: HashMap<CryptoAsset, Vec<Decimal>> = HashMap::new();

        let reader = match csv::Reader::from_path(&path) {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to open spot prices for warmup: {}", e);
                return HashMap::new();
            }
        };

        for row_result in reader.into_deserialize::<SpotPriceRow>() {
            let row = match row_result {
                Ok(r) => r,
                Err(_) => continue,
            };

            // Only include prices in warmup window (before start_time)
            if row.timestamp >= start_time || row.timestamp < warmup_start {
                continue;
            }

            let asset = match parse_asset(&row.asset) {
                Some(a) => a,
                None => continue,
            };

            // Filter by requested assets
            if !assets.is_empty() && !assets.contains(&asset) {
                continue;
            }

            result.entry(asset).or_default().push(row.price);
        }

        // Prices are already in chronological order from the CSV
        // Nothing to sort

        for (asset, prices) in &result {
            debug!("Loaded {} warmup prices for {}", prices.len(), asset);
        }

        result
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Sorted window events paired with their timestamps, plus the event->token mapping.
type WindowLoadResult = (Vec<(DateTime<Utc>, MarketEvent)>, EventTokenMap);

/// Load market windows from CSV and return sorted events + token mapping.
///
/// This is a free function (not a method on CsvReplayDataSource) so it can be
/// called during init_streaming without borrowing self.
fn load_market_windows(
    data_dir: &Path,
    assets: &[CryptoAsset],
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
) -> Result<WindowLoadResult, DataSourceError> {
    let path = data_dir.join("polymarket_market_windows.csv");
    if !path.exists() {
        warn!("Market windows file not found: {:?}", path);
        return Ok((Vec::new(), HashMap::new()));
    }

    let mut reader = csv::Reader::from_path(&path)
        .map_err(|e| DataSourceError::Parse(format!("Failed to open {:?}: {}", path, e)))?;

    let mut event_tokens: EventTokenMap = HashMap::new();
    let mut events: Vec<(DateTime<Utc>, MarketEvent)> = Vec::new();
    let mut seen_events: std::collections::HashSet<String> = std::collections::HashSet::new();

    for result in reader.deserialize() {
        let row: MarketWindowRow = result
            .map_err(|e| DataSourceError::Parse(format!("CSV parse error: {}", e)))?;

        // Deduplicate by event_id (CSV may contain duplicates from multiple collection sessions)
        if !seen_events.insert(row.event_id.clone()) {
            continue;
        }

        // Filter by asset if specified
        let asset = parse_asset(&row.asset);
        if let Some(a) = asset
            && !assets.is_empty()
            && !assets.contains(&a)
        {
            continue;
        }

        // Filter by time if specified
        if let Some(start) = start_time
            && row.window_end < start
        {
            continue;
        }
        if let Some(end) = end_time
            && row.window_start > end
        {
            continue;
        }

        // Skip markets that were already expired when discovered (no live data for them)
        if row.discovered_at > row.window_end {
            continue;
        }

        // Build event -> token mapping for validating book data
        event_tokens.insert(row.event_id.clone(), (row.yes_token_id.clone(), row.no_token_id.clone()));

        if let Some(asset) = asset {
            let open_event = MarketEvent::WindowOpen(WindowOpenEvent {
                event_id: row.event_id.clone(),
                condition_id: row.condition_id.clone(),
                asset,
                yes_token_id: row.yes_token_id,
                no_token_id: row.no_token_id,
                strike_price: row.strike_price,
                window_start: row.window_start,
                window_end: row.window_end,
                timestamp: row.window_start,
            });
            events.push((row.window_start, open_event));

            let close_event = MarketEvent::WindowClose(WindowCloseEvent {
                event_id: row.event_id,
                outcome: None,
                final_price: None,
                timestamp: row.window_end,
            });
            events.push((row.window_end, close_event));
        }
    }

    // Sort by timestamp, then by priority (WindowOpen before WindowClose at same time)
    events.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| event_priority(&a.1).cmp(&event_priority(&b.1)))
    });

    Ok((events, event_tokens))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = CsvReplayConfig::default();
        assert_eq!(config.data_dir, "data");
        assert_eq!(config.assets.len(), 3);
        assert_eq!(config.speed, 0.0);
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
    fn test_merge_entry_ordering() {
        let now = Utc::now();
        let earlier = now - chrono::Duration::seconds(10);
        let later = now + chrono::Duration::seconds(10);

        let entry1 = MergeEntry {
            timestamp: earlier,
            priority: 1,
            source_idx: 0,
            event: MarketEvent::Heartbeat(earlier),
        };
        let entry2 = MergeEntry {
            timestamp: later,
            priority: 1,
            source_idx: 1,
            event: MarketEvent::Heartbeat(later),
        };

        // Min-heap: earlier should come first (reversed ordering)
        assert!(entry1 > entry2);
    }

    #[test]
    fn test_merge_entry_tiebreak() {
        let now = Utc::now();

        // WindowOpen (priority 0) should come before SpotPrice (priority 1) at same timestamp
        let window_open = MergeEntry {
            timestamp: now,
            priority: 0,
            source_idx: 0,
            event: MarketEvent::Heartbeat(now), // placeholder
        };
        let spot = MergeEntry {
            timestamp: now,
            priority: 1,
            source_idx: 1,
            event: MarketEvent::Heartbeat(now),
        };

        // In min-heap, window_open should be "greater" so it gets popped first
        assert!(window_open > spot);
    }

    #[test]
    fn test_event_priority() {
        let now = Utc::now();
        let open = MarketEvent::WindowOpen(WindowOpenEvent {
            event_id: "e".into(),
            condition_id: "c".into(),
            asset: CryptoAsset::Btc,
            yes_token_id: "y".into(),
            no_token_id: "n".into(),
            strike_price: Decimal::ZERO,
            window_start: now,
            window_end: now,
            timestamp: now,
        });
        let close = MarketEvent::WindowClose(WindowCloseEvent {
            event_id: "e".into(),
            outcome: None,
            final_price: None,
            timestamp: now,
        });
        let spot = MarketEvent::SpotPrice(SpotPriceEvent {
            asset: CryptoAsset::Btc,
            price: Decimal::ZERO,
            quantity: Decimal::ZERO,
            timestamp: now,
        });

        assert_eq!(event_priority(&open), 0);
        assert_eq!(event_priority(&spot), 1);
        assert_eq!(event_priority(&close), 2);
    }
}
