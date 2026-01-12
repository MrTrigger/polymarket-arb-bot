//! Polymarket CLOB WebSocket capture for orderbook data.
//!
//! Connects to Polymarket CLOB WebSocket, subscribes to discovered market token IDs,
//! captures orderbook snapshots and deltas, and stores to ClickHouse.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use poly_common::{ClickHouseClient, MarketWindow, OrderBookDelta, OrderBookSnapshot};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{broadcast, RwLock};
use tokio::time::{interval, timeout};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{protocol::Message, Error as WsError},
};
use tracing::{debug, error, info, warn};

/// Polymarket CLOB WebSocket URL.
const CLOB_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

/// Ping interval to keep connection alive (Polymarket requires every 10s).
const PING_INTERVAL: Duration = Duration::from_secs(9);

/// Snapshot capture interval (100ms for high-fidelity backtest data).
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(100);

/// Errors that can occur during CLOB capture.
#[derive(Debug, Error)]
pub enum ClobError {
    #[error("WebSocket connection failed: {0}")]
    Connection(String),

    #[error("WebSocket error: {0}")]
    WebSocket(#[from] WsError),

    #[error("JSON parse error: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("ClickHouse error: {0}")]
    ClickHouse(#[from] poly_common::ClickHouseError),

    #[error("Connection timeout")]
    Timeout,

    #[error("Stream ended unexpectedly")]
    StreamEnded,
}

/// Configuration for the CLOB capture.
#[derive(Debug, Clone)]
pub struct ClobConfig {
    /// Buffer size before flushing snapshots to ClickHouse.
    pub snapshot_buffer_size: usize,
    /// Buffer size before flushing deltas to ClickHouse.
    pub delta_buffer_size: usize,
    /// Maximum time between flushes.
    pub flush_interval: Duration,
    /// Connection timeout.
    pub connect_timeout: Duration,
    /// Initial reconnect delay.
    pub initial_reconnect_delay: Duration,
    /// Maximum reconnect delay.
    pub max_reconnect_delay: Duration,
    /// Interval for periodic snapshot capture.
    pub snapshot_interval: Duration,
}

impl Default for ClobConfig {
    fn default() -> Self {
        Self {
            snapshot_buffer_size: 500,
            delta_buffer_size: 1000,
            flush_interval: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(10),
            initial_reconnect_delay: Duration::from_secs(1),
            max_reconnect_delay: Duration::from_secs(60),
            snapshot_interval: SNAPSHOT_INTERVAL,
        }
    }
}

/// Subscription message to Polymarket CLOB WebSocket.
#[derive(Debug, Serialize)]
struct SubscribeMessage {
    assets_ids: Vec<String>,
    #[serde(rename = "type")]
    msg_type: &'static str,
}

/// Dynamic subscription operation.
#[derive(Debug, Serialize)]
struct SubscriptionOp {
    assets_ids: Vec<String>,
    operation: &'static str,
}

/// Orderbook level from CLOB WebSocket.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderSummary {
    pub price: String,
    pub size: String,
}

/// Book message from CLOB WebSocket.
#[derive(Debug, Clone, Deserialize)]
pub struct BookMessage {
    pub event_type: String,
    pub asset_id: String,
    pub market: String,
    pub timestamp: String,
    pub hash: String,
    pub bids: Vec<OrderSummary>,
    pub asks: Vec<OrderSummary>,
}

/// Price change entry.
#[derive(Debug, Clone, Deserialize)]
pub struct PriceChange {
    pub asset_id: String,
    pub price: String,
    pub size: String,
    pub side: String,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub best_bid: Option<String>,
    #[serde(default)]
    pub best_ask: Option<String>,
}

/// Price change message from CLOB WebSocket.
#[derive(Debug, Clone, Deserialize)]
pub struct PriceChangeMessage {
    pub event_type: String,
    pub asset_id: String,
    pub market: String,
    pub timestamp: String,
    pub price_changes: Vec<PriceChange>,
}

/// Generic message for detecting event type.
#[derive(Debug, Deserialize)]
struct GenericMessage {
    event_type: Option<String>,
}

/// In-memory orderbook state for a single token.
#[derive(Debug, Clone, Default)]
pub struct OrderBookState {
    /// Token ID.
    pub token_id: String,
    /// Event/condition ID.
    pub event_id: String,
    /// Bid levels (price -> size).
    pub bids: HashMap<Decimal, Decimal>,
    /// Ask levels (price -> size).
    pub asks: HashMap<Decimal, Decimal>,
    /// Last update timestamp.
    pub last_update: Option<DateTime<Utc>>,
}

impl OrderBookState {
    pub fn new(token_id: String, event_id: String) -> Self {
        Self {
            token_id,
            event_id,
            bids: HashMap::new(),
            asks: HashMap::new(),
            last_update: None,
        }
    }

