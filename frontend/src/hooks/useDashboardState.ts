import { useEffect } from "react";
import { useShallow } from "zustand/react/shallow";
import { useDashboardStore } from "@/lib/store";
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
import { useWebSocket, type WebSocketConfig } from "./useWebSocket";

/**
 * Return type for the useDashboardState hook.
 */
export interface UseDashboardStateReturn {
  // Connection state
  /** WebSocket connection status. */
  connectionStatus: ConnectionStatus;
  /** Last WebSocket error, null if none. */
  connectionError: string | null;
  /** Time of last message received. */
  lastMessageTime: Date | null;
  /** Manually connect to WebSocket. */
  connect: () => void;
  /** Manually disconnect from WebSocket. */
  disconnect: () => void;

  // Dashboard state
  /** Whether we've received at least one snapshot. */
  initialized: boolean;
  /** Time of last store update. */
  lastUpdate: Date | null;

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
 * This hook:
 * 1. Connects to the WebSocket server
 * 2. Updates the Zustand store from incoming snapshots
 * 3. Provides convenient access to all dashboard state
 *
 * Uses useShallow for array/object comparisons to minimize re-renders.
 *
 * @param config - WebSocket configuration options
 * @returns Combined WebSocket and dashboard state
 *
 * @example
 * ```tsx
 * function Dashboard() {
 *   const {
 *     connectionStatus,
 *     metrics,
 *     markets,
 *     tradingEnabled,
 *   } = useDashboardState();
 *
 *   if (connectionStatus !== "connected") {
 *     return <ConnectionStatus status={connectionStatus} />;
 *   }
 *
 *   return (
 *     <div>
 *       <MetricsCards metrics={metrics} />
 *       <MarketsGrid markets={markets} />
 *     </div>
 *   );
 * }
 * ```
 */
export function useDashboardState(
  config?: WebSocketConfig
): UseDashboardStateReturn {
  // WebSocket connection
  const {
    status: connectionStatus,
    snapshot: wsSnapshot,
    error: connectionError,
    connect,
    disconnect,
    lastMessageTime,
  } = useWebSocket(config);

  // Store actions
  const updateFromSnapshot = useDashboardStore(
    (state) => state.updateFromSnapshot
  );

  // Update store when WebSocket receives a new snapshot
  useEffect(() => {
    if (wsSnapshot) {
      updateFromSnapshot(wsSnapshot);
    }
  }, [wsSnapshot, updateFromSnapshot]);

  // Store state - using useShallow for efficiency
  const { initialized, lastUpdate } = useDashboardStore(
    useShallow((state) => ({
      initialized: state.initialized,
      lastUpdate: state.lastUpdate,
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
    lastMessageTime,
    connect,
    disconnect,

    // State
    initialized,
    lastUpdate,

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
