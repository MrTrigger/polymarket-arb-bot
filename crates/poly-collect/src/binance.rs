//! Binance WebSocket capture for spot prices.
//!
//! Connects to Binance trade streams and captures BTC/ETH/SOL/XRP prices
//! to ClickHouse for backtesting and live trading.

use std::time::Duration;

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use poly_common::SpotPrice;
use rust_decimal::Decimal;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::time::{interval, timeout};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{protocol::Message, Error as WsError},
};
use tracing::{debug, error, info, warn};

use crate::data_writer::DataWriter;

/// Binance WebSocket base URL.
const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws";

/// Errors that can occur during Binance capture.
#[derive(Debug, Error)]
pub enum BinanceError {
    #[error("WebSocket connection failed: {0}")]
    Connection(String),

    #[error("WebSocket error: {0}")]
    WebSocket(#[from] WsError),

    #[error("JSON parse error: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("Write error: {0}")]
    Write(String),

    #[error("Connection timeout")]
    Timeout,

    #[error("Stream ended unexpectedly")]
    StreamEnded,
}

/// Configuration for the Binance capture.
#[derive(Debug, Clone)]
pub struct BinanceConfig {
    /// Symbols to subscribe (e.g., ["btcusdt", "ethusdt"]).
    pub symbols: Vec<String>,
    /// Buffer size before flushing to ClickHouse.
    pub buffer_size: usize,
    /// Maximum time between flushes.
    pub flush_interval: Duration,
    /// Connection timeout.
    pub connect_timeout: Duration,
    /// Initial reconnect delay.
    pub initial_reconnect_delay: Duration,
    /// Maximum reconnect delay.
    pub max_reconnect_delay: Duration,
}

impl Default for BinanceConfig {
    fn default() -> Self {
        Self {
            symbols: vec![
                "btcusdt".to_string(),
                "ethusdt".to_string(),
                "solusdt".to_string(),
                "xrpusdt".to_string(),
            ],
            buffer_size: 500,
            flush_interval: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(10),
            initial_reconnect_delay: Duration::from_secs(1),
            max_reconnect_delay: Duration::from_secs(60),
        }
    }
}

/// Binance trade message from the WebSocket stream.
#[derive(Debug, Deserialize)]
struct BinanceTrade {
    /// Event type (always "trade").
    #[serde(rename = "e")]
    _event_type: String,
    /// Event time in milliseconds.
    #[serde(rename = "E")]
    _event_time: u64,
    /// Symbol (e.g., "BTCUSDT").
    #[serde(rename = "s")]
    symbol: String,
    /// Trade ID.
    #[serde(rename = "t")]
    _trade_id: u64,
    /// Price as string.
    #[serde(rename = "p")]
    price: String,
    /// Quantity as string.
    #[serde(rename = "q")]
    quantity: String,
    /// Trade time in milliseconds.
    #[serde(rename = "T")]
    trade_time: u64,
    /// Is buyer the maker?
    #[serde(rename = "m")]
    _is_buyer_maker: bool,
}

/// Subscription request for Binance WebSocket.
#[derive(Debug, serde::Serialize)]
struct SubscribeRequest {
    method: &'static str,
    params: Vec<String>,
    id: u64,
}

/// Maps Binance symbol to asset name for storage.
fn symbol_to_asset(symbol: &str) -> Option<&'static str> {
    match symbol.to_uppercase().as_str() {
        "BTCUSDT" => Some("BTC"),
        "ETHUSDT" => Some("ETH"),
        "SOLUSDT" => Some("SOL"),
        "XRPUSDT" => Some("XRP"),
        _ => None,
    }
}

/// Binance WebSocket capture client.
pub struct BinanceCapture {
    config: BinanceConfig,
    writer: Arc<DataWriter>,
}

impl BinanceCapture {
    /// Creates a new Binance capture client.
    pub fn new(config: BinanceConfig, writer: Arc<DataWriter>) -> Self {
        Self { config, writer }
    }