    /// Apply a full book snapshot.
    pub fn apply_book(&mut self, book: &BookMessage) {
        self.bids.clear();
        self.asks.clear();

        for level in &book.bids {
            if let (Ok(price), Ok(size)) = (level.price.parse(), level.size.parse()) {
                self.bids.insert(price, size);
            }
        }
        for level in &book.asks {
            if let (Ok(price), Ok(size)) = (level.price.parse(), level.size.parse()) {
                self.asks.insert(price, size);
            }
        }

        self.last_update = parse_timestamp(&book.timestamp);
    }

    /// Apply price changes (deltas).
    pub fn apply_price_change(&mut self, change: &PriceChange) {
        let price: Decimal = match change.price.parse() {
            Ok(p) => p,
            Err(_) => return,
        };
        let size: Decimal = match change.size.parse() {
            Ok(s) => s,
            Err(_) => return,
        };

        let book = match change.side.to_lowercase().as_str() {
            "buy" | "bid" => &mut self.bids,
            "sell" | "ask" => &mut self.asks,
            _ => return,
        };

        if size.is_zero() {
            book.remove(&price);
        } else {
            book.insert(price, size);
        }
    }

    /// Get best bid price and size.
    pub fn best_bid(&self) -> (Decimal, Decimal) {
        self.bids
            .iter()
            .max_by_key(|(p, _)| *p)
            .map(|(p, s)| (*p, *s))
            .unwrap_or((Decimal::ZERO, Decimal::ZERO))
    }

    /// Get best ask price and size.
    pub fn best_ask(&self) -> (Decimal, Decimal) {
        self.asks
            .iter()
            .min_by_key(|(p, _)| *p)
            .map(|(p, s)| (*p, *s))
            .unwrap_or((Decimal::ONE, Decimal::ZERO))
    }

    /// Calculate spread in basis points.
    pub fn spread_bps(&self) -> u32 {
        let (best_bid, _) = self.best_bid();
        let (best_ask, _) = self.best_ask();
        if best_bid.is_zero() || best_ask.is_zero() {
            return 0;
        }
        let spread = best_ask - best_bid;
        let mid = (best_ask + best_bid) / Decimal::TWO;
        if mid.is_zero() {
            return 0;
        }
        // bps = (spread / mid) * 10000
        ((spread / mid) * Decimal::from(10000))
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0) as u32
    }

    /// Calculate total bid depth (sum of sizes).
    pub fn bid_depth(&self) -> Decimal {
        self.bids.values().copied().sum()
    }

    /// Calculate total ask depth (sum of sizes).
    pub fn ask_depth(&self) -> Decimal {
        self.asks.values().copied().sum()
    }

    /// Generate a snapshot record for ClickHouse.
    pub fn to_snapshot(&self, timestamp: DateTime<Utc>) -> OrderBookSnapshot {
        let (best_bid, best_bid_size) = self.best_bid();
        let (best_ask, best_ask_size) = self.best_ask();

        OrderBookSnapshot {
            token_id: self.token_id.clone(),
            event_id: self.event_id.clone(),
            timestamp,
            best_bid,
            best_bid_size,
            best_ask,
            best_ask_size,
            spread_bps: self.spread_bps(),
            bid_depth: self.bid_depth(),
            ask_depth: self.ask_depth(),
        }
    }
}

/// Parse timestamp from milliseconds string.
fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    ts.parse::<i64>()
        .ok()
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
}

