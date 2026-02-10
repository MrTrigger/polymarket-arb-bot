//! Live data source using WebSocket connections.
//!
//! Connects to Binance WebSocket for spot prices and uses the official
//! Polymarket SDK WebSocket for orderbook data.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{debug, error, info, trace, warn};

use alloy::primitives::U256;
use poly_common::types::CryptoAsset;
use polymarket_client_sdk::clob::ws::Client as PolyWsClient;

use super::{BookSnapshotEvent, DataSource, DataSourceError, MarketEvent, SpotPriceEvent};
use crate::types::PriceLevel;

/// Binance WebSocket URL.
const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws";

/// Configuration for the live data source.
#[derive(Debug, Clone)]
pub struct LiveDataSourceConfig {
    /// Assets to track.
    pub assets: Vec<CryptoAsset>,
    /// Connection timeout.
    pub connect_timeout: Duration,
    /// Heartbeat interval.
    pub heartbeat_interval: Duration,
    /// Event channel buffer size.
    pub event_buffer_size: usize,
    /// Binance ping interval.
    pub binance_ping_interval: Duration,
}

impl Default for LiveDataSourceConfig {
    fn default() -> Self {
        Self {
            assets: vec![
                CryptoAsset::Btc,
                CryptoAsset::Eth,
                CryptoAsset::Sol,
                CryptoAsset::Xrp,
            ],
            connect_timeout: Duration::from_secs(10),
            heartbeat_interval: Duration::from_secs(5),
            event_buffer_size: 10_000,
            binance_ping_interval: Duration::from_secs(30),
        }
    }
}

/// Information about an active market for subscriptions.
#[derive(Debug, Clone)]
pub struct ActiveMarket {
    /// Event ID.
    pub event_id: String,
    /// YES token ID.
    pub yes_token_id: String,
    /// NO token ID.
    pub no_token_id: String,
    /// Asset.
    pub asset: CryptoAsset,
    /// Strike price.
    pub strike_price: Decimal,
    /// Window end time.
    pub window_end: DateTime<Utc>,
}

/// Shared state for active markets.
pub type ActiveMarketsState = Arc<RwLock<HashMap<String, ActiveMarket>>>;

/// Event sender for injecting events into the data source.
pub type EventSender = mpsc::Sender<MarketEvent>;

/// Live data source that connects to real WebSocket feeds.
pub struct LiveDataSource {
    #[allow(dead_code)]
    config: LiveDataSourceConfig,
    event_rx: mpsc::Receiver<MarketEvent>,
    event_tx: EventSender,
    shutdown_tx: broadcast::Sender<()>,
    active_markets: ActiveMarketsState,
    is_running: bool,
}

impl LiveDataSource {
    /// Creates a new live data source.
    pub fn new(config: LiveDataSourceConfig) -> Self {
        let (event_tx, event_rx) = mpsc::channel(config.event_buffer_size);
        let (shutdown_tx, _) = broadcast::channel(16);
        let active_markets: ActiveMarketsState = Arc::new(RwLock::new(HashMap::new()));

        // Spawn Binance connection task
        let binance_shutdown = shutdown_tx.subscribe();
        let binance_tx = event_tx.clone();
        let binance_config = config.clone();
        tokio::spawn(async move {
            if let Err(e) =
                run_binance_connection(binance_config, binance_tx, binance_shutdown).await
            {
                error!("Binance connection error: {}", e);
            }
        });

        // Spawn Polymarket SDK WebSocket connection task
        let clob_shutdown = shutdown_tx.subscribe();
        let clob_tx = event_tx.clone();
        let clob_markets = Arc::clone(&active_markets);
        tokio::spawn(async move {
            if let Err(e) = run_sdk_clob_connection(clob_tx, clob_markets, clob_shutdown).await {
                error!("CLOB SDK connection error: {}", e);
            }
        });

        // Spawn Polymarket RTDS Chainlink price connection
        let rtds_shutdown = shutdown_tx.subscribe();
        let rtds_tx = event_tx.clone();
        let rtds_assets = config.assets.clone();
        tokio::spawn(async move {
            if let Err(e) =
                run_rtds_chainlink_connection(rtds_assets, rtds_tx, rtds_shutdown).await
            {
                error!("RTDS Chainlink connection error: {}", e);
            }
        });

        // Keep a sender for external event injection (must clone before moving to heartbeat)
        let external_event_tx = event_tx.clone();

        // Spawn heartbeat task
        let heartbeat_shutdown = shutdown_tx.subscribe();
        let heartbeat_tx = event_tx;
        let heartbeat_interval = config.heartbeat_interval;
        tokio::spawn(async move {
            run_heartbeat(heartbeat_interval, heartbeat_tx, heartbeat_shutdown).await;
        });

        Self {
            config,
            event_rx,
            event_tx: external_event_tx,
            shutdown_tx,
            active_markets,
            is_running: true,
        }
    }

