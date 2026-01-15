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
    routing::get,
    Json, Router,
};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use tracing::{error, info};
use uuid::Uuid;

use poly_common::ClickHouseClient;

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
}

impl ApiState {
    pub fn new(clickhouse: ClickHouseClient) -> Self {
        Self { clickhouse }
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
        .with_state(state)
}

/// Run the API server.
pub async fn run_api_server(
    config: ApiServerConfig,
    clickhouse: ClickHouseClient,
) -> anyhow::Result<()> {
    use std::path::PathBuf;
    use tower_http::services::{ServeDir, ServeFile};

    let state = Arc::new(ApiState::new(clickhouse));
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
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move { run_api_server(config, clickhouse).await })
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