/// Shared state for active markets (token_id -> market info).
pub type ActiveMarkets = Arc<RwLock<HashMap<String, MarketInfo>>>;

/// Information about an active market.
#[derive(Debug, Clone)]
pub struct MarketInfo {
    pub event_id: String,
    pub token_id: String,
    pub is_yes: bool,
}

/// Polymarket CLOB WebSocket capture client.
pub struct ClobCapture {
    config: ClobConfig,
    clickhouse: ClickHouseClient,
    /// Shared state of active markets.
    active_markets: ActiveMarkets,
}

impl ClobCapture {
    /// Creates a new CLOB capture client.
    pub fn new(
        config: ClobConfig,
        clickhouse: ClickHouseClient,
        active_markets: ActiveMarkets,
    ) -> Self {
        Self {
            config,
            clickhouse,
            active_markets,
        }
    }

    /// Runs the capture loop with automatic reconnection.
    pub async fn run(&self, mut shutdown: broadcast::Receiver<()>) -> Result<(), ClobError> {
        let mut reconnect_delay = self.config.initial_reconnect_delay;

        loop {
            if shutdown.try_recv().is_ok() {
                info!("CLOB capture: shutdown signal received");
                return Ok(());
            }

            match self.run_connection(&mut shutdown).await {
                Ok(()) => {
                    info!("CLOB capture: clean shutdown");
                    return Ok(());
                }
                Err(e) => {
                    warn!("CLOB capture error: {e}, reconnecting in {reconnect_delay:?}");

                    tokio::select! {
                        _ = tokio::time::sleep(reconnect_delay) => {}
                        _ = shutdown.recv() => {
                            info!("CLOB capture: shutdown during reconnect delay");
                            return Ok(());
                        }
                    }

                    reconnect_delay = (reconnect_delay * 2).min(self.config.max_reconnect_delay);
                }
            }
        }
    }