    /// Runs the capture loop with automatic reconnection.
    ///
    /// This function runs indefinitely until a shutdown signal is received.
    pub async fn run(&self, mut shutdown: broadcast::Receiver<()>) -> Result<(), BinanceError> {
        let mut reconnect_delay = self.config.initial_reconnect_delay;

        loop {
            // Check for shutdown before connecting
            if shutdown.try_recv().is_ok() {
                info!("Binance capture: shutdown signal received");
                return Ok(());
            }

            match self.run_connection(&mut shutdown).await {
                Ok(()) => {
                    // Clean shutdown
                    info!("Binance capture: clean shutdown");
                    return Ok(());
                }
                Err(e) => {
                    warn!("Binance capture error: {e}, reconnecting in {reconnect_delay:?}");

                    // Wait before reconnecting, but check for shutdown
                    tokio::select! {
                        _ = tokio::time::sleep(reconnect_delay) => {}
                        _ = shutdown.recv() => {
                            info!("Binance capture: shutdown during reconnect delay");
                            return Ok(());
                        }
                    }

                    // Exponential backoff for next failure
                    reconnect_delay = (reconnect_delay * 2).min(self.config.max_reconnect_delay);
                }
            }
        }
    }

    /// Runs a single WebSocket connection until error or shutdown.
    async fn run_connection(
        &self,
        shutdown: &mut broadcast::Receiver<()>,
    ) -> Result<(), BinanceError> {
        // Connect to Binance WebSocket
        info!("Connecting to Binance WebSocket at {BINANCE_WS_URL}");

        let connect_result = timeout(self.config.connect_timeout, connect_async(BINANCE_WS_URL)).await;

        let (ws_stream, _response) = match connect_result {
            Ok(Ok((stream, response))) => (stream, response),
            Ok(Err(e)) => {
                return Err(BinanceError::Connection(e.to_string()));
            }
            Err(_) => {
                return Err(BinanceError::Timeout);
            }
        };

        info!("Connected to Binance WebSocket");

        let (mut write, mut read) = ws_stream.split();

        // Subscribe to trade streams
        let streams: Vec<String> = self
            .config
            .symbols
            .iter()
            .map(|s| format!("{}@trade", s.to_lowercase()))
            .collect();

        let subscribe_msg = SubscribeRequest {
            method: "SUBSCRIBE",
            params: streams.clone(),
            id: 1,
        };

        let msg = serde_json::to_string(&subscribe_msg)?;
        write.send(Message::Text(msg.into())).await?;
        info!("Subscribed to streams: {:?}", streams);

        // Buffer for batching writes
        let mut buffer: Vec<SpotPrice> = Vec::with_capacity(self.config.buffer_size);
        let mut flush_timer = interval(self.config.flush_interval);
        let mut stats = CaptureStats::default();

        loop {
            tokio::select! {
                // Handle incoming messages
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Some(price) = self.parse_trade(&text) {
                                buffer.push(price);
                                stats.trades_received += 1;

                                // Flush if buffer is full
                                if buffer.len() >= self.config.buffer_size {
                                    self.flush_buffer(&mut buffer, &mut stats).await?;
                                }
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
                            // Flush remaining before returning
                            if !buffer.is_empty() {
                                self.flush_buffer(&mut buffer, &mut stats).await?;
                            }
                            return Err(BinanceError::StreamEnded);
                        }
                        Some(Err(e)) => {
                            error!("WebSocket error: {e}");
                            // Flush remaining before returning
                            if !buffer.is_empty() {
                                let _ = self.flush_buffer(&mut buffer, &mut stats).await;
                            }
                            return Err(BinanceError::WebSocket(e));
                        }
                        None => {
                            warn!("WebSocket stream ended");
                            if !buffer.is_empty() {
                                self.flush_buffer(&mut buffer, &mut stats).await?;
                            }
                            return Err(BinanceError::StreamEnded);
                        }
                        _ => {
                            // Binary or other message types, ignore
                        }
                    }
                }

                // Periodic flush
                _ = flush_timer.tick() => {
                    if !buffer.is_empty() {
                        self.flush_buffer(&mut buffer, &mut stats).await?;
                    }
                    // Log stats periodically
                    info!(
                        "Binance capture stats: {} trades received, {} written, {} errors",
                        stats.trades_received, stats.trades_written, stats.write_errors
                    );
                }

                // Shutdown signal
                _ = shutdown.recv() => {
                    info!("Binance capture: shutdown signal received");
                    // Flush remaining
                    if !buffer.is_empty() {
                        let _ = self.flush_buffer(&mut buffer, &mut stats).await;
                    }
                    return Ok(());
                }
            }
        }
    }

    /// Parses a trade message from Binance WebSocket.
    fn parse_trade(&self, text: &str) -> Option<SpotPrice> {
        // Ignore subscription confirmations and other non-trade messages
        if text.contains("\"result\"") || text.contains("\"id\"") {
            debug!("Ignoring non-trade message");
            return None;
        }

        let trade: BinanceTrade = match serde_json::from_str(text) {
            Ok(t) => t,
            Err(e) => {
                debug!("Failed to parse trade message: {e}");
                return None;
            }
        };

        let asset = symbol_to_asset(&trade.symbol)?;

        let price: Decimal = match trade.price.parse() {
            Ok(p) => p,
            Err(e) => {
                warn!("Failed to parse price '{}': {e}", trade.price);
                return None;
            }
        };

        let quantity: Decimal = match trade.quantity.parse() {
            Ok(q) => q,
            Err(e) => {
                warn!("Failed to parse quantity '{}': {e}", trade.quantity);
                return None;
            }
        };

        let timestamp = Utc.timestamp_millis_opt(trade.trade_time as i64).single()?;

        Some(SpotPrice {
            asset: asset.to_string(),
            price,
            timestamp,
            quantity,
        })
    }

    /// Flushes the buffer to storage.
    async fn flush_buffer(
        &self,
        buffer: &mut Vec<SpotPrice>,
        stats: &mut CaptureStats,
    ) -> Result<(), BinanceError> {
        let count = buffer.len();
        debug!("Flushing {} trades to storage", count);

        match self.writer.write_spot_prices(buffer).await {
            Ok(()) => {
                stats.trades_written += count as u64;
                buffer.clear();
                Ok(())
            }
            Err(e) => {
                stats.write_errors += 1;
                error!("Failed to write: {e}");
                // Keep buffer, will retry
                Err(BinanceError::Write(e.to_string()))
            }
        }
    }
}

