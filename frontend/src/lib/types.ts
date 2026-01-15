/**
 * TypeScript types matching Rust dashboard structs.
 *
 * These types are designed to match the JSON serialization from the Rust backend.
 * - Decimal values are serialized as strings for precision preservation
 * - DateTime values are serialized as ISO 8601 strings
 * - UUIDs are serialized as strings
 */

// ============================================================================
// Main Dashboard State (WebSocket payload)
// ============================================================================

/**
 * Complete dashboard state received via WebSocket.
 * This is the top-level type for real-time updates.
 */
export interface DashboardSnapshot {
  /** Snapshot timestamp (ISO 8601). */
  timestamp: string;
  /** Current metrics (P&L, trades, volume, etc.). */
  metrics: MetricsSnapshot;
  /** Active markets with order book data. */
  markets: ActiveMarket[];
  /** Recent trades (last 100). */
  recent_trades: Trade[];
  /** Current positions by market. */
  positions: Position[];
  /** Control flags (trading enabled, circuit breaker). */
  control: ControlState;
  /** Recent logs (last 100). */
  recent_logs: LogEntry[];
  /** Active anomalies. */
  anomalies: Anomaly[];
}

// ============================================================================
// Metrics
// ============================================================================

/**
 * Metrics snapshot with trading statistics.
 * Decimal values are strings for precision.
 */
export interface MetricsSnapshot {
  /** Total events processed. */
  events_processed: number;
  /** Total arbitrage opportunities detected. */
  opportunities_detected: number;
  /** Total trades executed. */
  trades_executed: number;
  /** Total trades failed. */
  trades_failed: number;
  /** Total trades skipped. */
  trades_skipped: number;
  /** Total P&L in USDC (string for precision). */
  pnl_usdc: string;
  /** Total volume in USDC (string for precision). */
  volume_usdc: string;
  /** Shadow orders fired. */
  shadow_orders_fired: number;
  /** Shadow orders filled. */
  shadow_orders_filled: number;
  /** Win rate (0.0-1.0), null if no trades. */
  win_rate: number | null;
}

// ============================================================================
// Markets
// ============================================================================

/**
 * State of an active trading market.
 */
export interface ActiveMarket {
  /** Event ID. */
  event_id: string;
  /** Asset symbol (BTC, ETH, etc.). */
  asset: string;
  /** Strike price (string for precision). */
  strike_price: string;
  /** Window end time (ISO 8601). */
  window_end: string;
  /** Seconds remaining in window. */
  seconds_remaining: number;
  /** Current spot price (string for precision). */
  spot_price: string;
  /** YES token order book summary. */
  yes_book: OrderBookSummary | null;
  /** NO token order book summary. */
  no_book: OrderBookSummary | null;
  /** Arbitrage spread (1.0 - YES_ask - NO_ask). Positive = opportunity. */
  arb_spread: string;
  /** Whether there's currently an arb opportunity. */
  has_arb_opportunity: boolean;
}

/**
 * Summary of an order book for dashboard display.
 */
export interface OrderBookSummary {
  /** Token ID. */
  token_id: string;
  /** Best bid price (string for precision). */
  best_bid: string;
  /** Best bid size (string for precision). */
  best_bid_size: string;
  /** Best ask price (string for precision). */
  best_ask: string;
  /** Best ask size (string for precision). */
  best_ask_size: string;
  /** Spread in basis points. */
  spread_bps: number;
  /** Last update timestamp (ms since epoch). */
  last_update_ms: number;
}

// ============================================================================
// Positions
// ============================================================================

/**
 * State of a position in a market.
 */
export interface Position {
  /** Event ID. */
  event_id: string;
  /** YES shares held (string for precision). */
  yes_shares: string;
  /** NO shares held (string for precision). */
  no_shares: string;
  /** YES cost basis (string for precision). */
  yes_cost_basis: string;
  /** NO cost basis (string for precision). */
  no_cost_basis: string;
  /** Realized P&L (string for precision). */
  realized_pnl: string;
  /** Total exposure (string for precision). */
  total_exposure: string;
  /** Imbalance ratio 0.0-1.0 (string for precision). */
  imbalance_ratio: string;
  /** Inventory state classification. */
  inventory_state: InventoryState;
}

/** Inventory state classification. */
export type InventoryState = "balanced" | "skewed" | "exposed" | "crisis";

// ============================================================================
// Trades
// ============================================================================

/** Trade side. */
export type TradeSide = "BUY" | "SELL";

/** Order type. */
export type OrderType = "MARKET" | "LIMIT" | "SHADOW";