    /// Add an active market to track.
    pub async fn add_market(&self, market: ActiveMarket) {
        let mut markets = self.active_markets.write().await;
        markets.insert(market.event_id.clone(), market);
    }

    /// Remove a market from tracking.
    pub async fn remove_market(&self, event_id: &str) {
        let mut markets = self.active_markets.write().await;
        markets.remove(event_id);
    }

    /// Get all active markets.
    pub async fn get_markets(&self) -> Vec<ActiveMarket> {
        let markets = self.active_markets.read().await;
        markets.values().cloned().collect()
    }

    /// Get a handle to the active markets state for external use (e.g., discovery).
    ///
    /// This allows external components to add markets to the data source
    /// even after the data source has been moved into a strategy loop.
    pub fn active_markets_handle(&self) -> ActiveMarketsState {
        Arc::clone(&self.active_markets)
    }

    /// Get an event sender for injecting events (e.g., WindowOpen from discovery).
    pub fn event_sender(&self) -> EventSender {
        self.event_tx.clone()
    }
}

#[async_trait]
impl DataSource for LiveDataSource {
    async fn next_event(&mut self) -> Result<Option<MarketEvent>, DataSourceError> {
        match self.event_rx.recv().await {
            Some(event) => Ok(Some(event)),
            None => {
                self.is_running = false;
                Ok(None)
            }
        }
    }

    fn has_more(&self) -> bool {
        self.is_running
    }

    fn current_time(&self) -> Option<DateTime<Utc>> {
        None // Live data source uses real time
    }

    async fn shutdown(&mut self) {
        self.is_running = false;
        let _ = self.shutdown_tx.send(());
    }
}

/// Run heartbeat task.
async fn run_heartbeat(
    interval_duration: Duration,
    tx: mpsc::Sender<MarketEvent>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut ticker = interval(interval_duration);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let event = MarketEvent::Heartbeat(Utc::now());
                if tx.send(event).await.is_err() {
                    break;
                }
            }
            _ = shutdown.recv() => {
                debug!("Heartbeat task shutting down");
                break;
            }
        }
    }
}

/// Binance trade message.
#[derive(Debug, Deserialize)]
struct BinanceTrade {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "q")]
    quantity: String,
    #[serde(rename = "T")]
    timestamp: i64,
}

/// Run Binance WebSocket connection.
async fn run_binance_connection(
    config: LiveDataSourceConfig,
    tx: mpsc::Sender<MarketEvent>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), DataSourceError> {
    let mut reconnect_delay = Duration::from_secs(1);
    let max_reconnect_delay = Duration::from_secs(60);

    loop {
        if shutdown.try_recv().is_ok() {
            info!("Binance connection: shutdown signal received");
            return Ok(());
        }

        match run_binance_session(&config, &tx, &mut shutdown).await {
            Ok(()) => {
                info!("Binance connection: clean shutdown");
                return Ok(());
            }
            Err(e) => {
                warn!(
                    "Binance connection error: {}, reconnecting in {:?}",
                    e, reconnect_delay
                );

                tokio::select! {
                    _ = tokio::time::sleep(reconnect_delay) => {}
                    _ = shutdown.recv() => {
                        info!("Binance connection: shutdown during reconnect");
                        return Ok(());
                    }
                }

                reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
            }
        }
    }
}

