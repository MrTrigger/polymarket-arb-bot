//! Polymarket CLOB WebSocket client.
//!
//! Connects to the Polymarket CLOB WebSocket for real-time orderbook data.
//! Emits events for order book updates that consumers can process.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, timeout};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{protocol::Message, Error as WsError},
};
use tracing::{debug, error, info, warn};

use crate::orderbook::{parse_timestamp, OrderBookState};
use crate::types::{BookMessage, GenericMessage, PriceChangeMessage, SubscribeMessage, SubscriptionOp};

/// Polymarket CLOB WebSocket URL.
const CLOB_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

/// Ping interval to keep connection alive (Polymarket requires every 10s).
const PING_INTERVAL: Duration = Duration::from_secs(9);

/// Errors that can occur with the CLOB client.
#[derive(Debug, Error)]
pub enum ClobError {
    #[error("WebSocket connection failed: {0}")]
    Connection(String),

    #[error("WebSocket error: {0}")]
    WebSocket(#[from] WsError),

    #[error("JSON parse error: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("Connection timeout")]
    Timeout,

    #[error("Stream ended unexpectedly")]
    StreamEnded,

    #[error("Channel send error")]
    ChannelClosed,
}

/// Configuration for the CLOB client.
#[derive(Debug, Clone)]
pub struct ClobConfig {
    /// Connection timeout.
    pub connect_timeout: Duration,
    /// Initial reconnect delay.
    pub initial_reconnect_delay: Duration,
    /// Maximum reconnect delay.
    pub max_reconnect_delay: Duration,
    /// Interval to check for new subscriptions.
    pub subscription_check_interval: Duration,
}

impl Default for ClobConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            initial_reconnect_delay: Duration::from_secs(1),
            max_reconnect_delay: Duration::from_secs(60),
            subscription_check_interval: Duration::from_secs(30),
        }
    }
}

/// Events emitted by the CLOB client.
#[derive(Debug, Clone)]
pub enum ClobEvent {
    /// Full orderbook snapshot received.
    BookSnapshot {
        token_id: String,
        event_id: String,
        book: OrderBookState,
    },
    /// Orderbook delta update.
    BookDelta {
        token_id: String,
        event_id: String,
        side: String,
        price: rust_decimal::Decimal,
        size: rust_decimal::Decimal,
        timestamp: chrono::DateTime<Utc>,
    },
    /// Connection established.
    Connected,
    /// Connection lost (will reconnect).
    Disconnected(String),
    /// Subscribed to tokens.
    Subscribed(Vec<String>),
}

/// Token subscription info.
#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub token_id: String,
    pub event_id: String,
    pub is_yes: bool,
}

/// CLOB WebSocket client.
///
/// Connects to Polymarket CLOB WebSocket, subscribes to market tokens,
/// and emits events for orderbook updates.
pub struct ClobClient {
    config: ClobConfig,
    /// Channel for emitting events.
    event_tx: mpsc::Sender<ClobEvent>,
    /// Tokens to subscribe to (can be updated dynamically).
    tokens: HashMap<String, TokenInfo>,
}

impl ClobClient {
    /// Create a new CLOB client.
    pub fn new(config: ClobConfig, event_tx: mpsc::Sender<ClobEvent>) -> Self {
        Self {
            config,
            event_tx,
            tokens: HashMap::new(),
        }
    }

    /// Add a token to subscribe to.
    pub fn add_token(&mut self, info: TokenInfo) {
        self.tokens.insert(info.token_id.clone(), info);
    }

    /// Add multiple tokens.
    pub fn add_tokens(&mut self, infos: Vec<TokenInfo>) {
        for info in infos {
            self.add_token(info);
        }
    }

    /// Remove a token subscription.
    pub fn remove_token(&mut self, token_id: &str) {
        self.tokens.remove(token_id);
    }

