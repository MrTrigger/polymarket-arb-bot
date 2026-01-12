//! Live data source using WebSocket connections.
//!
//! Connects to Binance and Polymarket WebSockets to receive real-time
//! market data for live trading.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::time::{interval, timeout};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{debug, error, info, warn};

use poly_common::types::{CryptoAsset, Side};

use super::{
    BookDeltaEvent, BookSnapshotEvent, DataSource, DataSourceError, MarketEvent, SpotPriceEvent,
};
use crate::types::PriceLevel;

/// Binance WebSocket URL.
const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws";

/// Polymarket CLOB WebSocket URL.
const CLOB_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

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
    /// CLOB ping interval (Polymarket requires every 10s).
    pub clob_ping_interval: Duration,
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
            clob_ping_interval: Duration::from_secs(9),
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

/// Live data source that connects to real WebSocket feeds.
pub struct LiveDataSource {
    #[allow(dead_code)]
    config: LiveDataSourceConfig,
    event_rx: mpsc::Receiver<MarketEvent>,
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

        // Spawn connection tasks
        let binance_shutdown = shutdown_tx.subscribe();
        let binance_tx = event_tx.clone();
        let binance_config = config.clone();
        tokio::spawn(async move {
            if let Err(e) = run_binance_connection(binance_config, binance_tx, binance_shutdown).await
            {
                error!("Binance connection error: {}", e);
            }
        });

        let clob_shutdown = shutdown_tx.subscribe();
        let clob_tx = event_tx.clone();
        let clob_markets = Arc::clone(&active_markets);
        let clob_config = config.clone();
        tokio::spawn(async move {
            if let Err(e) = run_clob_connection(clob_config, clob_tx, clob_markets, clob_shutdown).await
            {
                error!("CLOB connection error: {}", e);
            }
        });

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
                warn!("Binance connection error: {}, reconnecting in {:?}", e, reconnect_delay);

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

