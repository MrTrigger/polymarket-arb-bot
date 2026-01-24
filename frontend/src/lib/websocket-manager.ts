/**
 * Singleton WebSocket manager for the dashboard.
 *
 * Ensures only ONE WebSocket connection exists across the entire app,
 * regardless of how many components need dashboard state.
 */

import type { ConnectionStatus, DashboardSnapshot } from "./types";
import { useDashboardStore } from "./store";

export interface WebSocketManagerConfig {
  url: string;
  reconnectDelay: number;
  maxReconnectDelay: number;
  backoffMultiplier: number;
}

const DEFAULT_CONFIG: WebSocketManagerConfig = {
  url: "ws://localhost:3001",
  reconnectDelay: 1000,
  maxReconnectDelay: 30000,
  backoffMultiplier: 2,
};

type StatusListener = (status: ConnectionStatus) => void;

class WebSocketManager {
  private static instance: WebSocketManager | null = null;

  private config: WebSocketManagerConfig;
  private ws: WebSocket | null = null;
  private status: ConnectionStatus = "disconnected";
  private reconnectTimeout: ReturnType<typeof setTimeout> | null = null;
  private reconnectDelay: number;
  private shouldReconnect = true;
  private statusListeners: Set<StatusListener> = new Set();
  private lastError: string | null = null;

  private constructor(config: WebSocketManagerConfig) {
    this.config = config;
    this.reconnectDelay = config.reconnectDelay;
  }

  /**
   * Get the singleton instance.
   */
  static getInstance(config?: Partial<WebSocketManagerConfig>): WebSocketManager {
    if (!WebSocketManager.instance) {
      WebSocketManager.instance = new WebSocketManager({
        ...DEFAULT_CONFIG,
        ...config,
      });
    }
    return WebSocketManager.instance;
  }

  /**
   * Get current connection status.
   */
  getStatus(): ConnectionStatus {
    return this.status;
  }

  /**
   * Get last error message.
   */
  getLastError(): string | null {
    return this.lastError;
  }

  /**
   * Subscribe to status changes.
   */
  onStatusChange(listener: StatusListener): () => void {
    this.statusListeners.add(listener);
    // Immediately notify of current status
    listener(this.status);
    return () => {
      this.statusListeners.delete(listener);
    };
  }

  private setStatus(status: ConnectionStatus, error?: string) {
    this.status = status;
    if (error !== undefined) {
      this.lastError = error;
    }
    this.statusListeners.forEach((listener) => listener(status));
  }

  /**
   * Connect to the WebSocket server.
   */
  connect(): void {
    // Don't connect if already connected or connecting
    if (
      this.ws?.readyState === WebSocket.CONNECTING ||
      this.ws?.readyState === WebSocket.OPEN
    ) {
      return;
    }

    this.shouldReconnect = true;

    // Clean up existing connection
    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }

    this.setStatus("connecting");

    try {
      const ws = new WebSocket(this.config.url);
      this.ws = ws;

      ws.onopen = () => {
        this.setStatus("connected");
        this.lastError = null;
        // Reset reconnect delay on successful connection
        this.reconnectDelay = this.config.reconnectDelay;
      };

      ws.onmessage = (event) => {
        try {
          const data = JSON.parse(event.data) as DashboardSnapshot;
          // Update the Zustand store directly
          useDashboardStore.getState().updateFromSnapshot(data);
        } catch (parseError) {
          console.error("Failed to parse WebSocket message:", parseError);
        }
      };

      ws.onerror = (event) => {
        console.error("WebSocket error:", event);
        this.setStatus("error", "WebSocket connection error");
      };

      ws.onclose = (event) => {
        this.ws = null;

        if (event.wasClean) {
          this.setStatus("disconnected");
        } else {
          this.setStatus("error", `Connection closed unexpectedly (code: ${event.code})`);
        }

        // Schedule reconnect if we should
        if (this.shouldReconnect) {
          this.scheduleReconnect();
        }
      };
    } catch (err) {
      this.setStatus("error", err instanceof Error ? err.message : "Failed to connect");
      this.scheduleReconnect();
    }
  }

  /**
   * Disconnect from the WebSocket server.
   */
  disconnect(): void {
    this.shouldReconnect = false;
    this.clearReconnectTimeout();

    if (this.ws) {
      this.ws.close(1000, "Client disconnect");
      this.ws = null;
    }

    this.setStatus("disconnected");
  }

  private clearReconnectTimeout(): void {
    if (this.reconnectTimeout) {
      clearTimeout(this.reconnectTimeout);
      this.reconnectTimeout = null;
    }
  }

  private scheduleReconnect(): void {
    if (!this.shouldReconnect) {
      return;
    }

    this.clearReconnectTimeout();

    const delay = this.reconnectDelay;
    this.reconnectTimeout = setTimeout(() => {
      if (this.shouldReconnect) {
        // Increase delay for next attempt (exponential backoff)
        this.reconnectDelay = Math.min(
          delay * this.config.backoffMultiplier,
          this.config.maxReconnectDelay
        );
        this.connect();
      }
    }, delay);
  }
}

// Export a function to get the singleton
export function getWebSocketManager(config?: Partial<WebSocketManagerConfig>): WebSocketManager {
  return WebSocketManager.getInstance(config);
}

// Auto-connect on module load (for convenience)
// Components can still call connect/disconnect manually
let autoConnected = false;
export function ensureConnected(): void {
  if (!autoConnected) {
    autoConnected = true;
    getWebSocketManager().connect();
  }
}