/// Run a single Binance WebSocket session.
async fn run_binance_session(
    config: &LiveDataSourceConfig,
    tx: &mpsc::Sender<MarketEvent>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<(), DataSourceError> {
    info!("Connecting to Binance WebSocket at {}", BINANCE_WS_URL);

    let connect_result =
        tokio::time::timeout(config.connect_timeout, connect_async(BINANCE_WS_URL)).await;

    let (ws_stream, _) = match connect_result {
        Ok(Ok((stream, response))) => (stream, response),
        Ok(Err(e)) => return Err(DataSourceError::Connection(e.to_string())),
        Err(_) => return Err(DataSourceError::Timeout),
    };

    info!("Connected to Binance WebSocket");

    let (mut write, mut read) = ws_stream.split();

    // Subscribe to trade streams
    let streams: Vec<String> = config
        .assets
        .iter()
        .map(|a| format!("{}@trade", a.binance_symbol()))
        .collect();

    let subscribe_msg = serde_json::json!({
        "method": "SUBSCRIBE",
        "params": streams,
        "id": 1
    });

    write
        .send(Message::Text(subscribe_msg.to_string()))
        .await
        .map_err(|e| DataSourceError::WebSocket(e.to_string()))?;

    info!("Subscribed to {} Binance streams", streams.len());

    let mut ping_timer = interval(config.binance_ping_interval);

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(event) = parse_binance_trade(&text)
                            && tx.send(event).await.is_err()
                        {
                            return Err(DataSourceError::StreamEnded);
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        write.send(Message::Pong(data)).await
                            .map_err(|e| DataSourceError::WebSocket(e.to_string()))?;
                    }
                    Some(Ok(Message::Close(_))) => {
                        return Err(DataSourceError::StreamEnded);
                    }
                    Some(Err(e)) => {
                        return Err(DataSourceError::WebSocket(e.to_string()));
                    }
                    None => {
                        return Err(DataSourceError::StreamEnded);
                    }
                    _ => {}
                }
            }
            _ = ping_timer.tick() => {
                write.send(Message::Ping(vec![])).await
                    .map_err(|e| DataSourceError::WebSocket(e.to_string()))?;
            }
            _ = shutdown.recv() => {
                info!("Binance session: shutdown signal received");
                return Ok(());
            }
        }
    }
}

/// Parse a Binance trade message into a MarketEvent.
fn parse_binance_trade(text: &str) -> Option<MarketEvent> {
    // Skip subscription response
    if text.contains("\"result\":null") {
        return None;
    }

    let trade: BinanceTrade = serde_json::from_str(text).ok()?;

    let asset = match trade.symbol.to_uppercase().as_str() {
        "BTCUSDT" => CryptoAsset::Btc,
        "ETHUSDT" => CryptoAsset::Eth,
        "SOLUSDT" => CryptoAsset::Sol,
        "XRPUSDT" => CryptoAsset::Xrp,
        _ => return None,
    };

    let price: Decimal = trade.price.parse().ok()?;
    let quantity: Decimal = trade.quantity.parse().ok()?;
    let timestamp = Utc.timestamp_millis_opt(trade.timestamp).single()?;

    Some(MarketEvent::SpotPrice(SpotPriceEvent {
        asset,
        price,
        quantity,
        timestamp,
    }))
}

// ============================================================================
// Polymarket SDK WebSocket Connection
// ============================================================================