    let connect_result = timeout(config.connect_timeout, connect_async(BINANCE_WS_URL)).await;

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

/// CLOB subscription message.
#[derive(Debug, serde::Serialize)]
struct ClobSubscribeMessage {
    assets_ids: Vec<String>,
    #[serde(rename = "type")]
    msg_type: &'static str,
}

/// CLOB order level.
#[derive(Debug, Deserialize)]
struct OrderSummary {
    price: String,
    size: String,
}

/// CLOB book message.
#[derive(Debug, Deserialize)]
struct BookMessage {
    #[allow(dead_code)]
    event_type: String,
    asset_id: String,
    market: String,
    timestamp: String,
    bids: Vec<OrderSummary>,
    asks: Vec<OrderSummary>,
}

/// CLOB price change.
#[derive(Debug, Deserialize)]
struct PriceChangeEntry {
    price: String,
    size: String,
    side: String,
}

/// CLOB price change message.
#[derive(Debug, Deserialize)]
struct PriceChangeMessage {
    #[allow(dead_code)]
    event_type: String,
    asset_id: String,
    market: String,
    timestamp: String,
    price_changes: Vec<PriceChangeEntry>,
}

/// Generic CLOB message for type detection.
#[derive(Debug, Deserialize)]
struct GenericClobMessage {
    event_type: Option<String>,
}

/// Run Polymarket CLOB WebSocket connection.
async fn run_clob_connection(
    config: LiveDataSourceConfig,
    tx: mpsc::Sender<MarketEvent>,
    active_markets: ActiveMarketsState,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), DataSourceError> {
    let mut reconnect_delay = Duration::from_secs(1);
    let max_reconnect_delay = Duration::from_secs(60);

    loop {
        if shutdown.try_recv().is_ok() {
            info!("CLOB connection: shutdown signal received");
            return Ok(());
        }

        match run_clob_session(&config, &tx, &active_markets, &mut shutdown).await {
            Ok(()) => {
                info!("CLOB connection: clean shutdown");
                return Ok(());
            }
            Err(e) => {
                warn!("CLOB connection error: {}, reconnecting in {:?}", e, reconnect_delay);

                tokio::select! {
                    _ = tokio::time::sleep(reconnect_delay) => {}
                    _ = shutdown.recv() => {
                        info!("CLOB connection: shutdown during reconnect");
                        return Ok(());
                    }
                }

                reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
            }
        }
    }
}

/// Run a single CLOB WebSocket session.
async fn run_clob_session(
    config: &LiveDataSourceConfig,
    tx: &mpsc::Sender<MarketEvent>,
    active_markets: &ActiveMarketsState,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<(), DataSourceError> {
    info!("Connecting to Polymarket CLOB WebSocket at {}", CLOB_WS_URL);

    let connect_result = timeout(config.connect_timeout, connect_async(CLOB_WS_URL)).await;

    let (ws_stream, _) = match connect_result {
        Ok(Ok((stream, response))) => (stream, response),
        Ok(Err(e)) => return Err(DataSourceError::Connection(e.to_string())),
        Err(_) => return Err(DataSourceError::Timeout),
    };

    info!("Connected to Polymarket CLOB WebSocket");

    let (mut write, mut read) = ws_stream.split();

    // Get token IDs to subscribe to
    let token_ids = get_token_ids(active_markets).await;

    if !token_ids.is_empty() {
        let subscribe_msg = ClobSubscribeMessage {
            assets_ids: token_ids.clone(),
            msg_type: "market",
        };

        let msg = serde_json::to_string(&subscribe_msg)
            .map_err(|e| DataSourceError::Parse(e.to_string()))?;
        write
            .send(Message::Text(msg))
            .await
            .map_err(|e| DataSourceError::WebSocket(e.to_string()))?;

        info!("Subscribed to {} CLOB tokens", token_ids.len());
    }

    // Build token_id -> event_id mapping
    let token_event_map = build_token_event_map(active_markets).await;

    let mut ping_timer = interval(config.clob_ping_interval);
    let mut subscription_check = interval(Duration::from_secs(30));
    let mut subscribed_tokens: std::collections::HashSet<String> = token_ids.into_iter().collect();

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(event) = parse_clob_message(&text, &token_event_map)
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
                write.send(Message::Text("PING".to_string())).await
                    .map_err(|e| DataSourceError::WebSocket(e.to_string()))?;
            }
            _ = subscription_check.tick() => {
                // Check for new markets
                let current_tokens = get_token_ids(active_markets).await;
                let new_tokens: Vec<String> = current_tokens
                    .into_iter()
                    .filter(|t| !subscribed_tokens.contains(t))
                    .collect();

                if !new_tokens.is_empty() {
                    let sub_msg = serde_json::json!({
                        "assets_ids": new_tokens,
                        "operation": "subscribe"
                    });
                    write.send(Message::Text(sub_msg.to_string())).await
                        .map_err(|e| DataSourceError::WebSocket(e.to_string()))?;

                    for token in new_tokens {
                        subscribed_tokens.insert(token);
                    }
                }
            }
            _ = shutdown.recv() => {
                info!("CLOB session: shutdown signal received");
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

/// Parse a CLOB message into a MarketEvent.
fn parse_clob_message(text: &str, token_event_map: &HashMap<String, String>) -> Option<MarketEvent> {
    // Skip non-JSON messages like PONG
    let generic: GenericClobMessage = serde_json::from_str(text).ok()?;

    match generic.event_type.as_deref()? {
        "book" => parse_book_message(text, token_event_map),
        "price_change" => parse_price_change_message(text, token_event_map),
        _ => None,
    }
}

/// Parse a book (snapshot) message.
fn parse_book_message(text: &str, token_event_map: &HashMap<String, String>) -> Option<MarketEvent> {
    let book: BookMessage = serde_json::from_str(text).ok()?;

    let event_id = token_event_map
        .get(&book.asset_id)
        .cloned()
        .unwrap_or_else(|| book.market.clone());

    let bids: Vec<PriceLevel> = book
        .bids
        .iter()
        .filter_map(|o| {
            let price: Decimal = o.price.parse().ok()?;
            let size: Decimal = o.size.parse().ok()?;
            Some(PriceLevel::new(price, size))
        })
        .collect();

    let asks: Vec<PriceLevel> = book
        .asks
        .iter()
        .filter_map(|o| {
            let price: Decimal = o.price.parse().ok()?;
            let size: Decimal = o.size.parse().ok()?;
            Some(PriceLevel::new(price, size))
        })
        .collect();

    let timestamp = parse_timestamp(&book.timestamp)?;

    Some(MarketEvent::BookSnapshot(BookSnapshotEvent {
        token_id: book.asset_id,
        event_id,
        bids,
        asks,
        timestamp,
    }))
}

/// Parse a price_change (delta) message.
fn parse_price_change_message(
    text: &str,
    token_event_map: &HashMap<String, String>,
) -> Option<MarketEvent> {
    let msg: PriceChangeMessage = serde_json::from_str(text).ok()?;

    let event_id = token_event_map
        .get(&msg.asset_id)
        .cloned()
        .unwrap_or_else(|| msg.market.clone());

    // Process only the first price change for simplicity
    // In practice, we might want to emit multiple events
    let change = msg.price_changes.first()?;

    let side = match change.side.to_lowercase().as_str() {
        "buy" | "bid" => Side::Buy,
        "sell" | "ask" => Side::Sell,
        _ => return None,
    };

    let price: Decimal = change.price.parse().ok()?;
    let size: Decimal = change.size.parse().ok()?;
    let timestamp = parse_timestamp(&msg.timestamp)?;

    Some(MarketEvent::BookDelta(BookDeltaEvent {
        token_id: msg.asset_id,
        event_id,
        side,
        price,
        size,
        timestamp,
    }))
}

/// Parse a timestamp from milliseconds string.
fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    ts.parse::<i64>()
        .ok()
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
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
    fn test_parse_book_message() {
        let msg = r#"{
            "event_type": "book",
            "asset_id": "token123",
            "market": "event456",
            "timestamp": "1704067200000",
            "hash": "abc",
            "bids": [{"price": "0.45", "size": "100"}],
            "asks": [{"price": "0.55", "size": "150"}]
        }"#;

        let token_event_map = HashMap::new();
        let event = parse_clob_message(msg, &token_event_map);

        assert!(event.is_some());
        if let Some(MarketEvent::BookSnapshot(e)) = event {
            assert_eq!(e.token_id, "token123");
            assert_eq!(e.bids.len(), 1);
            assert_eq!(e.asks.len(), 1);
            assert_eq!(e.bids[0].price, dec!(0.45));
            assert_eq!(e.asks[0].price, dec!(0.55));
        } else {
            panic!("Expected BookSnapshot event");
        }
    }

    #[test]
    fn test_parse_price_change_message() {
        let msg = r#"{
            "event_type": "price_change",
            "asset_id": "token123",
            "market": "event456",
            "timestamp": "1704067200000",
            "price_changes": [{"price": "0.46", "size": "50", "side": "buy"}]
        }"#;

        let token_event_map = HashMap::new();
        let event = parse_clob_message(msg, &token_event_map);

        assert!(event.is_some());
        if let Some(MarketEvent::BookDelta(e)) = event {
            assert_eq!(e.token_id, "token123");
            assert_eq!(e.side, Side::Buy);
            assert_eq!(e.price, dec!(0.46));
            assert_eq!(e.size, dec!(50));
        } else {
            panic!("Expected BookDelta event");
        }
    }

    #[test]
    fn test_parse_timestamp() {
        let ts = parse_timestamp("1704067200000");
        assert!(ts.is_some());
        assert_eq!(ts.unwrap().timestamp_millis(), 1704067200000);
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
}
