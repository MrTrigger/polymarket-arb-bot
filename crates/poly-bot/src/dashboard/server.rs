//! WebSocket server for the React dashboard.
//!
//! This module provides a tokio-tungstenite WebSocket server that broadcasts
//! `DashboardState` snapshots to connected clients at a configurable interval.
//!
//! ## Features
//!
//! - Multiple client support with automatic cleanup on disconnect
//! - Full snapshot on client connect
//! - Periodic broadcasts at configured interval
//! - Graceful shutdown handling
//! - Connection statistics tracking

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::time::interval;
use tokio_tungstenite::{accept_async, tungstenite::protocol::Message};
use tracing::{debug, error, info, warn};

use crate::state::GlobalState;

use super::state::SharedDashboardStateManager;

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for the WebSocket server.
#[derive(Debug, Clone)]
pub struct WebSocketServerConfig {
    /// Port to listen on.
    pub port: u16,

    /// Broadcast interval in milliseconds.
    pub broadcast_interval_ms: u64,

    /// Maximum number of concurrent clients.
    pub max_clients: usize,
}

impl Default for WebSocketServerConfig {
    fn default() -> Self {
        Self {
            port: 3001,
            broadcast_interval_ms: 500,
            max_clients: 100,
        }
    }
}

impl WebSocketServerConfig {
    /// Create config from DashboardConfig.
    pub fn from_dashboard_config(config: &crate::config::DashboardConfig) -> Self {
        Self {
            port: config.websocket_port,
            broadcast_interval_ms: config.broadcast_interval_ms,
            max_clients: 100,
        }
    }
}

// ============================================================================
// Statistics
// ============================================================================

/// Statistics for the WebSocket server.
#[derive(Debug, Default)]
pub struct WebSocketServerStats {
    /// Total connections accepted.
    pub connections_accepted: AtomicU64,

    /// Current active connections.
    pub active_connections: AtomicU64,

    /// Total messages broadcast.
    pub messages_broadcast: AtomicU64,

    /// Total broadcast errors.
    pub broadcast_errors: AtomicU64,
}

impl WebSocketServerStats {
    /// Create new stats.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a snapshot of current stats.
    pub fn snapshot(&self) -> WebSocketServerStatsSnapshot {
        WebSocketServerStatsSnapshot {
            connections_accepted: self.connections_accepted.load(Ordering::Relaxed),
            active_connections: self.active_connections.load(Ordering::Relaxed),
            messages_broadcast: self.messages_broadcast.load(Ordering::Relaxed),
            broadcast_errors: self.broadcast_errors.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time snapshot of server stats.
#[derive(Debug, Clone)]
pub struct WebSocketServerStatsSnapshot {
    /// Total connections accepted.
    pub connections_accepted: u64,

    /// Current active connections.
    pub active_connections: u64,

    /// Total messages broadcast.
    pub messages_broadcast: u64,

    /// Total broadcast errors.
    pub broadcast_errors: u64,
}

// ============================================================================
// Client Handle
// ============================================================================

/// Unique client ID.
type ClientId = u64;

/// Handle for a connected client.
struct ClientHandle {
    /// Channel to send messages to this client.
    tx: mpsc::UnboundedSender<Message>,
}

// ============================================================================
// WebSocket Server
// ============================================================================

/// WebSocket server for broadcasting dashboard state.
pub struct WebSocketServer {
    config: WebSocketServerConfig,
    state_manager: SharedDashboardStateManager,
    global_state: Arc<GlobalState>,
    stats: Arc<WebSocketServerStats>,
    clients: Arc<RwLock<HashMap<ClientId, ClientHandle>>>,
    next_client_id: AtomicU64,
    shutdown_tx: broadcast::Sender<()>,
}

impl WebSocketServer {
    /// Create a new WebSocket server.
    pub fn new(
        config: WebSocketServerConfig,
        state_manager: SharedDashboardStateManager,
        global_state: Arc<GlobalState>,
    ) -> Self {
        let (shutdown_tx, _) = broadcast::channel(1);

        Self {
            config,
            state_manager,
            global_state,
            stats: Arc::new(WebSocketServerStats::new()),
            clients: Arc::new(RwLock::new(HashMap::new())),
            next_client_id: AtomicU64::new(1),
            shutdown_tx,
        }
    }

    /// Get the server statistics.
    pub fn stats(&self) -> &Arc<WebSocketServerStats> {
        &self.stats
    }

    /// Get the shutdown sender for triggering graceful shutdown.
    pub fn shutdown_handle(&self) -> broadcast::Sender<()> {
        self.shutdown_tx.clone()
    }

    /// Run the WebSocket server.
    ///
    /// This method blocks until shutdown is triggered.
    pub async fn run(&self) -> anyhow::Result<()> {
        let addr = format!("0.0.0.0:{}", self.config.port);
        let listener = TcpListener::bind(&addr).await?;

        info!(
            port = self.config.port,
            broadcast_interval_ms = self.config.broadcast_interval_ms,
            "Dashboard WebSocket server started"
        );

        // Start the broadcast task
        info!("Starting broadcast task");
        let broadcast_handle = self.spawn_broadcast_task();

        // Accept connections until shutdown
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, addr)) => {
                            self.handle_new_connection(stream, addr).await;
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to accept connection");
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!("WebSocket server shutting down");
                    break;
                }
            }
        }

