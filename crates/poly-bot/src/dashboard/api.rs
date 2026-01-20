//! REST API endpoints for the dashboard.
//!
//! Provides HTTP endpoints for historical data queries from ClickHouse:
//! - `GET /api/sessions` - List all sessions
//! - `GET /api/sessions/:id/equity` - Equity curve for a session
//! - `GET /api/sessions/:id/trades` - Trades for a session
//! - `GET /api/sessions/:id/logs` - Logs for a session (paginated)
//! - `GET /api/sessions/:id/markets` - Market sessions for a session

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use tokio::sync::broadcast;
use chrono::{Duration, Utc};
use clickhouse::Row;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use poly_common::ClickHouseClient;

use crate::state::{BotMode, BotStatus, GlobalState};

// ============================================================================
// API Types
// ============================================================================

/// API error response.
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
    pub message: String,
}

impl ApiError {
    pub fn new(error: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal_error", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }
}

/// Paginated response wrapper.
#[derive(Debug, Serialize)]
pub struct PaginatedResponse<T> {
    pub data: Vec<T>,
    pub page: u32,
    pub page_size: u32,
    pub total: u64,
    pub has_more: bool,
}

impl<T> PaginatedResponse<T> {
    pub fn new(data: Vec<T>, page: u32, page_size: u32, total: u64) -> Self {
        let has_more = (page as u64 * page_size as u64) < total;
        Self {
            data,
            page,
            page_size,
            total,
            has_more,
        }
    }
}

/// Pagination query parameters.
#[derive(Debug, Deserialize)]
pub struct PaginationParams {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}

fn default_page() -> u32 {
    1
}

fn default_page_size() -> u32 {
    50
}

impl PaginationParams {
    pub fn offset(&self) -> u64 {
        ((self.page.saturating_sub(1)) as u64) * (self.page_size as u64)
    }

    pub fn limit(&self) -> u64 {
        self.page_size.min(1000) as u64
    }
}

/// Log filter query parameters.
#[derive(Debug, Deserialize)]
pub struct LogFilterParams {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// Filter by log level (e.g., "ERROR", "WARN", "INFO").
    pub level: Option<String>,
    /// Search in message text.
    pub search: Option<String>,
}

impl LogFilterParams {
    pub fn offset(&self) -> u64 {
        ((self.page.saturating_sub(1)) as u64) * (self.page_size as u64)
    }

    pub fn limit(&self) -> u64 {
        self.page_size.min(1000) as u64
    }
}

// ============================================================================
// ClickHouse Row Types (for reading)
// ============================================================================

/// Session row from ClickHouse.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct SessionApiRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: Uuid,
    pub mode: String,
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub start_time: time::OffsetDateTime,
    #[serde(with = "clickhouse::serde::time::datetime64::millis::option")]
    pub end_time: Option<time::OffsetDateTime>,
    pub config_hash: String,
    pub total_pnl: f64,
    pub total_volume: f64,
    pub trades_executed: u32,
    pub trades_failed: u32,
    pub trades_skipped: u32,
    pub opportunities_detected: u64,
    pub events_processed: u64,
    pub markets_traded: Vec<String>,
    pub exit_reason: String,
}

/// Equity point for the equity curve.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct EquityPointRow {
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub timestamp: time::OffsetDateTime,
    pub total_pnl: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub total_exposure: f64,
    pub trade_count: u32,
    pub max_drawdown: f64,
    pub current_drawdown: f64,
}

/// Trade row from ClickHouse.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct TradeApiRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub trade_id: Uuid,
    pub event_id: String,
    pub token_id: String,
    pub outcome: String,
    pub side: String,
    pub order_type: String,
    pub requested_price: f64,
    pub requested_size: f64,
    pub fill_price: f64,
    pub fill_size: f64,
    pub slippage_bps: i32,
    pub fees: f64,
    pub total_cost: f64,
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub fill_time: time::OffsetDateTime,
    pub latency_ms: u32,
    pub spot_price_at_fill: f64,
    pub arb_margin_at_fill: f64,
    pub status: String,
}