    /// Runs a single WebSocket connection until error or shutdown.
    async fn run_connection(
        &self,
        shutdown: &mut broadcast::Receiver<()>,
    ) -> Result<(), ClobError> {
        info!("Connecting to Polymarket CLOB WebSocket at {CLOB_WS_URL}");

        let connect_result = timeout(self.config.connect_timeout, connect_async(CLOB_WS_URL)).await;

        let (ws_stream, _response) = match connect_result {
            Ok(Ok((stream, response))) => (stream, response),
            Ok(Err(e)) => {
                return Err(ClobError::Connection(e.to_string()));
            }
            Err(_) => {
                return Err(ClobError::Timeout);
            }
        };

        info!("Connected to Polymarket CLOB WebSocket");

        let (mut write, mut read) = ws_stream.split();

        // Get current active market token IDs
        let token_ids = self.get_active_token_ids().await;

        if token_ids.is_empty() {
            warn!("No active markets to subscribe to, waiting...");
            tokio::time::sleep(Duration::from_secs(10)).await;
            return Err(ClobError::StreamEnded);
        }

        // Subscribe to market data
        let subscribe_msg = SubscribeMessage {
            assets_ids: token_ids.clone(),
            msg_type: "market",
        };

        let msg = serde_json::to_string(&subscribe_msg)?;
        write.send(Message::Text(msg.into())).await?;
        info!("Subscribed to {} market tokens", token_ids.len());

        // Track subscribed tokens
        let mut subscribed: HashSet<String> = token_ids.into_iter().collect();

        // Initialize orderbook state
        let mut orderbooks: HashMap<String, OrderBookState> = HashMap::new();
        for token_id in &subscribed {
            if let Some(info) = self.get_market_info(token_id).await {
                orderbooks.insert(
                    token_id.clone(),
                    OrderBookState::new(token_id.clone(), info.event_id),
                );
            }
        }

        // Buffers for batching writes
        let mut snapshot_buffer: Vec<OrderBookSnapshot> =
            Vec::with_capacity(self.config.snapshot_buffer_size);
        let mut delta_buffer: Vec<OrderBookDelta> =
            Vec::with_capacity(self.config.delta_buffer_size);

        let mut flush_timer = interval(self.config.flush_interval);
        let mut ping_timer = interval(PING_INTERVAL);
        let mut snapshot_timer = interval(self.config.snapshot_interval);
        let mut subscription_check_timer = interval(Duration::from_secs(30));

        let mut stats = CaptureStats::default();

        loop {
            tokio::select! {
                // Handle incoming messages
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_message(
                                &text,
                                &mut orderbooks,
                                &mut delta_buffer,
                                &mut stats,
                            ).await;

                            // Flush delta buffer if full
                            if delta_buffer.len() >= self.config.delta_buffer_size {
                                self.flush_deltas(&mut delta_buffer, &mut stats).await?;
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            debug!("Received ping, sending pong");
                            write.send(Message::Pong(data)).await?;
                        }
                        Some(Ok(Message::Pong(_))) => {
                            debug!("Received pong");
                        }
                        Some(Ok(Message::Close(frame))) => {
                            info!("WebSocket closed by server: {:?}", frame);
                            self.flush_all(&mut snapshot_buffer, &mut delta_buffer, &mut stats).await?;
                            return Err(ClobError::StreamEnded);
                        }
                        Some(Err(e)) => {
                            error!("WebSocket error: {e}");
                            let _ = self.flush_all(&mut snapshot_buffer, &mut delta_buffer, &mut stats).await;
                            return Err(ClobError::WebSocket(e));
                        }
                        None => {
                            warn!("WebSocket stream ended");
                            self.flush_all(&mut snapshot_buffer, &mut delta_buffer, &mut stats).await?;
                            return Err(ClobError::StreamEnded);
                        }
                        _ => {}
                    }
                }

                // Periodic snapshot capture
                _ = snapshot_timer.tick() => {
                    let now = Utc::now();
                    for orderbook in orderbooks.values() {
                        if orderbook.last_update.is_some() {
                            snapshot_buffer.push(orderbook.to_snapshot(now));
                            stats.snapshots_captured += 1;
                        }
                    }

                    if snapshot_buffer.len() >= self.config.snapshot_buffer_size {
                        self.flush_snapshots(&mut snapshot_buffer, &mut stats).await?;
                    }
                }

                // Periodic flush
                _ = flush_timer.tick() => {
                    self.flush_all(&mut snapshot_buffer, &mut delta_buffer, &mut stats).await?;
                    info!(
                        "CLOB capture stats: {} snapshots, {} deltas, {} books, {} errors",
                        stats.snapshots_written, stats.deltas_written,
                        stats.books_received, stats.write_errors
                    );
                }

                // Keepalive ping
                _ = ping_timer.tick() => {
                    debug!("Sending PING");
                    write.send(Message::Text("PING".into())).await?;
                }

                // Check for new markets to subscribe
                _ = subscription_check_timer.tick() => {
                    let current_tokens = self.get_active_token_ids().await;
                    let new_tokens: Vec<String> = current_tokens
                        .into_iter()
                        .filter(|t| !subscribed.contains(t))
                        .collect();

                    if !new_tokens.is_empty() {
                        info!("Subscribing to {} new market tokens", new_tokens.len());

                        let sub_op = SubscriptionOp {
                            assets_ids: new_tokens.clone(),
                            operation: "subscribe",
                        };
                        let msg = serde_json::to_string(&sub_op)?;
                        write.send(Message::Text(msg.into())).await?;

                        for token_id in new_tokens {
                            if let Some(info) = self.get_market_info(&token_id).await {
                                orderbooks.insert(
                                    token_id.clone(),
                                    OrderBookState::new(token_id.clone(), info.event_id),
                                );
                            }
                            subscribed.insert(token_id);
                        }
                    }
                }

                // Shutdown signal
                _ = shutdown.recv() => {
                    info!("CLOB capture: shutdown signal received");
                    let _ = self.flush_all(&mut snapshot_buffer, &mut delta_buffer, &mut stats).await;
                    return Ok(());
                }
            }
        }
    }

    /// Handle an incoming WebSocket message.
    async fn handle_message(
        &self,
        text: &str,
        orderbooks: &mut HashMap<String, OrderBookState>,
        delta_buffer: &mut Vec<OrderBookDelta>,
        stats: &mut CaptureStats,
    ) {
        // Try to detect message type
        let generic: GenericMessage = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(_) => {
                // Might be a non-JSON message like PONG response
                debug!("Non-JSON message: {}", text);
                return;
            }
        };

        match generic.event_type.as_deref() {
            Some("book") => {
                self.handle_book_message(text, orderbooks, stats).await;
            }
            Some("price_change") => {
                self.handle_price_change(text, orderbooks, delta_buffer, stats)
                    .await;
            }
            Some("last_trade_price") => {
                // We don't need trade price for orderbook capture
                debug!("Ignoring last_trade_price message");
            }
            Some("tick_size_change") => {
                debug!("Received tick_size_change message");
            }
            _ => {
                debug!("Unknown message type: {:?}", generic.event_type);
            }
        }
    }

    /// Handle a book (full snapshot) message.
    async fn handle_book_message(
        &self,
        text: &str,
        orderbooks: &mut HashMap<String, OrderBookState>,
        stats: &mut CaptureStats,
    ) {
        let book: BookMessage = match serde_json::from_str(text) {
            Ok(b) => b,
            Err(e) => {
                warn!("Failed to parse book message: {e}");
                return;
            }
        };

        stats.books_received += 1;

        let token_id = &book.asset_id;

        if let Some(orderbook) = orderbooks.get_mut(token_id) {
            orderbook.apply_book(&book);
            debug!(
                "Applied book for token {}: {} bids, {} asks",
                token_id,
                book.bids.len(),
                book.asks.len()
            );
        } else {
            // Create new orderbook state if we don't have one
            if let Some(info) = self.get_market_info(token_id).await {
                let mut orderbook = OrderBookState::new(token_id.clone(), info.event_id);
                orderbook.apply_book(&book);
                orderbooks.insert(token_id.clone(), orderbook);
                debug!("Created new orderbook for token {}", token_id);
            }
        }
    }

    /// Handle a price_change (delta) message.
    async fn handle_price_change(
        &self,
        text: &str,
        orderbooks: &mut HashMap<String, OrderBookState>,
        delta_buffer: &mut Vec<OrderBookDelta>,
        stats: &mut CaptureStats,
    ) {
        let msg: PriceChangeMessage = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(e) => {
                warn!("Failed to parse price_change message: {e}");
                return;
            }
        };

        let timestamp = parse_timestamp(&msg.timestamp).unwrap_or_else(Utc::now);
        let token_id = &msg.asset_id;
        let event_id = &msg.market;

        for change in &msg.price_changes {
            // Update in-memory orderbook
            if let Some(orderbook) = orderbooks.get_mut(token_id) {
                orderbook.apply_price_change(change);
                orderbook.last_update = Some(timestamp);
            }

            // Create delta record for storage
            let price: Decimal = change.price.parse().unwrap_or(Decimal::ZERO);
            let size: Decimal = change.size.parse().unwrap_or(Decimal::ZERO);

            delta_buffer.push(OrderBookDelta {
                token_id: token_id.clone(),
                event_id: event_id.clone(),
                timestamp,
                side: change.side.clone(),
                price,
                size,
            });

            stats.deltas_received += 1;
        }
    }

    /// Get all active token IDs from the shared state.
    async fn get_active_token_ids(&self) -> Vec<String> {
        let markets = self.active_markets.read().await;
        markets.keys().cloned().collect()
    }

    /// Get market info for a token ID.
    async fn get_market_info(&self, token_id: &str) -> Option<MarketInfo> {
        let markets = self.active_markets.read().await;
        markets.get(token_id).cloned()
    }

    /// Flush snapshot buffer to ClickHouse.
    async fn flush_snapshots(
        &self,
        buffer: &mut Vec<OrderBookSnapshot>,
        stats: &mut CaptureStats,
    ) -> Result<(), ClobError> {
        if buffer.is_empty() {
            return Ok(());
        }

        let count = buffer.len();
        debug!("Flushing {} snapshots to ClickHouse", count);

        match self.clickhouse.insert_snapshots(buffer).await {
            Ok(()) => {
                stats.snapshots_written += count as u64;
                buffer.clear();
                Ok(())
            }
            Err(e) => {
                stats.write_errors += 1;
                error!("Failed to write snapshots to ClickHouse: {e}");
                Err(ClobError::ClickHouse(e))
            }
        }
    }

    /// Flush delta buffer to ClickHouse.
    async fn flush_deltas(
        &self,
        buffer: &mut Vec<OrderBookDelta>,
        stats: &mut CaptureStats,
    ) -> Result<(), ClobError> {
        if buffer.is_empty() {
            return Ok(());
        }

        let count = buffer.len();
        debug!("Flushing {} deltas to ClickHouse", count);

        match self.clickhouse.insert_deltas(buffer).await {
            Ok(()) => {
                stats.deltas_written += count as u64;
                buffer.clear();
                Ok(())
            }
            Err(e) => {
                stats.write_errors += 1;
                error!("Failed to write deltas to ClickHouse: {e}");
                Err(ClobError::ClickHouse(e))
            }
        }
    }

    /// Flush all buffers.
    async fn flush_all(
        &self,
        snapshot_buffer: &mut Vec<OrderBookSnapshot>,
        delta_buffer: &mut Vec<OrderBookDelta>,
        stats: &mut CaptureStats,
    ) -> Result<(), ClobError> {
        self.flush_snapshots(snapshot_buffer, stats).await?;
        self.flush_deltas(delta_buffer, stats).await?;
        Ok(())
    }
}