        // Clean up
        broadcast_handle.abort();

        // Close all client connections
        let clients = self.clients.read().await;
        for (_, client) in clients.iter() {
            let _ = client.tx.send(Message::Close(None));
        }

        info!("WebSocket server stopped");
        Ok(())
    }

    /// Handle a new incoming connection.
    async fn handle_new_connection(&self, stream: TcpStream, addr: SocketAddr) {
        // Check client limit
        let current_clients = self.stats.active_connections.load(Ordering::Relaxed);
        if current_clients >= self.config.max_clients as u64 {
            warn!(
                addr = %addr,
                current = current_clients,
                max = self.config.max_clients,
                "Rejecting connection: max clients reached"
            );
            return;
        }

        // Assign client ID
        let client_id = self.next_client_id.fetch_add(1, Ordering::Relaxed);

        // Accept WebSocket handshake
        let ws_stream = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(e) => {
                warn!(addr = %addr, error = %e, "WebSocket handshake failed");
                return;
            }
        };

        self.stats.connections_accepted.fetch_add(1, Ordering::Relaxed);
        self.stats.active_connections.fetch_add(1, Ordering::Relaxed);

        info!(client_id, addr = %addr, "Client connected");

        // Split the WebSocket
        let (ws_tx, ws_rx) = ws_stream.split();

        // Create message channel for this client
        let (tx, rx) = mpsc::unbounded_channel::<Message>();

        // Register client
        {
            let mut clients = self.clients.write().await;
            clients.insert(client_id, ClientHandle { tx: tx.clone() });
        }

        // Send initial snapshot
        let snapshot = self.state_manager.snapshot(&self.global_state);
        if let Ok(json) = serde_json::to_string(&snapshot) {
            let _ = tx.send(Message::Text(json));
        }

        // Spawn tasks for this client
        let stats = Arc::clone(&self.stats);
        let clients = Arc::clone(&self.clients);
        let shutdown_tx = self.shutdown_tx.clone();

        tokio::spawn(async move {
            Self::client_task(client_id, ws_tx, ws_rx, rx, stats, clients, shutdown_tx).await;
        });
    }

    /// Task that handles a single client's communication.
    async fn client_task(
        client_id: ClientId,
        mut ws_tx: futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<TcpStream>,
            Message,
        >,
        mut ws_rx: futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<TcpStream>,
        >,
        mut rx: mpsc::UnboundedReceiver<Message>,
        stats: Arc<WebSocketServerStats>,
        clients: Arc<RwLock<HashMap<ClientId, ClientHandle>>>,
        shutdown_tx: broadcast::Sender<()>,
    ) {
        let mut shutdown_rx = shutdown_tx.subscribe();

        loop {
            tokio::select! {
                // Outgoing messages
                Some(msg) = rx.recv() => {
                    if let Err(e) = ws_tx.send(msg).await {
                        debug!(client_id, error = %e, "Failed to send message");
                        break;
                    }
                }
                // Incoming messages (pings, pongs, close)
                msg_result = ws_rx.next() => {
                    match msg_result {
                        Some(Ok(Message::Ping(data))) => {
                            if let Err(e) = ws_tx.send(Message::Pong(data)).await {
                                debug!(client_id, error = %e, "Failed to send pong");
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            debug!(client_id, "Client requested close");
                            break;
                        }
                        Some(Err(e)) => {
                            debug!(client_id, error = %e, "WebSocket error");
                            break;
                        }
                        None => {
                            debug!(client_id, "Connection closed");
                            break;
                        }
                        _ => {
                            // Ignore other messages (Text, Binary)
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    debug!(client_id, "Shutdown signal received");
                    break;
                }
            }
        }

        // Clean up
        {
            let mut clients_guard = clients.write().await;
            clients_guard.remove(&client_id);
        }

        stats.active_connections.fetch_sub(1, Ordering::Relaxed);
        info!(client_id, "Client disconnected");
    }

    /// Spawn the broadcast task.
    fn spawn_broadcast_task(&self) -> tokio::task::JoinHandle<()> {
        let state_manager = Arc::clone(&self.state_manager);
        let global_state = Arc::clone(&self.global_state);
        let clients = Arc::clone(&self.clients);
        let stats = Arc::clone(&self.stats);
        let broadcast_interval = Duration::from_millis(self.config.broadcast_interval_ms);
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut ticker = interval(broadcast_interval);

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        // Create snapshot
                        let snapshot = state_manager.snapshot(&global_state);

                        // Serialize to JSON
                        let json = match serde_json::to_string(&snapshot) {
                            Ok(j) => j,
                            Err(e) => {
                                error!(error = %e, "Failed to serialize dashboard state");
                                continue;
                            }
                        };

                        let msg = Message::Text(json);

                        // Broadcast to all clients
                        let clients_guard = clients.read().await;
                        let client_count = clients_guard.len();

                        if client_count == 0 {
                            continue;
                        }

                        let mut errors = 0u64;
                        for (_, client) in clients_guard.iter() {
                            if client.tx.send(msg.clone()).is_err() {
                                errors += 1;
                            }
                        }

                        stats.messages_broadcast.fetch_add(client_count as u64, Ordering::Relaxed);
                        if errors > 0 {
                            stats.broadcast_errors.fetch_add(errors, Ordering::Relaxed);
                        }

                        debug!(
                            clients = client_count,
                            errors,
                            "Broadcast complete"
                        );
                    }
                    _ = shutdown_rx.recv() => {
                        debug!("Broadcast task shutting down");
                        break;
                    }
                }
            }
        })
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Shared reference to WebSocketServer.
pub type SharedWebSocketServer = Arc<WebSocketServer>;

