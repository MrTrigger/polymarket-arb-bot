import { useCallback, useEffect, useRef, useState } from "react";
import type { ConnectionStatus, DashboardSnapshot } from "@/lib/types";

/**
 * Configuration for the WebSocket connection.
 */
export interface WebSocketConfig {
  /** WebSocket server URL. Defaults to ws://localhost:3001 */
  url?: string;
  /** Initial reconnect delay in ms. Defaults to 1000. */
  reconnectDelay?: number;
  /** Maximum reconnect delay in ms. Defaults to 30000. */
  maxReconnectDelay?: number;
  /** Reconnect backoff multiplier. Defaults to 2. */
  backoffMultiplier?: number;
  /** Whether to automatically connect on mount. Defaults to true. */
  autoConnect?: boolean;
}

const DEFAULT_CONFIG: Required<WebSocketConfig> = {
  url: "ws://localhost:3001",
  reconnectDelay: 1000,
  maxReconnectDelay: 30000,
  backoffMultiplier: 2,
  autoConnect: true,
};

/**
 * Return type for the useWebSocket hook.
 */
export interface UseWebSocketReturn {
  /** Current connection status. */
  status: ConnectionStatus;
  /** Latest snapshot received, null if none received yet. */
  snapshot: DashboardSnapshot | null;
  /** Last error message, null if no error. */
  error: string | null;
  /** Manually connect to the WebSocket server. */
  connect: () => void;
  /** Manually disconnect from the WebSocket server. */
  disconnect: () => void;
  /** Time of last successful message received. */
  lastMessageTime: Date | null;
}

/**
 * React hook for WebSocket connection with auto-reconnect.
 *
 * Features:
 * - Connects to configurable WebSocket URL
 * - Auto-reconnects with exponential backoff on disconnect
 * - Parses incoming JSON as DashboardSnapshot
 * - Exposes connection status for UI
 * - Handles errors gracefully
 *
 * @param config - WebSocket configuration options
 * @returns WebSocket state and control functions
 *
 * @example
 * ```tsx
 * const { status, snapshot, error, connect, disconnect } = useWebSocket({
 *   url: "ws://localhost:3001",
 * });
 *
 * if (status === "connected" && snapshot) {
 *   console.log("P&L:", snapshot.metrics.pnl_usdc);
 * }
 * ```
 */