/// Log row from ClickHouse.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct LogApiRow {
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub timestamp: time::OffsetDateTime,
    pub level: String,
    pub target: String,
    pub message: String,
    pub event_id: Option<String>,
    pub token_id: Option<String>,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub trade_id: Option<Uuid>,
    pub fields: String,
}

/// Market session row from ClickHouse.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct MarketSessionApiRow {
    pub event_id: String,
    pub asset: String,
    pub strike_price: f64,
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub window_start: time::OffsetDateTime,
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub window_end: time::OffsetDateTime,
    #[serde(with = "clickhouse::serde::time::datetime64::millis::option")]
    pub first_trade_time: Option<time::OffsetDateTime>,
    pub yes_shares: f64,
    pub no_shares: f64,
    pub yes_cost_basis: f64,
    pub no_cost_basis: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub trades_count: u32,
    pub opportunities_count: u32,
    pub skipped_count: u32,
    pub volume: f64,
    pub fees: f64,
    pub settlement_outcome: Option<String>,
    pub settlement_pnl: Option<f64>,
}

// ============================================================================
// API State
// ============================================================================

/// Shared state for API handlers.
#[derive(Clone)]
pub struct ApiState {
    pub clickhouse: ClickHouseClient,
    /// Global trading state for control endpoints.
    pub global_state: Option<Arc<GlobalState>>,
    /// Shutdown signal sender.
    pub shutdown_tx: Option<broadcast::Sender<()>>,
}

impl ApiState {
    pub fn new(clickhouse: ClickHouseClient) -> Self {
        Self {
            clickhouse,
            global_state: None,
            shutdown_tx: None,
        }
    }

    /// Create with global state for control endpoints.
    pub fn with_global_state(
        clickhouse: ClickHouseClient,
        global_state: Arc<GlobalState>,
        shutdown_tx: broadcast::Sender<()>,
    ) -> Self {
        Self {
            clickhouse,
            global_state: Some(global_state),
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

// ============================================================================
// API Handlers
// ============================================================================

/// GET /api/sessions - List all sessions.
async fn list_sessions(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<Vec<SessionApiRow>>, (StatusCode, Json<ApiError>)> {
    let query = r#"
        SELECT
            session_id,
            mode,
            start_time,
            end_time,
            config_hash,
            total_pnl,
            total_volume,
            trades_executed,
            trades_failed,
            trades_skipped,
            opportunities_detected,
            events_processed,
            markets_traded,
            exit_reason
        FROM sessions
        ORDER BY start_time DESC
        LIMIT 100
    "#;

    match state.clickhouse.inner().query(query).fetch_all::<SessionApiRow>().await {
        Ok(sessions) => Ok(Json(sessions)),
        Err(e) => {
            error!(error = %e, "Failed to fetch sessions");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::internal(format!("Database error: {}", e))),
            ))
        }
    }
}

/// GET /api/sessions/:id/equity - Equity curve for a session.
async fn get_session_equity(
    State(state): State<Arc<ApiState>>,
    Path(session_id): Path<Uuid>,
) -> Result<Json<Vec<EquityPointRow>>, (StatusCode, Json<ApiError>)> {
    let query = r#"
        SELECT
            timestamp,
            total_pnl,
            realized_pnl,
            unrealized_pnl,
            total_exposure,
            trade_count,
            max_drawdown,
            current_drawdown
        FROM pnl_snapshots
        WHERE session_id = ?
        ORDER BY timestamp ASC
    "#;

    match state
        .clickhouse
        .inner()
        .query(query)
        .bind(session_id)
        .fetch_all::<EquityPointRow>()
        .await
    {
        Ok(points) => Ok(Json(points)),
        Err(e) => {
            error!(error = %e, session_id = %session_id, "Failed to fetch equity curve");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::internal(format!("Database error: {}", e))),
            ))
        }
    }
}