/** Trade execution status. */
export type TradeStatus = "FILLED" | "PARTIAL" | "CANCELLED" | "FAILED";

/** Outcome side. */
export type Outcome = "yes" | "no";

/**
 * Record of a trade executed by the bot.
 */
export interface Trade {
  /** Unique trade identifier (UUID string). */
  trade_id: string;
  /** Session this trade belongs to (UUID string). */
  session_id: string;
  /** Decision ID that triggered this trade. */
  decision_id: number;
  /** Event ID for the market. */
  event_id: string;
  /** Token ID that was traded. */
  token_id: string;
  /** Outcome side (YES/NO). */
  outcome: Outcome;
  /** Trade side (BUY/SELL). */
  side: TradeSide;
  /** Order type. */
  order_type: OrderType;
  /** Requested price (string for precision). */
  requested_price: string;
  /** Requested size (string for precision). */
  requested_size: string;
  /** Actual fill price (string for precision). */
  fill_price: string;
  /** Actual fill size (string for precision). */
  fill_size: string;
  /** Slippage in basis points. */
  slippage_bps: number;
  /** Trading fees paid (string for precision). */
  fees: string;
  /** Total cost of the trade (string for precision). */
  total_cost: string;
  /** When the order was placed (ISO 8601). */
  order_time: string;
  /** When the fill occurred (ISO 8601). */
  fill_time: string;
  /** Latency from order to fill in milliseconds. */
  latency_ms: number;
  /** Spot price at the time of fill (string for precision). */
  spot_price_at_fill: string;
  /** Arbitrage margin at time of fill (string for precision). */
  arb_margin_at_fill: string;
  /** Execution status. */
  status: TradeStatus;
}

// ============================================================================
// Control State
// ============================================================================

/**
 * Control flags state for dashboard.
 */
export interface ControlState {
  /** Whether trading is globally enabled. */
  trading_enabled: boolean;
  /** Whether circuit breaker is tripped. */
  circuit_breaker_tripped: boolean;
  /** Consecutive failure count. */
  consecutive_failures: number;
  /** Circuit breaker trip time (ISO 8601), null if not tripped. */
  circuit_breaker_trip_time: string | null;
  /** Whether shutdown has been requested. */
  shutdown_requested: boolean;
}

// ============================================================================
// Logs
// ============================================================================

/** Log level. */
export type LogLevel = "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR";

/**
 * Structured log entry.
 */
export interface LogEntry {
  /** Session this log belongs to (UUID string). */
  session_id: string;
  /** Log timestamp (ISO 8601). */
  timestamp: string;
  /** Log level. */
  level: LogLevel;
  /** Module path (e.g., "poly_bot::strategy::arb"). */
  target: string;
  /** Log message. */
  message: string;
  /** Associated event ID (if applicable). */
  event_id: string | null;
  /** Associated token ID (if applicable). */
  token_id: string | null;
  /** Associated trade ID (if applicable). */
  trade_id: string | null;
  /** Additional structured fields as JSON string. */
  fields: string;
}

// ============================================================================
// Anomalies
// ============================================================================

/** Anomaly severity level. */
export type AnomalySeverity = "low" | "medium" | "high" | "critical";

/**
 * An anomaly detected by the system.
 */
export interface Anomaly {
  /** Unique anomaly ID. */
  id: string;
  /** Anomaly type (e.g., "price_spike", "latency_spike"). */
  anomaly_type: string;
  /** Severity level. */
  severity: AnomalySeverity;
  /** Human-readable message. */
  message: string;
  /** When the anomaly was detected (ISO 8601). */
  detected_at: string;
  /** Associated event ID (if applicable). */
  event_id: string | null;
  /** Whether the anomaly has been dismissed. */
  dismissed: boolean;
}

// ============================================================================
// Equity Curve (from REST API)
// ============================================================================

/**
 * Single point on the equity curve.
 * Used for historical P&L visualization.
 */
export interface EquityPoint {
  /** Timestamp (ISO 8601). */
  timestamp: string;
  /** Total P&L at this point (string for precision). */
  total_pnl: string;
  /** Realized P&L (string for precision). */
  realized_pnl: string;
  /** Unrealized P&L (string for precision). */
  unrealized_pnl: string;
  /** Total exposure at this point (string for precision). */
  total_exposure: string;
  /** Trade count at this point. */
  trade_count: number;
}

// ============================================================================
// Session (from REST API)
// ============================================================================

/** Bot operating mode. */
export type BotMode = "live" | "paper" | "shadow" | "backtest";