    /// Get currently subscribed token count.
    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Run the client with automatic reconnection.
    pub async fn run(&self, mut shutdown: broadcast::Receiver<()>) -> Result<(), ClobError> {
        let mut reconnect_delay = self.config.initial_reconnect_delay;

        loop {
            if shutdown.try_recv().is_ok() {
                info!("CLOB client: shutdown signal received");
                return Ok(());
            }

            if self.tokens.is_empty() {
                debug!("No tokens to subscribe to, waiting...");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
                    _ = shutdown.recv() => {
                        info!("CLOB client: shutdown during wait");
                        return Ok(());
                    }
                }
            }

            match self.run_connection(&mut shutdown).await {
                Ok(()) => {
                    info!("CLOB client: clean shutdown");
                    return Ok(());
                }
                Err(e) => {
                    warn!("CLOB client error: {e}, reconnecting in {reconnect_delay:?}");

                    let _ = self.event_tx.send(ClobEvent::Disconnected(e.to_string())).await;

                    tokio::select! {
                        _ = tokio::time::sleep(reconnect_delay) => {}
                        _ = shutdown.recv() => {
                            info!("CLOB client: shutdown during reconnect delay");
                            return Ok(());
                        }
                    }

                    reconnect_delay = (reconnect_delay * 2).min(self.config.max_reconnect_delay);
                }
            }
        }
    }

    /// Run a single WebSocket connection.
    async fn run_connection(
        &self,
        shutdown: &mut broadcast::Receiver<()>,
    ) -> Result<(), ClobError> {
        info!("Connecting to Polymarket CLOB WebSocket at {CLOB_WS_URL}");

        let connect_result = timeout(self.config.connect_timeout, connect_async(CLOB_WS_URL)).await;

        let (ws_stream, _response) = match connect_result {
            Ok(Ok((stream, response))) => (stream, response),
            Ok(Err(e)) => return Err(ClobError::Connection(e.to_string())),
            Err(_) => return Err(ClobError::Timeout),
        };

        info!("Connected to Polymarket CLOB WebSocket");
        let _ = self.event_tx.send(ClobEvent::Connected).await;

        let (mut write, mut read) = ws_stream.split();

        // Get token IDs to subscribe
        let token_ids: Vec<String> = self.tokens.keys().cloned().collect();

        if token_ids.is_empty() {
            warn!("No tokens to subscribe to");
            return Err(ClobError::StreamEnded);
        }

        // Subscribe to markets
        let subscribe_msg = SubscribeMessage {
            assets_ids: token_ids.clone(),
            msg_type: "market",
        };

        let msg = serde_json::to_string(&subscribe_msg)?;
        write.send(Message::Text(msg)).await?;
        info!("Subscribed to {} market tokens", token_ids.len());

        let _ = self.event_tx.send(ClobEvent::Subscribed(token_ids.clone())).await;

        // Track subscribed tokens
        let mut subscribed: HashSet<String> = token_ids.into_iter().collect();

        // Initialize orderbook states
        let mut orderbooks: HashMap<String, OrderBookState> = HashMap::new();
        for (token_id, info) in &self.tokens {
            orderbooks.insert(
                token_id.clone(),
                OrderBookState::new(token_id.clone(), info.event_id.clone()),
            );
        }

        let mut ping_timer = interval(PING_INTERVAL);
        let mut subscription_check = interval(self.config.subscription_check_interval);

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_message(&text, &mut orderbooks).await;
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
                            return Err(ClobError::StreamEnded);
                        }
                        Some(Err(e)) => {
                            error!("WebSocket error: {e}");
                            return Err(ClobError::WebSocket(e));
                        }
                        None => {
                            warn!("WebSocket stream ended");
                            return Err(ClobError::StreamEnded);
                        }
                        _ => {}
                    }
                }

                _ = ping_timer.tick() => {
                    debug!("Sending PING");
                    write.send(Message::Text("PING".to_string())).await?;
                }

                _ = subscription_check.tick() => {
                    // Check for new tokens to subscribe
                    let new_tokens: Vec<String> = self.tokens.keys()
                        .filter(|t| !subscribed.contains(*t))
                        .cloned()
                        .collect();

                    if !new_tokens.is_empty() {
                        info!("Subscribing to {} new tokens", new_tokens.len());

                        let sub_op = SubscriptionOp {
                            assets_ids: new_tokens.clone(),
                            operation: "subscribe",
                        };
                        let msg = serde_json::to_string(&sub_op)?;
                        write.send(Message::Text(msg)).await?;

                        for token_id in &new_tokens {
                            if let Some(info) = self.tokens.get(token_id) {
                                orderbooks.insert(
                                    token_id.clone(),
                                    OrderBookState::new(token_id.clone(), info.event_id.clone()),
                                );
                            }
                            subscribed.insert(token_id.clone());
                        }

                        let _ = self.event_tx.send(ClobEvent::Subscribed(new_tokens)).await;
                    }
                }

                _ = shutdown.recv() => {
                    info!("CLOB client: shutdown signal received");
                    return Ok(());
                }
            }
        }
    }

    /// Handle an incoming WebSocket message.
    async fn handle_message(&self, text: &str, orderbooks: &mut HashMap<String, OrderBookState>) {
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
                self.handle_book_message(text, orderbooks).await;
            }
            Some("price_change") => {
                self.handle_price_change(text, orderbooks).await;
            }
            Some("last_trade_price") => {
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
    ) {
        let book: BookMessage = match serde_json::from_str(text) {
            Ok(b) => b,
            Err(e) => {
                warn!("Failed to parse book message: {e}");
                return;
            }
        };

        let token_id = &book.asset_id;
        let event_id = &book.market;

        if let Some(orderbook) = orderbooks.get_mut(token_id) {
            orderbook.apply_book(&book);
            debug!(
                "Applied book for token {}: {} bids, {} asks",
                token_id,
                book.bids.len(),
                book.asks.len()
            );

            // Emit snapshot event
            let _ = self
                .event_tx
                .send(ClobEvent::BookSnapshot {
                    token_id: token_id.clone(),
                    event_id: event_id.clone(),
                    book: orderbook.clone(),
                })
                .await;
        } else {
            // Create new orderbook if we don't have one
            let mut orderbook = OrderBookState::new(token_id.clone(), event_id.clone());
            orderbook.apply_book(&book);
            orderbooks.insert(token_id.clone(), orderbook.clone());

            let _ = self
                .event_tx
                .send(ClobEvent::BookSnapshot {
                    token_id: token_id.clone(),
                    event_id: event_id.clone(),
                    book: orderbook,
                })
                .await;
        }
    }

    /// Handle a price_change (delta) message.
    async fn handle_price_change(
        &self,
        text: &str,
        orderbooks: &mut HashMap<String, OrderBookState>,
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

            // Emit delta event
            let price = change.price.parse().unwrap_or(rust_decimal::Decimal::ZERO);
            let size = change.size.parse().unwrap_or(rust_decimal::Decimal::ZERO);

            let _ = self
                .event_tx
                .send(ClobEvent::BookDelta {
                    token_id: token_id.clone(),
                    event_id: event_id.clone(),
                    side: change.side.clone(),
                    price,
                    size,
                    timestamp,
                })
                .await;
        }
    }
}