/// GET /api/sessions/:id/trades - Trades for a session.
async fn get_session_trades(
    State(state): State<Arc<ApiState>>,
    Path(session_id): Path<Uuid>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<PaginatedResponse<TradeApiRow>>, (StatusCode, Json<ApiError>)> {
    // Get total count
    let count_query = "SELECT count() FROM bot_trades WHERE session_id = ?";
    let total: u64 = match state
        .clickhouse
        .inner()
        .query(count_query)
        .bind(session_id)
        .fetch_one::<u64>()
        .await
    {
        Ok(count) => count,
        Err(e) => {
            error!(error = %e, session_id = %session_id, "Failed to count trades");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::internal(format!("Database error: {}", e))),
            ));
        }
    };

    // Get trades with pagination
    let query = r#"
        SELECT
            trade_id,
            event_id,
            token_id,
            outcome,
            side,
            order_type,
            requested_price,
            requested_size,
            fill_price,
            fill_size,
            slippage_bps,
            fees,
            total_cost,
            fill_time,
            latency_ms,
            spot_price_at_fill,
            arb_margin_at_fill,
            status
        FROM bot_trades
        WHERE session_id = ?
        ORDER BY fill_time DESC
        LIMIT ? OFFSET ?
    "#;

    match state
        .clickhouse
        .inner()
        .query(query)
        .bind(session_id)
        .bind(params.limit())
        .bind(params.offset())
        .fetch_all::<TradeApiRow>()
        .await
    {
        Ok(trades) => Ok(Json(PaginatedResponse::new(
            trades,
            params.page,
            params.page_size,
            total,
        ))),
        Err(e) => {
            error!(error = %e, session_id = %session_id, "Failed to fetch trades");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::internal(format!("Database error: {}", e))),
            ))
        }
    }
}

/// GET /api/sessions/:id/logs - Logs for a session (paginated).
async fn get_session_logs(
    State(state): State<Arc<ApiState>>,
    Path(session_id): Path<Uuid>,
    Query(params): Query<LogFilterParams>,
) -> Result<Json<PaginatedResponse<LogApiRow>>, (StatusCode, Json<ApiError>)> {
    // Build WHERE clause based on filters
    let mut where_parts = vec!["session_id = ?".to_string()];
    let mut bindings: Vec<String> = vec![session_id.to_string()];

    if let Some(ref level) = params.level {
        where_parts.push("level = ?".to_string());
        bindings.push(level.to_uppercase());
    }

    if let Some(ref search) = params.search {
        where_parts.push("message ILIKE ?".to_string());
        bindings.push(format!("%{}%", search));
    }

    let where_clause = where_parts.join(" AND ");

    // Get total count with filters
    let count_query = format!("SELECT count() FROM structured_logs WHERE {}", where_clause);

    // Build and execute count query dynamically
    let total: u64 = {
        let mut q = state.clickhouse.inner().query(&count_query);
        q = q.bind(session_id);
        if let Some(ref level) = params.level {
            q = q.bind(level.to_uppercase());
        }
        if let Some(ref search) = params.search {
            q = q.bind(format!("%{}%", search));
        }
        match q.fetch_one::<u64>().await {
            Ok(count) => count,
            Err(e) => {
                error!(error = %e, session_id = %session_id, "Failed to count logs");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError::internal(format!("Database error: {}", e))),
                ));
            }
        }
    };

    // Get logs with pagination
    let data_query = format!(
        r#"
        SELECT
            timestamp,
            level,
            target,
            message,
            event_id,
            token_id,
            trade_id,
            fields
        FROM structured_logs
        WHERE {}
        ORDER BY timestamp DESC
        LIMIT ? OFFSET ?
        "#,
        where_clause
    );

    let logs: Vec<LogApiRow> = {
        let mut q = state.clickhouse.inner().query(&data_query);
        q = q.bind(session_id);
        if let Some(ref level) = params.level {
            q = q.bind(level.to_uppercase());
        }
        if let Some(ref search) = params.search {
            q = q.bind(format!("%{}%", search));
        }
        q = q.bind(params.limit());
        q = q.bind(params.offset());

        match q.fetch_all::<LogApiRow>().await {
            Ok(rows) => rows,
            Err(e) => {
                error!(error = %e, session_id = %session_id, "Failed to fetch logs");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError::internal(format!("Database error: {}", e))),
                ));
            }
        }
    };

    Ok(Json(PaginatedResponse::new(
        logs,
        params.page,
        params.page_size,
        total,
    )))
}

