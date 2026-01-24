import { useEffect, useState, useCallback } from "react";
import { useShallow } from "zustand/react/shallow";
import { useDashboardStore } from "@/lib/store";
import { getWebSocketManager, ensureConnected } from "@/lib/websocket-manager";
import type {
  ActiveMarket,
  Anomaly,
  ConnectionStatus,
  ControlState,
  LogEntry,
  MetricsSnapshot,
  Position,
  Trade,
} from "@/lib/types";

/**
 * Return type for the useDashboardState hook.
 */
export interface UseDashboardStateReturn {
  // Connection state
  /** WebSocket connection status. */
  connectionStatus: ConnectionStatus;
  /** Last WebSocket error, null if none. */
  connectionError: string | null;
  /** Manually connect to WebSocket. */
  connect: () => void;
  /** Manually disconnect from WebSocket. */
  disconnect: () => void;

  // Dashboard state
  /** Whether we've received at least one snapshot. */
  initialized: boolean;
  /** Time of last store update. */
  lastUpdate: Date | null;
  /** Snapshot timestamp from backend (ISO 8601). */
  snapshotTimestamp: string | null;

  // Metrics
  /** Current metrics snapshot. */
  metrics: MetricsSnapshot | null;

  // Markets
  /** All active markets. */
  markets: ActiveMarket[];
  /** Markets with arb opportunities. */
  arbOpportunities: ActiveMarket[];

  // Positions
  /** All current positions. */
  positions: Position[];

  // Trades
  /** Recent trades (last 100). */
  recentTrades: Trade[];

  // Logs
  /** Recent logs (last 100). */
  recentLogs: LogEntry[];

  // Control
  /** Control state (trading enabled, circuit breaker). */
  controlState: ControlState | null;
  /** Whether trading is enabled. */
  tradingEnabled: boolean;
  /** Whether circuit breaker is tripped. */
  circuitBreakerTripped: boolean;

  // Anomalies
  /** All anomalies. */
  anomalies: Anomaly[];
  /** Active (non-dismissed) anomalies. */
  activeAnomalies: Anomaly[];
}

/**
 * Combined hook for WebSocket connection and dashboard state.
 *
 * Uses a SINGLETON WebSocket connection shared across ALL components.
 * This prevents multiple connections when many components use this hook.
 *
 * @returns Combined WebSocket and dashboard state
 */
export function useDashboardState(): UseDashboardStateReturn {
  // Ensure WebSocket is connected (singleton - only connects once)
  useEffect(() => {
    ensureConnected();
  }, []);

  // Subscribe to WebSocket status changes
  const [connectionStatus, setConnectionStatus] = useState<ConnectionStatus>(
    () => getWebSocketManager().getStatus()
  );
  const [connectionError, setConnectionError] = useState<string | null>(
    () => getWebSocketManager().getLastError()
  );

  useEffect(() => {
    const manager = getWebSocketManager();
    const unsubscribe = manager.onStatusChange((status) => {
      setConnectionStatus(status);
      setConnectionError(manager.getLastError());
    });
    return unsubscribe;
  }, []);

  // Connection controls
  const connect = useCallback(() => {
    getWebSocketManager().connect();
  }, []);

  const disconnect = useCallback(() => {
    getWebSocketManager().disconnect();
  }, []);

  // Store state - using useShallow for efficiency
  const { initialized, lastUpdate, snapshotTimestamp } = useDashboardStore(
    useShallow((state) => ({
      initialized: state.initialized,
      lastUpdate: state.lastUpdate,
      snapshotTimestamp: state.snapshot?.timestamp ?? null,
    }))
  );

  // Metrics
  const metrics = useDashboardStore(
    (state) => state.snapshot?.metrics ?? null
  );

  // Markets - use useShallow for array comparison
  const markets = useDashboardStore(
    useShallow((state) => state.snapshot?.markets ?? [])
  );

  const arbOpportunities = useDashboardStore(
    useShallow((state) =>
      (state.snapshot?.markets ?? []).filter((m) => m.has_arb_opportunity)
    )
  );

  // Positions
  const positions = useDashboardStore(
    useShallow((state) => state.snapshot?.positions ?? [])
  );

  // Trades
  const recentTrades = useDashboardStore(
    useShallow((state) => state.snapshot?.recent_trades ?? [])
  );

  // Logs
  const recentLogs = useDashboardStore(
    useShallow((state) => state.snapshot?.recent_logs ?? [])
  );

  // Control state
  const controlState = useDashboardStore(
    (state) => state.snapshot?.control ?? null
  );

  const tradingEnabled = useDashboardStore(
    (state) => state.snapshot?.control?.trading_enabled ?? false
  );

  const circuitBreakerTripped = useDashboardStore(
    (state) => state.snapshot?.control?.circuit_breaker_tripped ?? false
  );

  // Anomalies
  const anomalies = useDashboardStore(
    useShallow((state) => state.snapshot?.anomalies ?? [])
  );

  const activeAnomalies = useDashboardStore(
    useShallow((state) =>
      (state.snapshot?.anomalies ?? []).filter((a) => !a.dismissed)
    )
  );

  return {
    // Connection
    connectionStatus,
    connectionError,
    connect,
    disconnect,

    // State
    initialized,
    lastUpdate,
    snapshotTimestamp,

    // Metrics
    metrics,

    // Markets
    markets,
    arbOpportunities,

    // Positions
    positions,

    // Trades
    recentTrades,

    // Logs
    recentLogs,

    // Control
    controlState,
    tradingEnabled,
    circuitBreakerTripped,

    // Anomalies
    anomalies,
    activeAnomalies,
  };
}

export default useDashboardState;