/// Create a shared WebSocket server.
pub fn create_websocket_server(
    config: WebSocketServerConfig,
    state_manager: SharedDashboardStateManager,
    global_state: Arc<GlobalState>,
) -> SharedWebSocketServer {
    tracing::info!("WebSocket server using GlobalState at {:p}", Arc::as_ptr(&global_state));
    Arc::new(WebSocketServer::new(config, state_manager, global_state))
}

/// Spawn the WebSocket server as a background task.
///
/// Returns a handle that can be used to abort the task.
pub fn spawn_websocket_server(
    config: WebSocketServerConfig,
    state_manager: SharedDashboardStateManager,
    global_state: Arc<GlobalState>,
) -> (SharedWebSocketServer, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let server = create_websocket_server(config, state_manager, global_state);
    let server_clone = Arc::clone(&server);

    let handle = tokio::spawn(async move { server_clone.run().await });

    (server, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::state::{create_shared_dashboard_state_manager, DashboardState};

    #[test]
    fn test_websocket_server_config_default() {
        let config = WebSocketServerConfig::default();
        assert_eq!(config.port, 3001);
        assert_eq!(config.broadcast_interval_ms, 500);
        assert_eq!(config.max_clients, 100);
    }

    #[test]
    fn test_websocket_server_config_from_dashboard_config() {
        let dashboard_config = crate::config::DashboardConfig {
            websocket_port: 8080,
            broadcast_interval_ms: 250,
            ..Default::default()
        };

        let config = WebSocketServerConfig::from_dashboard_config(&dashboard_config);
        assert_eq!(config.port, 8080);
        assert_eq!(config.broadcast_interval_ms, 250);
    }

    #[test]
    fn test_websocket_server_stats() {
        let stats = WebSocketServerStats::new();

        stats.connections_accepted.store(10, Ordering::Relaxed);
        stats.active_connections.store(5, Ordering::Relaxed);
        stats.messages_broadcast.store(100, Ordering::Relaxed);
        stats.broadcast_errors.store(2, Ordering::Relaxed);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.connections_accepted, 10);
        assert_eq!(snapshot.active_connections, 5);
        assert_eq!(snapshot.messages_broadcast, 100);
        assert_eq!(snapshot.broadcast_errors, 2);
    }

    #[test]
    fn test_websocket_server_creation() {
        let config = WebSocketServerConfig::default();
        let state_manager = create_shared_dashboard_state_manager();
        let global_state = Arc::new(GlobalState::new());

        let server = WebSocketServer::new(config, state_manager, global_state);

        let stats = server.stats().snapshot();
        assert_eq!(stats.connections_accepted, 0);
        assert_eq!(stats.active_connections, 0);
    }

    #[tokio::test]
    async fn test_websocket_server_shutdown() {
        let config = WebSocketServerConfig {
            port: 0, // Use random available port
            ..Default::default()
        };
        let state_manager = create_shared_dashboard_state_manager();
        let global_state = Arc::new(GlobalState::new());

        let server = create_websocket_server(config, state_manager, global_state);
        let shutdown_handle = server.shutdown_handle();

        // Send shutdown immediately
        let _ = shutdown_handle.send(());

        // The server should handle shutdown gracefully
        // (In a real test, we'd start the server and then shutdown)
    }

    #[test]
    fn test_shared_websocket_server() {
        let config = WebSocketServerConfig::default();
        let state_manager = create_shared_dashboard_state_manager();
        let global_state = Arc::new(GlobalState::new());

        let server = create_websocket_server(config, state_manager, global_state);

        // Should be able to clone the Arc
        let _server2 = Arc::clone(&server);
    }

    #[tokio::test]
    async fn test_dashboard_state_serialization_for_broadcast() {
        let state_manager = create_shared_dashboard_state_manager();
        let global_state = Arc::new(GlobalState::new());

        // Create a snapshot
        let snapshot = state_manager.snapshot(&global_state);

        // Should serialize to JSON without error
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("\"metrics\""));
        assert!(json.contains("\"markets\""));
        assert!(json.contains("\"control\""));

        // Should deserialize back
        let parsed: DashboardState = serde_json::from_str(&json).unwrap();
        assert!(parsed.markets.is_empty());
    }
}