/// GET /api/sessions/:id/markets - Market sessions for a session.
async fn get_session_markets(
    State(state): State<Arc<ApiState>>,
    Path(session_id): Path<Uuid>,
) -> Result<Json<Vec<MarketSessionApiRow>>, (StatusCode, Json<ApiError>)> {
    let query = r#"
        SELECT
            event_id,
            asset,
            strike_price,
            window_start,
            window_end,
            first_trade_time,
            yes_shares,
            no_shares,
            yes_cost_basis,
            no_cost_basis,
            realized_pnl,
            unrealized_pnl,
            trades_count,
            opportunities_count,
            skipped_count,
            volume,
            fees,
            settlement_outcome,
            settlement_pnl
        FROM market_sessions
        WHERE session_id = ?
        ORDER BY window_start DESC
    "#;

    match state
        .clickhouse
        .inner()
        .query(query)
        .bind(session_id)
        .fetch_all::<MarketSessionApiRow>()
        .await
    {
        Ok(markets) => Ok(Json(markets)),
        Err(e) => {
            error!(error = %e, session_id = %session_id, "Failed to fetch market sessions");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::internal(format!("Database error: {}", e))),
            ))
        }
    }
}

/// Health check endpoint.
async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

// ============================================================================
// Spot Prices API
// ============================================================================

/// Spot price data point for charts.
#[derive(Debug, Clone, Serialize, Deserialize, Row)]
pub struct SpotPriceApiRow {
    pub price: f64,
    #[serde(with = "clickhouse::serde::time::datetime64::millis")]
    pub timestamp: time::OffsetDateTime,
}

/// Query parameters for spot prices.
#[derive(Debug, Deserialize)]
pub struct SpotPricesParams {
    /// Number of minutes of history to fetch (default: 60, max: 480 = 8 hours).
    #[serde(default = "default_minutes")]
    pub minutes: u32,
}

fn default_minutes() -> u32 {
    60
}

/// Binance API URL for klines.
const BINANCE_KLINES_URL: &str = "https://api.binance.com/api/v3/klines";

/// Convert asset symbol to Binance trading pair.
fn asset_to_binance_symbol(asset: &str) -> &'static str {
    match asset.to_uppercase().as_str() {
        "BTC" => "BTCUSDT",
        "ETH" => "ETHUSDT",
        "SOL" => "SOLUSDT",
        "XRP" => "XRPUSDT",
        _ => "BTCUSDT",
    }
}

