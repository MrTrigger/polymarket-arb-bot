//! Polymarket RTDS WebSocket capture for Chainlink oracle prices.
//!
//! Connects to `wss://ws-live-data.polymarket.com` and subscribes to
//! `crypto_prices_chainlink` topic to capture Chainlink Data Stream prices.
//! Writes prices to `chainlink_prices.csv` for backtesting.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use poly_common::CryptoAsset;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::time::interval;
use tokio_tungstenite::{
    connect_async,
    tungstenite::protocol::Message,
};
use tracing::{debug, info, warn};

use crate::data_writer::DataWriter;

/// Polymarket RTDS WebSocket URL.
const RTDS_WS_URL: &str = "wss://ws-live-data.polymarket.com";

/// Errors that can occur during RTDS capture.
#[derive(Debug, Error)]
pub enum RtdsError {
    #[error("WebSocket connection failed: {0}")]
    Connection(String),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("Write error: {0}")]
    Write(String),

    #[error("Connection timeout")]
    Timeout,

    #[error("Stream ended unexpectedly")]
    StreamEnded,
}

/// Configuration for the RTDS capture.
#[derive(Debug, Clone)]
pub struct RtdsConfig {
    /// Assets to capture Chainlink prices for.
    pub assets: Vec<CryptoAsset>,
    /// Buffer size before flushing to CSV.
    pub buffer_size: usize,
    /// Maximum time between flushes.
    pub flush_interval: Duration,
    /// Connection timeout.
    pub connect_timeout: Duration,
    /// Ping interval to keep connection alive.
    pub ping_interval: Duration,
}

impl Default for RtdsConfig {
    fn default() -> Self {
        Self {
            assets: vec![
                CryptoAsset::Btc,
                CryptoAsset::Eth,
                CryptoAsset::Sol,
                CryptoAsset::Xrp,
            ],
            buffer_size: 100,
            flush_interval: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(5),
        }
    }
}

/// RTDS message from WebSocket.
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

/// A Chainlink price record for CSV output.
#[derive(Debug, serde::Serialize)]
pub struct ChainlinkPriceRecord {
    pub timestamp_ms: i64,
    pub asset: String,
    pub price: String,
}

/// RTDS Chainlink price capture.
pub struct RtdsCapture;