/// Statistics for the capture session.
#[derive(Debug, Default)]
struct CaptureStats {
    books_received: u64,
    snapshots_captured: u64,
    snapshots_written: u64,
    deltas_received: u64,
    deltas_written: u64,
    write_errors: u64,
}

/// Helper to update active markets from discovered MarketWindows.
pub async fn update_active_markets(active_markets: &ActiveMarkets, windows: &[MarketWindow]) {
    let mut markets = active_markets.write().await;

    for window in windows {
        // Add YES token
        markets.insert(
            window.yes_token_id.clone(),
            MarketInfo {
                event_id: window.event_id.clone(),
                token_id: window.yes_token_id.clone(),
                is_yes: true,
            },
        );

        // Add NO token
        markets.insert(
            window.no_token_id.clone(),
            MarketInfo {
                event_id: window.event_id.clone(),
                token_id: window.no_token_id.clone(),
                is_yes: false,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_orderbook_state_apply_book() {
        let mut state = OrderBookState::new("token1".to_string(), "event1".to_string());

        let book = BookMessage {
            event_type: "book".to_string(),
            asset_id: "token1".to_string(),
            market: "event1".to_string(),
            timestamp: "1704067200000".to_string(),
            hash: "abc123".to_string(),
            bids: vec![
                OrderSummary {
                    price: "0.45".to_string(),
                    size: "100".to_string(),
                },
                OrderSummary {
                    price: "0.44".to_string(),
                    size: "200".to_string(),
                },
            ],
            asks: vec![
                OrderSummary {
                    price: "0.55".to_string(),
                    size: "150".to_string(),
                },
                OrderSummary {
                    price: "0.56".to_string(),
                    size: "250".to_string(),
                },
            ],
        };

        state.apply_book(&book);

        assert_eq!(state.bids.len(), 2);
        assert_eq!(state.asks.len(), 2);

        let (best_bid, best_bid_size) = state.best_bid();
        assert_eq!(best_bid, dec!(0.45));
        assert_eq!(best_bid_size, dec!(100));

        let (best_ask, best_ask_size) = state.best_ask();
        assert_eq!(best_ask, dec!(0.55));
        assert_eq!(best_ask_size, dec!(150));
    }

    #[test]
    fn test_orderbook_state_apply_price_change() {
        let mut state = OrderBookState::new("token1".to_string(), "event1".to_string());
        state.bids.insert(dec!(0.45), dec!(100));

        // Update existing level
        let change = PriceChange {
            asset_id: "token1".to_string(),
            price: "0.45".to_string(),
            size: "150".to_string(),
            side: "buy".to_string(),
            hash: None,
            best_bid: None,
            best_ask: None,
        };
        state.apply_price_change(&change);
        assert_eq!(state.bids.get(&dec!(0.45)), Some(&dec!(150)));

        // Remove level (size = 0)
        let change = PriceChange {
            asset_id: "token1".to_string(),
            price: "0.45".to_string(),
            size: "0".to_string(),
            side: "buy".to_string(),
            hash: None,
            best_bid: None,
            best_ask: None,
        };
        state.apply_price_change(&change);
        assert!(state.bids.get(&dec!(0.45)).is_none());
    }

    #[test]
    fn test_orderbook_state_depth() {
        let mut state = OrderBookState::new("token1".to_string(), "event1".to_string());
        state.bids.insert(dec!(0.45), dec!(100));
        state.bids.insert(dec!(0.44), dec!(200));
        state.asks.insert(dec!(0.55), dec!(150));
        state.asks.insert(dec!(0.56), dec!(250));

        assert_eq!(state.bid_depth(), dec!(300));
        assert_eq!(state.ask_depth(), dec!(400));
    }

    #[test]
    fn test_orderbook_state_spread_bps() {
        let mut state = OrderBookState::new("token1".to_string(), "event1".to_string());
        state.bids.insert(dec!(0.48), dec!(100));
        state.asks.insert(dec!(0.52), dec!(100));

        // Spread = 0.04, mid = 0.50, spread_bps = 0.04/0.50 * 10000 = 800
        let spread = state.spread_bps();
        assert_eq!(spread, 800);
    }

    #[test]
    fn test_orderbook_state_to_snapshot() {
        let mut state = OrderBookState::new("token1".to_string(), "event1".to_string());
        state.bids.insert(dec!(0.45), dec!(100));
        state.asks.insert(dec!(0.55), dec!(150));

        let now = Utc::now();
        let snapshot = state.to_snapshot(now);

        assert_eq!(snapshot.token_id, "token1");
        assert_eq!(snapshot.event_id, "event1");
        assert_eq!(snapshot.best_bid, dec!(0.45));
        assert_eq!(snapshot.best_bid_size, dec!(100));
        assert_eq!(snapshot.best_ask, dec!(0.55));
        assert_eq!(snapshot.best_ask_size, dec!(150));
    }

    #[test]
    fn test_parse_timestamp() {
        let ts = parse_timestamp("1704067200000");
        assert!(ts.is_some());
        let dt = ts.unwrap();
        assert_eq!(dt.timestamp_millis(), 1704067200000);
    }

    #[test]
    fn test_default_config() {
        let config = ClobConfig::default();
        assert_eq!(config.snapshot_buffer_size, 500);
        assert_eq!(config.delta_buffer_size, 1000);
        assert_eq!(config.snapshot_interval, Duration::from_millis(100));
    }

    #[test]
    fn test_subscribe_message_serialization() {
        let msg = SubscribeMessage {
            assets_ids: vec!["token1".to_string(), "token2".to_string()],
            msg_type: "market",
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"assets_ids\""));
        assert!(json.contains("\"type\":\"market\""));
    }

    #[test]
    fn test_book_message_parsing() {
        let json = r#"{
            "event_type": "book",
            "asset_id": "token123",
            "market": "cond456",
            "timestamp": "1704067200000",
            "hash": "abc123",
            "bids": [{"price": "0.45", "size": "100"}],
            "asks": [{"price": "0.55", "size": "150"}]
        }"#;

        let book: BookMessage = serde_json::from_str(json).unwrap();
        assert_eq!(book.event_type, "book");
        assert_eq!(book.asset_id, "token123");
        assert_eq!(book.bids.len(), 1);
        assert_eq!(book.asks.len(), 1);
    }

    #[test]
    fn test_price_change_message_parsing() {
        let json = r#"{
            "event_type": "price_change",
            "asset_id": "token123",
            "market": "cond456",
            "timestamp": "1704067200000",
            "price_changes": [
                {
                    "asset_id": "token123",
                    "price": "0.46",
                    "size": "50",
                    "side": "buy"
                }
            ]
        }"#;

        let msg: PriceChangeMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.event_type, "price_change");
        assert_eq!(msg.price_changes.len(), 1);
        assert_eq!(msg.price_changes[0].price, "0.46");
    }
}