/// Fetch historical prices from Binance as fallback.
async fn fetch_binance_prices(asset: &str, minutes: u32) -> Result<Vec<SpotPriceApiRow>, String> {
    let client = HttpClient::new();
    let symbol = asset_to_binance_symbol(asset);

    let now = Utc::now();
    let start_time = (now - Duration::minutes(minutes as i64)).timestamp_millis();
    let end_time = now.timestamp_millis();

    let url = format!(
        "{}?symbol={}&interval=1m&startTime={}&endTime={}&limit={}",
        BINANCE_KLINES_URL, symbol, start_time, end_time, minutes.min(1000)
    );

    debug!("Fetching Binance prices for {}: {}", asset, url);

    let response = client.get(&url).send().await
        .map_err(|e| format!("HTTP request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("Binance API error: HTTP {}", response.status()));
    }

    let data: Vec<Vec<serde_json::Value>> = response.json().await
        .map_err(|e| format!("JSON parse failed: {}", e))?;

    // Convert klines to SpotPriceApiRow
    // Kline format: [open_time, open, high, low, close, volume, close_time, ...]
    let prices: Vec<SpotPriceApiRow> = data
        .iter()
        .filter_map(|kline| {
            let timestamp_ms = kline.first()?.as_i64()?;
            let close_price = kline.get(4)?.as_str()?.parse::<f64>().ok()?;

            // Convert timestamp to OffsetDateTime
            let timestamp = time::OffsetDateTime::from_unix_timestamp(timestamp_ms / 1000).ok()?;

            Some(SpotPriceApiRow {
                price: close_price,
                timestamp,
            })
        })
        .collect();

    info!("Fetched {} prices from Binance for {}", prices.len(), asset);
    Ok(prices)
}

/// GET /api/prices/:asset - Historical spot prices for an asset.
/// First tries ClickHouse, then falls back to Binance if no data.
async fn get_spot_prices(
    State(state): State<Arc<ApiState>>,
    Path(asset): Path<String>,
    Query(params): Query<SpotPricesParams>,
) -> Result<Json<Vec<SpotPriceApiRow>>, (StatusCode, Json<ApiError>)> {
    // Validate asset
    let asset_upper = asset.to_uppercase();
    if !["BTC", "ETH", "SOL", "XRP"].contains(&asset_upper.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::bad_request(format!("Unknown asset: {}", asset))),
        ));
    }

    // Limit minutes to reasonable range (max 8 hours = 480 minutes)
    let minutes = params.minutes.clamp(1, 480);

    // Try ClickHouse first
    let query = r#"
        SELECT
            toFloat64(price) as price,
            timestamp
        FROM spot_prices
        WHERE asset = ?
          AND timestamp >= now() - INTERVAL ? MINUTE
        ORDER BY timestamp ASC
    "#;

    let clickhouse_result = state
        .clickhouse
        .inner()
        .query(query)
        .bind(&asset_upper)
        .bind(minutes)
        .fetch_all::<SpotPriceApiRow>()
        .await;

    match clickhouse_result {
        Ok(prices) if !prices.is_empty() => {
            debug!("Serving {} prices from ClickHouse for {}", prices.len(), asset_upper);
            Ok(Json(prices))
        }
        Ok(_) => {
            // ClickHouse returned empty - fallback to Binance
            warn!("ClickHouse has no price data for {}, falling back to Binance", asset_upper);
            match fetch_binance_prices(&asset_upper, minutes).await {
                Ok(prices) => Ok(Json(prices)),
                Err(e) => {
                    error!(error = %e, asset = %asset, "Binance fallback also failed");
                    // Return empty array rather than error for charts
                    Ok(Json(vec![]))
                }
            }
        }
        Err(e) => {
            // ClickHouse error - try Binance
            warn!(error = %e, asset = %asset, "ClickHouse query failed, trying Binance fallback");
            match fetch_binance_prices(&asset_upper, minutes).await {
                Ok(prices) => Ok(Json(prices)),
                Err(binance_err) => {
                    error!(clickhouse_error = %e, binance_error = %binance_err, asset = %asset, "Both ClickHouse and Binance failed");
                    Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiError::internal(format!("Database error: {}", e))),
                    ))
                }
            }
        }
    }
}

// ============================================================================
// Control API Handlers
// ============================================================================

/// Response for control endpoints.
#[derive(Debug, Serialize)]
pub struct ControlResponse {
    pub success: bool,
    pub message: String,
}

impl ControlResponse {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
        }
    }
}

/// POST /api/control/pause - Pause trading.
async fn pause_trading(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<ControlResponse>, (StatusCode, Json<ApiError>)> {
    let global_state = state.global_state.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::internal("Global state not available")),
        )
    })?;

    global_state.disable_trading();
    global_state.control.set_status(BotStatus::Paused);
    info!("Trading paused via API");

    Ok(Json(ControlResponse::ok("Trading paused")))
}