export function useWebSocket(config: WebSocketConfig = {}): UseWebSocketReturn {
  const mergedConfig = { ...DEFAULT_CONFIG, ...config };

  const [status, setStatus] = useState<ConnectionStatus>("disconnected");
  const [snapshot, setSnapshot] = useState<DashboardSnapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [lastMessageTime, setLastMessageTime] = useState<Date | null>(null);

  // Refs for managing connection state across renders
  const wsRef = useRef<WebSocket | null>(null);
  const reconnectTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(
    null
  );
  const reconnectDelayRef = useRef(mergedConfig.reconnectDelay);
  const shouldReconnectRef = useRef(true);
  const mountedRef = useRef(true);
  // Ref to hold the connect function to break circular dependency
  const connectInternalRef = useRef<(() => void) | null>(null);

  /**
   * Clean up reconnect timeout.
   */
  const clearReconnectTimeout = useCallback(() => {
    if (reconnectTimeoutRef.current) {
      clearTimeout(reconnectTimeoutRef.current);
      reconnectTimeoutRef.current = null;
    }
  }, []);

  /**
   * Schedule a reconnect with exponential backoff.
   */
  const scheduleReconnect = useCallback(() => {
    if (!shouldReconnectRef.current || !mountedRef.current) {
      return;
    }

    clearReconnectTimeout();

    const delay = reconnectDelayRef.current;
    reconnectTimeoutRef.current = setTimeout(() => {
      if (mountedRef.current && shouldReconnectRef.current) {
        // Increase delay for next attempt (exponential backoff)
        reconnectDelayRef.current = Math.min(
          delay * mergedConfig.backoffMultiplier,
          mergedConfig.maxReconnectDelay
        );
        // Use ref to call connect function to avoid circular dependency
        connectInternalRef.current?.();
      }
    }, delay);
  }, [clearReconnectTimeout, mergedConfig.backoffMultiplier, mergedConfig.maxReconnectDelay]);

  /**
   * Internal connect function.
   */
  const connectInternal = useCallback(() => {
    // Don't connect if already connected or connecting
    if (
      wsRef.current?.readyState === WebSocket.CONNECTING ||
      wsRef.current?.readyState === WebSocket.OPEN
    ) {
      return;
    }

    // Clean up existing connection
    if (wsRef.current) {
      wsRef.current.close();
      wsRef.current = null;
    }

    setStatus("connecting");
    setError(null);

    try {
      const ws = new WebSocket(mergedConfig.url);
      wsRef.current = ws;

      ws.onopen = () => {
        if (!mountedRef.current) return;
        setStatus("connected");
        setError(null);
        // Reset reconnect delay on successful connection
        reconnectDelayRef.current = mergedConfig.reconnectDelay;
      };

      ws.onmessage = (event) => {
        if (!mountedRef.current) return;
        try {
          const data = JSON.parse(event.data) as DashboardSnapshot;
          setSnapshot(data);
          setLastMessageTime(new Date());
        } catch (parseError) {
          console.error("Failed to parse WebSocket message:", parseError);
          // Don't set error state for parse errors - connection is still valid
        }
      };

      ws.onerror = (event) => {
        if (!mountedRef.current) return;
        console.error("WebSocket error:", event);
        setStatus("error");
        setError("WebSocket connection error");
      };

      ws.onclose = (event) => {
        if (!mountedRef.current) return;

        wsRef.current = null;

        if (event.wasClean) {
          setStatus("disconnected");
        } else {
          setStatus("error");
          setError(`Connection closed unexpectedly (code: ${event.code})`);
        }

        // Schedule reconnect if we should
        if (shouldReconnectRef.current) {
          scheduleReconnect();
        }
      };
    } catch (err) {
      setStatus("error");
      setError(err instanceof Error ? err.message : "Failed to connect");
      scheduleReconnect();
    }
  }, [mergedConfig.url, mergedConfig.reconnectDelay, scheduleReconnect]);

  // Keep the ref updated with the latest connectInternal function
  useEffect(() => {
    connectInternalRef.current = connectInternal;
  }, [connectInternal]);

  /**
   * Public connect function - enables auto-reconnect and connects.
   */
  const connect = useCallback(() => {
    shouldReconnectRef.current = true;
    connectInternal();
  }, [connectInternal]);

  /**
   * Disconnect and disable auto-reconnect.
   */
  const disconnect = useCallback(() => {
    shouldReconnectRef.current = false;
    clearReconnectTimeout();

    if (wsRef.current) {
      wsRef.current.close(1000, "Client disconnect");
      wsRef.current = null;
    }

    setStatus("disconnected");
  }, [clearReconnectTimeout]);

  // Auto-connect on mount - use setTimeout to avoid sync setState in effect
  useEffect(() => {
    mountedRef.current = true;

    if (mergedConfig.autoConnect) {
      // Schedule connection on next tick to avoid sync setState
      const timerId = setTimeout(() => {
        if (mountedRef.current) {
          connect();
        }
      }, 0);
      return () => {
        clearTimeout(timerId);
        mountedRef.current = false;
        shouldReconnectRef.current = false;
        clearReconnectTimeout();
        if (wsRef.current) {
          wsRef.current.close(1000, "Component unmount");
          wsRef.current = null;
        }
      };
    }

    return () => {
      mountedRef.current = false;
      shouldReconnectRef.current = false;
      clearReconnectTimeout();

      if (wsRef.current) {
        wsRef.current.close(1000, "Component unmount");
        wsRef.current = null;
      }
    };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps
  // Only run on mount/unmount - config changes don't reconnect

  return {
    status,
    snapshot,
    error,
    connect,
    disconnect,
    lastMessageTime,
  };
}

export default useWebSocket;
