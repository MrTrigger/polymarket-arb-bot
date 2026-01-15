import { create } from "zustand";
import { subscribeWithSelector } from "zustand/middleware";
import type {
  ActiveMarket,
  Anomaly,
  ControlState,
  DashboardSnapshot,
  LogEntry,
  MetricsSnapshot,
  Position,
  Trade,
} from "./types";

/**
 * Dashboard store state.
 */
export interface DashboardState {
  /** Latest snapshot from WebSocket. */
  snapshot: DashboardSnapshot | null;
  /** Timestamp of last update. */
  lastUpdate: Date | null;
  /** Whether we've received at least one snapshot. */
  initialized: boolean;
}

/**
 * Dashboard store actions.
 */
export interface DashboardActions {
  /** Update state from a new WebSocket snapshot. */
  updateFromSnapshot: (snapshot: DashboardSnapshot) => void;
  /** Reset the store to initial state. */
  reset: () => void;
}

/**
 * Combined store type.
 */
export type DashboardStore = DashboardState & DashboardActions;

/**
 * Initial state values.
 */
const initialState: DashboardState = {
  snapshot: null,
  lastUpdate: null,
  initialized: false,
};

/**
 * Zustand store for dashboard state.
 *
 * Uses subscribeWithSelector middleware for efficient selective subscriptions.
 * Components can subscribe to specific slices of state to minimize re-renders.
 *
 * @example
 * ```tsx
 * // Subscribe to just metrics
 * const metrics = useDashboardStore((state) => state.snapshot?.metrics);
 *
 * // Subscribe with shallow equality
 * const markets = useDashboardStore(
 *   (state) => state.snapshot?.markets ?? [],
 *   shallow
 * );
 * ```
 */
export const useDashboardStore = create<DashboardStore>()(
  subscribeWithSelector((set) => ({
    // State
    ...initialState,

    // Actions
    updateFromSnapshot: (snapshot: DashboardSnapshot) => {
      set({
        snapshot,
        lastUpdate: new Date(),
        initialized: true,
      });
    },

    reset: () => {
      set(initialState);
    },
  }))
);

// ============================================================================
// Selectors
// ============================================================================
// These are pure functions for selecting specific state slices.
// Use with useDashboardStore for efficient subscriptions.

/**
 * Select metrics snapshot.
 */
export const selectMetrics = (
  state: DashboardStore
): MetricsSnapshot | null => {
  return state.snapshot?.metrics ?? null;
};

/**
 * Select active markets.
 */
export const selectMarkets = (state: DashboardStore): ActiveMarket[] => {
  return state.snapshot?.markets ?? [];
};

/**
 * Select markets with active arb opportunities.
 */
export const selectArbOpportunities = (
  state: DashboardStore
): ActiveMarket[] => {
  return (state.snapshot?.markets ?? []).filter((m) => m.has_arb_opportunity);
};

/**
 * Select positions.
 */
export const selectPositions = (state: DashboardStore): Position[] => {
  return state.snapshot?.positions ?? [];
};

/**
 * Select recent trades.
 */
export const selectRecentTrades = (state: DashboardStore): Trade[] => {
  return state.snapshot?.recent_trades ?? [];
};

/**
 * Select recent logs.
 */
export const selectRecentLogs = (state: DashboardStore): LogEntry[] => {
  return state.snapshot?.recent_logs ?? [];
};

/**
 * Select logs filtered by level.
 */
export const selectLogsByLevel =
  (levels: string[]) =>
  (state: DashboardStore): LogEntry[] => {
    return (state.snapshot?.recent_logs ?? []).filter((log) =>
      levels.includes(log.level)
    );
  };

/**
 * Select control state.
 */
export const selectControlState = (
  state: DashboardStore
): ControlState | null => {
  return state.snapshot?.control ?? null;
};

/**
 * Select anomalies.
 */
export const selectAnomalies = (state: DashboardStore): Anomaly[] => {
  return state.snapshot?.anomalies ?? [];
};

/**
 * Select active (non-dismissed) anomalies.
 */
export const selectActiveAnomalies = (state: DashboardStore): Anomaly[] => {
  return (state.snapshot?.anomalies ?? []).filter((a) => !a.dismissed);
};

/**
 * Select whether trading is currently enabled.
 */
export const selectTradingEnabled = (state: DashboardStore): boolean => {
  return state.snapshot?.control?.trading_enabled ?? false;
};

/**
 * Select whether circuit breaker is tripped.
 */
export const selectCircuitBreakerTripped = (
  state: DashboardStore
): boolean => {
  return state.snapshot?.control?.circuit_breaker_tripped ?? false;
};

/**
 * Select total P&L as string (for precision).
 */
export const selectPnl = (state: DashboardStore): string => {
  return state.snapshot?.metrics?.pnl_usdc ?? "0";
};

/**
 * Select total volume as string (for precision).
 */
export const selectVolume = (state: DashboardStore): string => {
  return state.snapshot?.metrics?.volume_usdc ?? "0";
};

/**
 * Select trade counts.
 */
export const selectTradeCounts = (
  state: DashboardStore
): {
  executed: number;
  failed: number;
  skipped: number;
} => {
  const metrics = state.snapshot?.metrics;
  return {
    executed: metrics?.trades_executed ?? 0,
    failed: metrics?.trades_failed ?? 0,
    skipped: metrics?.trades_skipped ?? 0,
  };
};

/**
 * Select win rate (null if no trades).
 */
export const selectWinRate = (state: DashboardStore): number | null => {
  return state.snapshot?.metrics?.win_rate ?? null;
};

/**
 * Select a specific market by event ID.
 */
export const selectMarketById =
  (eventId: string) =>
  (state: DashboardStore): ActiveMarket | null => {
    return (
      (state.snapshot?.markets ?? []).find((m) => m.event_id === eventId) ??
      null
    );
  };

/**
 * Select position for a specific market.
 */
export const selectPositionByEventId =
  (eventId: string) =>
  (state: DashboardStore): Position | null => {
    return (
      (state.snapshot?.positions ?? []).find((p) => p.event_id === eventId) ??
      null
    );
  };

/**
 * Select trades for a specific market.
 */
export const selectTradesByEventId =
  (eventId: string) =>
  (state: DashboardStore): Trade[] => {
    return (state.snapshot?.recent_trades ?? []).filter(
      (t) => t.event_id === eventId
    );
  };

export default useDashboardStore;