/// POST /api/control/resume - Resume trading.
async fn resume_trading(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<ControlResponse>, (StatusCode, Json<ApiError>)> {
    let global_state = state.global_state.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::internal("Global state not available")),
        )
    })?;

    // Check if circuit breaker is tripped
    if global_state.control.circuit_breaker_tripped.load(std::sync::atomic::Ordering::Acquire) {
        return Ok(Json(ControlResponse::error(
            "Cannot resume: circuit breaker is tripped. Reset it first.",
        )));
    }

    global_state.enable_trading();
    global_state.control.set_status(BotStatus::Trading);
    info!("Trading resumed via API");

    Ok(Json(ControlResponse::ok("Trading resumed")))
}

/// POST /api/control/stop - Graceful shutdown.
async fn stop_bot(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<ControlResponse>, (StatusCode, Json<ApiError>)> {
    let global_state = state.global_state.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::internal("Global state not available")),
        )
    })?;

    global_state.control.request_shutdown();
    info!("Shutdown requested via API");

    // Send shutdown signal if available
    if let Some(shutdown_tx) = &state.shutdown_tx {
        let _ = shutdown_tx.send(());
    }

    Ok(Json(ControlResponse::ok("Shutdown initiated")))
}

/// POST /api/control/reset-circuit-breaker - Reset the circuit breaker.
async fn reset_circuit_breaker(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<ControlResponse>, (StatusCode, Json<ApiError>)> {
    let global_state = state.global_state.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::internal("Global state not available")),
        )
    })?;

    global_state.reset_circuit_breaker();
    global_state.control.set_status(BotStatus::Ready);
    info!("Circuit breaker reset via API");

    Ok(Json(ControlResponse::ok("Circuit breaker reset")))
}

/// POST /api/control/cancel-all-orders - Cancel all pending orders.
async fn cancel_all_orders(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<ControlResponse>, (StatusCode, Json<ApiError>)> {
    let global_state = state.global_state.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::internal("Global state not available")),
        )
    })?;

    // Clear pending orders count
    let pending = global_state.control.get_pending_orders();
    global_state.control.clear_pending_orders();
    info!(pending_orders = pending, "Cancelled all pending orders via API");

    Ok(Json(ControlResponse::ok(format!(
        "Cancelled {} pending orders",
        pending
    ))))
}

/// Request body for switching mode.
#[derive(Debug, Deserialize)]
pub struct SwitchModeRequest {
    pub mode: String,
}

/// POST /api/control/switch-mode - Switch between paper and live mode.
async fn switch_mode(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<SwitchModeRequest>,
) -> Result<Json<ControlResponse>, (StatusCode, Json<ApiError>)> {
    let global_state = state.global_state.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::internal("Global state not available")),
        )
    })?;

    let new_mode = match request.mode.to_lowercase().as_str() {
        "paper" => BotMode::Paper,
        "live" => BotMode::Live,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError::bad_request(format!(
                    "Invalid mode: {}. Use 'paper' or 'live'.",
                    request.mode
                ))),
            ));
        }
    };

    let current_mode = global_state.control.get_mode();
    if current_mode == new_mode {
        return Ok(Json(ControlResponse::ok(format!(
            "Already in {} mode",
            new_mode.as_str()
        ))));
    }

    // Check for open positions before switching to paper
    let total_exposure = global_state.market_data.total_exposure();
    if total_exposure > rust_decimal::Decimal::ZERO {
        warn!(
            exposure = %total_exposure,
            "Mode switch requested with open positions"
        );
        return Ok(Json(ControlResponse::error(format!(
            "Cannot switch modes with open positions (exposure: ${}). Close positions first.",
            total_exposure
        ))));
    }

    global_state.control.set_mode(new_mode);
    info!(mode = new_mode.as_str(), "Mode switched via API");

    Ok(Json(ControlResponse::ok(format!(
        "Switched to {} mode",
        new_mode.as_str()
    ))))
}