/// Run Polymarket CLOB WebSocket connection using the official SDK.
async fn run_sdk_clob_connection(
    tx: mpsc::Sender<MarketEvent>,
    active_markets: ActiveMarketsState,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), DataSourceError> {
    let mut reconnect_delay = Duration::from_secs(1);
    let max_reconnect_delay = Duration::from_secs(60);
    let mut logged_waiting = false;

    loop {
        if shutdown.try_recv().is_ok() {
            info!("CLOB SDK connection: shutdown signal received");
            return Ok(());
        }

        // Wait for markets before connecting - no point connecting with nothing to subscribe to
        let token_ids = get_token_ids(&active_markets).await;
        if token_ids.is_empty() {
            if !logged_waiting {
                info!("CLOB SDK connection: waiting for markets to be discovered...");
                logged_waiting = true;
            }
            // Poll for markets every 5 seconds
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    continue;
                }
                _ = shutdown.recv() => {
                    info!("CLOB SDK connection: shutdown while waiting for markets");
                    return Ok(());
                }
            }
        }

        // Markets available, reset the waiting flag
        logged_waiting = false;

        match run_sdk_clob_session(&tx, &active_markets, &mut shutdown).await {
            Ok(()) => {
                info!("CLOB SDK connection: clean shutdown");
                return Ok(());
            }
            Err(e) => {
                // Check if we still have markets - if not, go back to waiting state
                let current_tokens = get_token_ids(&active_markets).await;
                if current_tokens.is_empty() {
                    debug!("CLOB SDK connection closed, no active markets");
                    reconnect_delay = Duration::from_secs(1);
                    continue;
                }

                // Intentional reconnect (new subscriptions) vs real error
                let is_resubscribe = matches!(&e, DataSourceError::Connection(msg) if msg.contains("Reconnecting"));
                if is_resubscribe {
                    info!("CLOB SDK: reconnecting to add new market subscriptions");
                    reconnect_delay = Duration::from_secs(1);
                } else {
                    warn!(
                        "CLOB SDK connection error: {}, reconnecting in {:?}",
                        e, reconnect_delay
                    );
                    reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
                }

                tokio::select! {
                    _ = tokio::time::sleep(reconnect_delay) => {}
                    _ = shutdown.recv() => {
                        info!("CLOB SDK connection: shutdown during reconnect");
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Run a single CLOB WebSocket session using the SDK.
async fn run_sdk_clob_session(
    tx: &mpsc::Sender<MarketEvent>,
    active_markets: &ActiveMarketsState,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<(), DataSourceError> {
    info!("Connecting to Polymarket CLOB WebSocket via SDK");

    // Create SDK WebSocket client
    let client = PolyWsClient::default();

    // Get token IDs to subscribe to (as strings)
    let token_id_strings = get_token_ids(active_markets).await;

    if token_id_strings.is_empty() {
        return Err(DataSourceError::Connection(
            "No tokens to subscribe to".to_string(),
        ));
    }

    // Convert string token IDs to U256 for SDK
    let token_ids: Vec<U256> = token_id_strings
        .iter()
        .filter_map(|s| U256::from_str_radix(s, 10).ok())
        .collect();

    if token_ids.is_empty() {
        return Err(DataSourceError::Connection(
            "Failed to parse any token IDs".to_string(),
        ));
    }

    info!("Subscribing to {} tokens via SDK", token_ids.len());

    // Build token_id -> event_id mapping (using string keys)
    let token_event_map = build_token_event_map(active_markets).await;

    // Subscribe to orderbook updates using the SDK
    let stream = client
        .subscribe_orderbook(token_ids)
        .map_err(|e| DataSourceError::Connection(format!("SDK subscription failed: {}", e)))?;

    let mut stream = Box::pin(stream);

    info!("Connected and subscribed to CLOB via SDK");

    // Track subscribed tokens for dynamic subscription updates
    let subscribed_tokens: std::collections::HashSet<String> =
        token_id_strings.into_iter().collect();
    let mut subscription_check = interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            book_result = stream.next() => {
                match book_result {
                    Some(Ok(book)) => {
                        // Convert U256 asset_id to string for our internal types
                        let asset_id_str = book.asset_id.to_string();

                        // Look up event_id from our mapping
                        let event_id = token_event_map
                            .get(&asset_id_str)
                            .cloned()
                            .unwrap_or_else(|| format!("{:?}", book.market));

                        // SDK price/size are already Decimal
                        let bids: Vec<PriceLevel> = book
                            .bids
                            .iter()
                            .map(|b| PriceLevel::new(b.price, b.size))
                            .collect();

                        let asks: Vec<PriceLevel> = book
                            .asks
                            .iter()
                            .map(|a| PriceLevel::new(a.price, a.size))
                            .collect();

                        // Use timestamp from SDK (i64 milliseconds) or current time
                        let timestamp = Utc
                            .timestamp_millis_opt(book.timestamp)
                            .single()
                            .unwrap_or_else(Utc::now);

                        let event = MarketEvent::BookSnapshot(BookSnapshotEvent {
                            token_id: asset_id_str.clone(),
                            event_id,
                            bids,
                            asks,
                            timestamp,
                        });

                        trace!(
                            token_id = %asset_id_str,
                            bids = book.bids.len(),
                            asks = book.asks.len(),
                            "Received orderbook update from SDK"
                        );

                        if tx.send(event).await.is_err() {
                            return Err(DataSourceError::StreamEnded);
                        }
                    }
                    Some(Err(e)) => {
                        return Err(DataSourceError::WebSocket(format!("SDK stream error: {}", e)));
                    }
                    None => {
                        return Err(DataSourceError::StreamEnded);
                    }
                }
            }
            _ = subscription_check.tick() => {
                // Check for new markets - need to reconnect to add new subscriptions
                let current_tokens = get_token_ids(active_markets).await;
                let new_tokens: Vec<String> = current_tokens
                    .iter()
                    .filter(|t| !subscribed_tokens.contains(*t))
                    .cloned()
                    .collect();

                if !new_tokens.is_empty() {
                    info!(
                        "New markets discovered ({} tokens), reconnecting to add subscriptions",
                        new_tokens.len()
                    );
                    // Return to trigger reconnection with new subscriptions
                    return Err(DataSourceError::Connection(
                        "Reconnecting to add new market subscriptions".to_string(),
                    ));
                }
            }
            _ = shutdown.recv() => {
                info!("CLOB SDK session: shutdown signal received");
                return Ok(());
            }
        }
    }
}

/// Get all token IDs from active markets.
async fn get_token_ids(active_markets: &ActiveMarketsState) -> Vec<String> {
    let markets = active_markets.read().await;
    markets
        .values()
        .flat_map(|m| vec![m.yes_token_id.clone(), m.no_token_id.clone()])
        .collect()
}

/// Build a mapping from token_id to event_id.
async fn build_token_event_map(active_markets: &ActiveMarketsState) -> HashMap<String, String> {
    let markets = active_markets.read().await;
    let mut map = HashMap::new();
    for market in markets.values() {
        map.insert(market.yes_token_id.clone(), market.event_id.clone());
        map.insert(market.no_token_id.clone(), market.event_id.clone());
    }
    map
}

// ============================================================================
// Polymarket RTDS WebSocket Connection (Chainlink Prices)
// ============================================================================

/// Polymarket RTDS WebSocket URL for real-time data streams.
const RTDS_WS_URL: &str = "wss://ws-live-data.polymarket.com";

/// RTDS message payload for Chainlink price updates.
#[derive(Debug, Deserialize)]
struct RtdsMessage {
    #[serde(default)]
    topic: String,
    #[serde(default)]
    payload: Option<RtdsPayload>,
}

/// Payload within an RTDS message.
#[derive(Debug, Deserialize)]
struct RtdsPayload {
    symbol: String,
    /// Float value (e.g. 67979.63546244064).
    value: f64,
    timestamp: i64,
}

/// Run RTDS Chainlink WebSocket connection with reconnection logic.
async fn run_rtds_chainlink_connection(
    assets: Vec<CryptoAsset>,
    tx: mpsc::Sender<MarketEvent>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), DataSourceError> {
    let mut reconnect_delay = Duration::from_secs(1);
    let max_reconnect_delay = Duration::from_secs(60);

    loop {
        if shutdown.try_recv().is_ok() {
            info!("RTDS Chainlink connection: shutdown signal received");
            return Ok(());
        }

        match run_rtds_chainlink_session(&assets, &tx, &mut shutdown).await {
            Ok(()) => {
                info!("RTDS Chainlink connection: clean shutdown");
                return Ok(());
            }
            Err(e) => {
                warn!(
                    "RTDS Chainlink connection error: {}, reconnecting in {:?}",
                    e, reconnect_delay
                );

                tokio::select! {
                    _ = tokio::time::sleep(reconnect_delay) => {}
                    _ = shutdown.recv() => {
                        info!("RTDS Chainlink connection: shutdown during reconnect");
                        return Ok(());
                    }
                }

                reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
            }
        }
    }
}

/// Run a single RTDS Chainlink WebSocket session.
async fn run_rtds_chainlink_session(
    assets: &[CryptoAsset],
    tx: &mpsc::Sender<MarketEvent>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<(), DataSourceError> {
    info!("Connecting to Polymarket RTDS at {}", RTDS_WS_URL);

    let connect_result =
        tokio::time::timeout(Duration::from_secs(10), connect_async(RTDS_WS_URL)).await;

    let (ws_stream, _) = match connect_result {
        Ok(Ok((stream, response))) => (stream, response),
        Ok(Err(e)) => return Err(DataSourceError::Connection(e.to_string())),
        Err(_) => return Err(DataSourceError::Timeout),
    };

    info!("Connected to Polymarket RTDS WebSocket");

    let (mut write, mut read) = ws_stream.split();

    // Send initial ping (required by RTDS protocol before subscribing)
    write
        .send(Message::Text("ping".to_string()))
        .await
        .map_err(|e| DataSourceError::WebSocket(e.to_string()))?;

    // Subscribe to all Chainlink price updates (no per-symbol filter needed)
    let subscribe_msg = serde_json::json!({
        "action": "subscribe",
        "subscriptions": [{
            "topic": "crypto_prices_chainlink",
            "type": "update"
        }],
    });

    write
        .send(Message::Text(subscribe_msg.to_string()))
        .await
        .map_err(|e| DataSourceError::WebSocket(e.to_string()))?;

    let symbols: Vec<&str> = assets.iter().map(|a| a.chainlink_symbol()).collect();
    info!("Subscribed to RTDS crypto_prices_chainlink for {:?}", symbols);

    // Build symbol -> asset map
    let symbol_map: HashMap<String, CryptoAsset> = assets
        .iter()
        .map(|a| (a.chainlink_symbol().to_string(), *a))
        .collect();

    let mut ping_timer = interval(Duration::from_secs(5));
    ping_timer.tick().await; // skip immediate first tick

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(event) = parse_rtds_chainlink_message(&text, &symbol_map)
                            && tx.send(event).await.is_err() {
                            return Err(DataSourceError::StreamEnded);
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        write.send(Message::Pong(data)).await
                            .map_err(|e| DataSourceError::WebSocket(e.to_string()))?;
                    }
                    Some(Ok(Message::Close(_))) => {
                        return Err(DataSourceError::StreamEnded);
                    }
                    Some(Err(e)) => {
                        return Err(DataSourceError::WebSocket(e.to_string()));
                    }
                    None => {
                        return Err(DataSourceError::StreamEnded);
                    }
                    _ => {}
                }
            }
            _ = ping_timer.tick() => {
                write.send(Message::Text("ping".to_string())).await
                    .map_err(|e| DataSourceError::WebSocket(e.to_string()))?;
            }
            _ = shutdown.recv() => {
                info!("RTDS Chainlink session: shutdown signal received");
                return Ok(());
            }
        }
    }
}