impl RtdsCapture {
    /// Run the RTDS capture with reconnection logic.
    pub async fn run(
        writer: Arc<DataWriter>,
        config: RtdsConfig,
        mut shutdown: broadcast::Receiver<()>,
    ) -> Result<(), RtdsError> {
        let mut reconnect_delay = Duration::from_secs(1);
        let max_reconnect_delay = Duration::from_secs(60);

        loop {
            if shutdown.try_recv().is_ok() {
                info!("RTDS Chainlink capture: shutdown signal received");
                return Ok(());
            }

            match Self::run_session(&writer, &config, &mut shutdown).await {
                Ok(()) => {
                    info!("RTDS Chainlink capture: clean shutdown");
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
                            info!("RTDS Chainlink capture: shutdown during reconnect");
                            return Ok(());
                        }
                    }

                    reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
                }
            }
        }
    }

    /// Run a single RTDS WebSocket session.
    async fn run_session(
        writer: &Arc<DataWriter>,
        config: &RtdsConfig,
        shutdown: &mut broadcast::Receiver<()>,
    ) -> Result<(), RtdsError> {
        info!("Connecting to Polymarket RTDS at {}", RTDS_WS_URL);

        let connect_result =
            tokio::time::timeout(config.connect_timeout, connect_async(RTDS_WS_URL)).await;

        let (ws_stream, _) = match connect_result {
            Ok(Ok((stream, response))) => (stream, response),
            Ok(Err(e)) => return Err(RtdsError::Connection(e.to_string())),
            Err(_) => return Err(RtdsError::Timeout),
        };

        info!("Connected to Polymarket RTDS WebSocket");

        let (mut write, mut read) = ws_stream.split();

        // Send initial ping (required by RTDS protocol before subscribing)
        write
            .send(Message::Text("ping".into()))
            .await
            .map_err(|e| RtdsError::WebSocket(e.to_string()))?;

        // Subscribe to all Chainlink price updates (no per-symbol filter needed)
        let subscribe_msg = serde_json::json!({
            "action": "subscribe",
            "subscriptions": [{
                "topic": "crypto_prices_chainlink",
                "type": "update"
            }],
        });

        write
            .send(Message::Text(subscribe_msg.to_string().into()))
            .await
            .map_err(|e| RtdsError::WebSocket(e.to_string()))?;

        let symbols: Vec<&str> = config.assets.iter().map(|a| a.chainlink_symbol()).collect();
        info!("Subscribed to RTDS crypto_prices_chainlink for {:?}", symbols);

        // Build symbol -> asset map
        let symbol_map: HashMap<String, CryptoAsset> = config
            .assets
            .iter()
            .map(|a| (a.chainlink_symbol().to_string(), *a))
            .collect();

        let mut buffer: Vec<ChainlinkPriceRecord> = Vec::with_capacity(config.buffer_size);
        let mut total_records: u64 = 0;
        let mut flush_timer = interval(config.flush_interval);
        let mut ping_timer = interval(config.ping_interval);
        // Skip the immediate first tick of both timers
        flush_timer.tick().await;
        ping_timer.tick().await;

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Some(record) = Self::parse_message(&text, &symbol_map) {
                                buffer.push(record);
                                if buffer.len() >= config.buffer_size {
                                    Self::flush_buffer(writer, &mut buffer, &mut total_records)?;
                                }
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            write.send(Message::Pong(data)).await
                                .map_err(|e| RtdsError::WebSocket(e.to_string()))?;
                        }
                        Some(Ok(Message::Close(frame))) => {
                            warn!("RTDS: Close frame: {:?}", frame);
                            Self::flush_buffer(writer, &mut buffer, &mut total_records)?;
                            return Err(RtdsError::StreamEnded);
                        }
                        Some(Err(e)) => {
                            Self::flush_buffer(writer, &mut buffer, &mut total_records)?;
                            return Err(RtdsError::WebSocket(e.to_string()));
                        }
                        None => {
                            Self::flush_buffer(writer, &mut buffer, &mut total_records)?;
                            return Err(RtdsError::StreamEnded);
                        }
                        _ => {}
                    }
                }
                _ = flush_timer.tick() => {
                    if !buffer.is_empty() {
                        Self::flush_buffer(writer, &mut buffer, &mut total_records)?;
                    }
                }
                _ = ping_timer.tick() => {
                    write.send(Message::Text("ping".into())).await
                        .map_err(|e| RtdsError::WebSocket(e.to_string()))?;
                }
                _ = shutdown.recv() => {
                    info!("RTDS Chainlink session: shutdown ({} records written)", total_records);
                    Self::flush_buffer(writer, &mut buffer, &mut total_records)?;
                    return Ok(());
                }
            }
        }
    }

    /// Parse an RTDS message into a CSV record.
    fn parse_message(
        text: &str,
        symbol_map: &HashMap<String, CryptoAsset>,
    ) -> Option<ChainlinkPriceRecord> {
        let msg: RtdsMessage = serde_json::from_str(text).ok()?;

        if msg.topic != "crypto_prices_chainlink" {
            return None;
        }

        let payload = msg.payload?;
        let asset = symbol_map.get(&payload.symbol)?;

        Some(ChainlinkPriceRecord {
            timestamp_ms: payload.timestamp,
            asset: asset.as_str().to_string(),
            price: payload.value.to_string(),
        })
    }

    /// Flush buffered records to CSV.
    fn flush_buffer(
        writer: &Arc<DataWriter>,
        buffer: &mut Vec<ChainlinkPriceRecord>,
        total_records: &mut u64,
    ) -> Result<(), RtdsError> {
        if buffer.is_empty() {
            return Ok(());
        }

        if let Some(csv_writer) = writer.csv() {
            csv_writer
                .write_chainlink_prices(buffer)
                .map_err(|e| RtdsError::Write(e.to_string()))?;
        }

        *total_records += buffer.len() as u64;
        if total_records.is_multiple_of(1000) {
            debug!("RTDS Chainlink: wrote {} total records", total_records);
        }

        buffer.clear();
        Ok(())
    }
}