/// GET /api/control/status - Get current control status.
async fn get_control_status(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    let global_state = state.global_state.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::internal("Global state not available")),
        )
    })?;

    let control = &global_state.control;

    Ok(Json(serde_json::json!({
        "trading_enabled": control.trading_enabled.load(std::sync::atomic::Ordering::Acquire),
        "circuit_breaker_tripped": control.circuit_breaker_tripped.load(std::sync::atomic::Ordering::Acquire),
        "consecutive_failures": control.consecutive_failures.load(std::sync::atomic::Ordering::Acquire),
        "shutdown_requested": control.shutdown_requested.load(std::sync::atomic::Ordering::Acquire),
        "bot_status": control.get_status().as_str(),
        "current_mode": control.get_mode().as_str(),
        "allowance_status": control.get_allowance_status().as_str(),
        "pending_orders_count": control.get_pending_orders(),
        "error_message": control.get_error_message(),
    })))
}

// ============================================================================
// Router Configuration
// ============================================================================

/// Configuration for the REST API server.
#[derive(Debug, Clone)]
pub struct ApiServerConfig {
    /// Port to listen on.
    pub port: u16,
    /// Enable CORS for frontend development.
    pub enable_cors: bool,
    /// Path to static files directory (frontend dist/).
    /// If set, serves the React dashboard at the root path.
    pub static_dir: Option<String>,
}

impl Default for ApiServerConfig {
    fn default() -> Self {
        Self {
            port: 3002,
            enable_cors: true,
            static_dir: None,
        }
    }
}

impl ApiServerConfig {
    /// Create config from DashboardConfig.
    pub fn from_dashboard_config(config: &crate::config::DashboardConfig) -> Self {
        Self {
            port: config.api_port,
            enable_cors: true,
            static_dir: config.static_dir.clone(),
        }
    }
}

/// Create the API router with all endpoints.
pub fn create_api_router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/api/health", get(health_check))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}/equity", get(get_session_equity))
        .route("/api/sessions/{id}/trades", get(get_session_trades))
        .route("/api/sessions/{id}/logs", get(get_session_logs))
        .route("/api/sessions/{id}/markets", get(get_session_markets))
        .route("/api/prices/{asset}", get(get_spot_prices))
        // Control endpoints
        .route("/api/control/status", get(get_control_status))
        .route("/api/control/pause", post(pause_trading))
        .route("/api/control/resume", post(resume_trading))
        .route("/api/control/stop", post(stop_bot))
        .route("/api/control/reset-circuit-breaker", post(reset_circuit_breaker))
        .route("/api/control/cancel-all-orders", post(cancel_all_orders))
        .route("/api/control/switch-mode", post(switch_mode))
        .with_state(state)
}

/// Run the API server.
pub async fn run_api_server(
    config: ApiServerConfig,
    clickhouse: ClickHouseClient,
    global_state: Option<Arc<GlobalState>>,
    shutdown_tx: Option<broadcast::Sender<()>>,
) -> anyhow::Result<()> {
    use std::path::PathBuf;
    use tower_http::services::{ServeDir, ServeFile};

    let state = Arc::new(if let (Some(gs), Some(tx)) = (global_state, shutdown_tx) {
        ApiState::with_global_state(clickhouse, gs, tx)
    } else {
        ApiState::new(clickhouse)
    });
    let api_router = create_api_router(state);

    // Build the app with optional static file serving
    let app = if let Some(ref static_dir) = config.static_dir {
        let static_path = PathBuf::from(static_dir);
        let index_path = static_path.join("index.html");

        info!(
            static_dir = %static_dir,
            "Serving static files for dashboard"
        );

        // Serve API routes first, then fallback to static files
        // The fallback serves index.html for any non-API route (SPA routing)
        Router::new()
            .merge(api_router)
            .fallback_service(
                ServeDir::new(&static_path)
                    .not_found_service(ServeFile::new(&index_path)),
            )
    } else {
        api_router
    };

    // Add CORS if enabled
    let app = if config.enable_cors {
        use tower_http::cors::{Any, CorsLayer};
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);
        app.layer(cors)
    } else {
        app
    };

    let addr = format!("0.0.0.0:{}", config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    if config.static_dir.is_some() {
        info!(
            port = config.port,
            "Dashboard server started (API + static files)"
        );
    } else {
        info!(port = config.port, "Dashboard REST API server started");
    }

    axum::serve(listener, app).await?;

    Ok(())
}