/// Parse an RTDS Chainlink price message into a MarketEvent.
fn parse_rtds_chainlink_message(
    text: &str,
    symbol_map: &HashMap<String, CryptoAsset>,
) -> Option<MarketEvent> {
    let msg: RtdsMessage = serde_json::from_str(text).ok()?;

    if msg.topic != "crypto_prices_chainlink" {
        return None;
    }

    let payload = msg.payload?;
    let asset = symbol_map.get(&payload.symbol)?;
    let price = Decimal::from_f64_retain(payload.value)?;
    let timestamp = Utc
        .timestamp_millis_opt(payload.timestamp)
        .single()
        .unwrap_or_else(Utc::now);

    Some(MarketEvent::ChainlinkPrice(
        super::ChainlinkPriceEvent {
            asset: *asset,
            price,
            timestamp,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_default_config() {
        let config = LiveDataSourceConfig::default();
        assert_eq!(config.assets.len(), 4);
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
    }

    #[test]
    fn test_parse_binance_trade() {
        let msg = r#"{"e":"trade","E":1704067200000,"s":"BTCUSDT","t":12345,"p":"50000.50","q":"0.1","T":1704067200000,"m":false,"M":true}"#;
        let event = parse_binance_trade(msg);

        assert!(event.is_some());
        if let Some(MarketEvent::SpotPrice(e)) = event {
            assert_eq!(e.asset, CryptoAsset::Btc);
            assert_eq!(e.price, dec!(50000.50));
            assert_eq!(e.quantity, dec!(0.1));
        } else {
            panic!("Expected SpotPrice event");
        }
    }

    #[test]
    fn test_parse_binance_subscription_response() {
        let msg = r#"{"result":null,"id":1}"#;
        let event = parse_binance_trade(msg);
        assert!(event.is_none());
    }

    #[test]
    fn test_active_market() {
        let market = ActiveMarket {
            event_id: "event123".to_string(),
            yes_token_id: "yes_token".to_string(),
            no_token_id: "no_token".to_string(),
            asset: CryptoAsset::Btc,
            strike_price: dec!(100000),
            window_end: Utc::now() + chrono::Duration::minutes(15),
        };

        assert_eq!(market.asset, CryptoAsset::Btc);
        assert_eq!(market.strike_price, dec!(100000));
    }

    #[test]
    fn test_parse_rtds_chainlink_message() {
        let symbol_map: HashMap<String, CryptoAsset> = vec![
            ("btc/usd".to_string(), CryptoAsset::Btc),
            ("eth/usd".to_string(), CryptoAsset::Eth),
        ]
        .into_iter()
        .collect();

        let msg = r#"{"topic":"crypto_prices_chainlink","type":"update","payload":{"symbol":"btc/usd","value":97500.25,"timestamp":1704067200000}}"#;
        let event = parse_rtds_chainlink_message(msg, &symbol_map);
        assert!(event.is_some());
        if let Some(MarketEvent::ChainlinkPrice(e)) = event {
            assert_eq!(e.asset, CryptoAsset::Btc);
            assert_eq!(e.price, dec!(97500.25));
        } else {
            panic!("Expected ChainlinkPrice event");
        }
    }

    #[test]
    fn test_parse_rtds_unknown_topic() {
        let symbol_map: HashMap<String, CryptoAsset> = HashMap::new();
        let msg = r#"{"topic":"crypto_prices","type":"update","payload":{"symbol":"btcusdt","value":97500,"timestamp":1704067200000}}"#;
        let event = parse_rtds_chainlink_message(msg, &symbol_map);
        assert!(event.is_none());
    }
}