/// Builder for creating a ClobClient with tokens.
pub struct ClobClientBuilder {
    config: ClobConfig,
    tokens: Vec<TokenInfo>,
}

impl ClobClientBuilder {
    /// Create a new builder with default config.
    pub fn new() -> Self {
        Self {
            config: ClobConfig::default(),
            tokens: Vec::new(),
        }
    }

    /// Set custom config.
    pub fn config(mut self, config: ClobConfig) -> Self {
        self.config = config;
        self
    }

    /// Add a token to subscribe.
    pub fn token(mut self, token_id: String, event_id: String, is_yes: bool) -> Self {
        self.tokens.push(TokenInfo {
            token_id,
            event_id,
            is_yes,
        });
        self
    }

    /// Add multiple tokens.
    pub fn tokens(mut self, tokens: Vec<TokenInfo>) -> Self {
        self.tokens.extend(tokens);
        self
    }

    /// Build the client.
    pub fn build(self, event_tx: mpsc::Sender<ClobEvent>) -> ClobClient {
        let mut client = ClobClient::new(self.config, event_tx);
        client.add_tokens(self.tokens);
        client
    }
}

impl Default for ClobClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clob_config_default() {
        let config = ClobConfig::default();
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.initial_reconnect_delay, Duration::from_secs(1));
        assert_eq!(config.max_reconnect_delay, Duration::from_secs(60));
    }

    #[test]
    fn test_clob_client_add_tokens() {
        let (tx, _rx) = mpsc::channel(100);
        let mut client = ClobClient::new(ClobConfig::default(), tx);

        assert_eq!(client.token_count(), 0);

        client.add_token(TokenInfo {
            token_id: "token1".to_string(),
            event_id: "event1".to_string(),
            is_yes: true,
        });

        assert_eq!(client.token_count(), 1);

        client.add_tokens(vec![
            TokenInfo {
                token_id: "token2".to_string(),
                event_id: "event1".to_string(),
                is_yes: false,
            },
            TokenInfo {
                token_id: "token3".to_string(),
                event_id: "event2".to_string(),
                is_yes: true,
            },
        ]);

        assert_eq!(client.token_count(), 3);
    }

    #[test]
    fn test_clob_client_builder() {
        let (tx, _rx) = mpsc::channel(100);

        let client = ClobClientBuilder::new()
            .token("token1".to_string(), "event1".to_string(), true)
            .token("token2".to_string(), "event1".to_string(), false)
            .build(tx);

        assert_eq!(client.token_count(), 2);
    }

    #[test]
    fn test_clob_client_remove_token() {
        let (tx, _rx) = mpsc::channel(100);
        let mut client = ClobClient::new(ClobConfig::default(), tx);

        client.add_token(TokenInfo {
            token_id: "token1".to_string(),
            event_id: "event1".to_string(),
            is_yes: true,
        });

        assert_eq!(client.token_count(), 1);

        client.remove_token("token1");

        assert_eq!(client.token_count(), 0);
    }
}