/// Statistics for the capture session.
#[derive(Debug, Default)]
struct CaptureStats {
    trades_received: u64,
    trades_written: u64,
    write_errors: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputMode;

    fn create_test_writer() -> Arc<DataWriter> {
        let temp_dir = std::env::temp_dir().join("binance_test");
        let mode = OutputMode::Csv {
            output_dir: temp_dir,
        };
        Arc::new(DataWriter::new(&mode, None).unwrap())
    }

    #[test]
    fn test_symbol_to_asset() {
        assert_eq!(symbol_to_asset("BTCUSDT"), Some("BTC"));
        assert_eq!(symbol_to_asset("ETHUSDT"), Some("ETH"));
        assert_eq!(symbol_to_asset("SOLUSDT"), Some("SOL"));
        assert_eq!(symbol_to_asset("XRPUSDT"), Some("XRP"));
        assert_eq!(symbol_to_asset("btcusdt"), Some("BTC"));
        assert_eq!(symbol_to_asset("UNKNOWN"), None);
    }

    #[test]
    fn test_parse_trade() {
        let config = BinanceConfig::default();
        let writer = create_test_writer();
        let capture = BinanceCapture::new(config, writer);

        // Valid trade message
        let msg = r#"{
            "e": "trade",
            "E": 1704067200000,
            "s": "BTCUSDT",
            "t": 123456789,
            "p": "42000.50",
            "q": "0.001",
            "T": 1704067200000,
            "m": true
        }"#;

        let result = capture.parse_trade(msg);
        assert!(result.is_some());
        let price = result.unwrap();
        assert_eq!(price.asset, "BTC");
        assert_eq!(price.price.to_string(), "42000.50");
        assert_eq!(price.quantity.to_string(), "0.001");
    }

    #[test]
    fn test_parse_subscription_response() {
        let config = BinanceConfig::default();
        let writer = create_test_writer();
        let capture = BinanceCapture::new(config, writer);

        // Subscription confirmation should be ignored
        let msg = r#"{"result":null,"id":1}"#;
        let result = capture.parse_trade(msg);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_invalid_message() {
        let config = BinanceConfig::default();
        let writer = create_test_writer();
        let capture = BinanceCapture::new(config, writer);

        // Invalid JSON
        let msg = "not json";
        let result = capture.parse_trade(msg);
        assert!(result.is_none());

        // Unknown symbol
        let msg = r#"{
            "e": "trade",
            "E": 1704067200000,
            "s": "UNKNOWN",
            "t": 123456789,
            "p": "100.00",
            "q": "1.0",
            "T": 1704067200000,
            "m": true
        }"#;
        let result = capture.parse_trade(msg);
        assert!(result.is_none());
    }

    #[test]
    fn test_default_config() {
        let config = BinanceConfig::default();
        assert_eq!(config.symbols.len(), 4);
        assert!(config.symbols.contains(&"btcusdt".to_string()));
        assert!(config.symbols.contains(&"ethusdt".to_string()));
        assert!(config.symbols.contains(&"solusdt".to_string()));
        assert!(config.symbols.contains(&"xrpusdt".to_string()));
        assert_eq!(config.buffer_size, 500);
    }
}