/// Spawn the API server as a background task.
pub fn spawn_api_server(
    config: ApiServerConfig,
    clickhouse: ClickHouseClient,
    global_state: Option<Arc<GlobalState>>,
    shutdown_tx: Option<broadcast::Sender<()>>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move { run_api_server(config, clickhouse, global_state, shutdown_tx).await })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_api_error_new() {
        let error = ApiError::new("test_error", "Test message");
        assert_eq!(error.error, "test_error");
        assert_eq!(error.message, "Test message");
    }

    #[test]
    fn test_api_error_internal() {
        let error = ApiError::internal("Something went wrong");
        assert_eq!(error.error, "internal_error");
        assert_eq!(error.message, "Something went wrong");
    }

    #[test]
    fn test_api_error_not_found() {
        let error = ApiError::not_found("Session not found");
        assert_eq!(error.error, "not_found");
        assert_eq!(error.message, "Session not found");
    }

    #[test]
    fn test_api_error_bad_request() {
        let error = ApiError::bad_request("Invalid parameter");
        assert_eq!(error.error, "bad_request");
        assert_eq!(error.message, "Invalid parameter");
    }

    #[test]
    fn test_pagination_params_default() {
        let params = PaginationParams {
            page: default_page(),
            page_size: default_page_size(),
        };
        assert_eq!(params.page, 1);
        assert_eq!(params.page_size, 50);
    }

    #[test]
    fn test_pagination_params_offset() {
        let params = PaginationParams {
            page: 3,
            page_size: 20,
        };
        assert_eq!(params.offset(), 40); // (3-1) * 20 = 40
    }

    #[test]
    fn test_pagination_params_offset_page_one() {
        let params = PaginationParams {
            page: 1,
            page_size: 50,
        };
        assert_eq!(params.offset(), 0);
    }

    #[test]
    fn test_pagination_params_limit_max() {
        let params = PaginationParams {
            page: 1,
            page_size: 5000, // Over max
        };
        assert_eq!(params.limit(), 1000); // Capped at 1000
    }

    #[test]
    fn test_paginated_response_has_more() {
        let response: PaginatedResponse<u32> = PaginatedResponse::new(vec![1, 2, 3], 1, 50, 100);
        assert!(response.has_more);

        let response2: PaginatedResponse<u32> = PaginatedResponse::new(vec![1, 2], 2, 50, 100);
        assert!(!response2.has_more);
    }

    #[test]
    fn test_log_filter_params_offset() {
        let params = LogFilterParams {
            page: 2,
            page_size: 100,
            level: Some("ERROR".to_string()),
            search: None,
        };
        assert_eq!(params.offset(), 100);
        assert_eq!(params.limit(), 100);
    }

    #[test]
    fn test_api_server_config_default() {
        let config = ApiServerConfig::default();
        assert_eq!(config.port, 3002);
        assert!(config.enable_cors);
    }

    #[test]
    fn test_api_server_config_from_dashboard_config() {
        let dashboard_config = crate::config::DashboardConfig {
            api_port: 8080,
            ..Default::default()
        };

        let config = ApiServerConfig::from_dashboard_config(&dashboard_config);
        assert_eq!(config.port, 8080);
        assert!(config.enable_cors);
    }

    #[test]
    fn test_api_state_creation() {
        let clickhouse = ClickHouseClient::with_defaults();
        let state = ApiState::new(clickhouse);
        // Should create without error
        let _ = state;
    }

    #[test]
    fn test_create_api_router() {
        let clickhouse = ClickHouseClient::with_defaults();
        let state = Arc::new(ApiState::new(clickhouse));
        let router = create_api_router(state);
        // Should create router without error
        let _ = router;
    }
}