/** Reason for session exit. */
export type ExitReason = "graceful" | "crash" | "circuit_breaker" | "manual";

/**
 * Session record from the sessions API.
 */
export interface Session {
  /** Unique session identifier (UUID string). */
  session_id: string;
  /** Operating mode for this session. */
  mode: BotMode;
  /** Session start time (ISO 8601). */
  start_time: string;
  /** Session end time (ISO 8601), null if still running. */
  end_time: string | null;
  /** SHA256 hash of config for reproducibility. */
  config_hash: string;
  /** Total realized P&L for the session (string for precision). */
  total_pnl: string;
  /** Total trading volume (string for precision). */
  total_volume: string;
  /** Number of trades executed. */
  trades_executed: number;
  /** Number of trades that failed. */
  trades_failed: number;
  /** Number of opportunities skipped. */
  trades_skipped: number;
  /** Total arbitrage opportunities detected. */
  opportunities_detected: number;
  /** Total market events processed. */
  events_processed: number;
  /** List of event IDs traded in this session. */
  markets_traded: string[];
  /** Reason for session exit, null if still running. */
  exit_reason: ExitReason | null;
}

// ============================================================================
// Market Session (from REST API)
// ============================================================================

/**
 * Per-market metrics within a trading session.
 */
export interface MarketSession {
  /** Session this belongs to (UUID string). */
  session_id: string;
  /** Event ID for the market. */
  event_id: string;
  /** Asset this market tracks. */
  asset: string;
  /** Strike price (string for precision). */
  strike_price: string;
  /** Window start time (ISO 8601). */
  window_start: string;
  /** Window end time (ISO 8601). */
  window_end: string;
  /** Time of first trade in this market (ISO 8601), null if no trades. */
  first_trade_time: string | null;
  /** YES shares held (string for precision). */
  yes_shares: string;
  /** NO shares held (string for precision). */
  no_shares: string;
  /** Cost basis for YES shares (string for precision). */
  yes_cost_basis: string;
  /** Cost basis for NO shares (string for precision). */
  no_cost_basis: string;
  /** Realized P&L from this market (string for precision). */
  realized_pnl: string;
  /** Unrealized P&L at current prices (string for precision). */
  unrealized_pnl: string;
  /** Number of trades in this market. */
  trades_count: number;
  /** Number of opportunities detected. */
  opportunities_count: number;
  /** Number of opportunities skipped. */
  skipped_count: number;
  /** Trading volume in this market (string for precision). */
  volume: string;
  /** Fees paid in this market (string for precision). */
  fees: string;
  /** Settlement outcome, null if not settled. */
  settlement_outcome: Outcome | null;
  /** P&L from settlement (string for precision), null if not settled. */
  settlement_pnl: string | null;
}

// ============================================================================
// API Response Types
// ============================================================================

/**
 * Paginated response wrapper.
 */
export interface PaginatedResponse<T> {
  /** Data items. */
  data: T[];
  /** Total count of items. */
  total: number;
  /** Current page (1-indexed). */
  page: number;
  /** Page size. */
  page_size: number;
  /** Total pages. */
  total_pages: number;
}

/**
 * API error response.
 */
export interface ApiError {
  /** Error message. */
  error: string;
  /** Error code. */
  code: string;
}

// ============================================================================
// Helper Types
// ============================================================================

/**
 * WebSocket connection status.
 */
export type ConnectionStatus = "connecting" | "connected" | "disconnected" | "error";

/**
 * Parse a decimal string to number.
 * Use with caution - may lose precision for very large/small values.
 */
export function parseDecimal(value: string): number {
  return parseFloat(value);
}

/**
 * Format a decimal string for display (2 decimal places).
 */
export function formatUsd(value: string): string {
  const num = parseFloat(value);
  return num.toLocaleString("en-US", {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

/**
 * Format a decimal string as percentage.
 */
export function formatPercent(value: string | number): string {
  const num = typeof value === "string" ? parseFloat(value) : value;
  return `${(num * 100).toFixed(2)}%`;
}

/**
 * Format basis points for display.
 */
export function formatBps(bps: number): string {
  return `${bps} bps`;
}

/**
 * Format a timestamp for display.
 */
export function formatTimestamp(iso: string): string {
  return new Date(iso).toLocaleString();
}

/**
 * Format seconds remaining as mm:ss.
 */
export function formatTimeRemaining(seconds: number): string {
  const mins = Math.floor(seconds / 60);
  const secs = seconds % 60;
  return `${mins}:${secs.toString().padStart(2, "0")}`;
}
