/**
 * API client for bot control endpoints.
 *
 * These functions communicate with the REST API to control the bot.
 */

import type { BotModeValue } from "./types";

/** API base URL - defaults to same origin in production */
const API_BASE =
  import.meta.env.VITE_API_URL || `http://localhost:${import.meta.env.VITE_API_PORT || 3002}`;

/**
 * Response from control endpoints.
 */
export interface ControlResponse {
  success: boolean;
  message: string;
}

/**
 * Helper function to make POST requests to control endpoints.
 */
async function postControl(
  endpoint: string,
  body?: object
): Promise<ControlResponse> {
  const response = await fetch(`${API_BASE}/api/control/${endpoint}`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
    },
    body: body ? JSON.stringify(body) : undefined,
  });

  if (!response.ok) {
    const error = await response.json().catch(() => ({ error: "Unknown error" }));
    throw new Error(error.message || error.error || "Request failed");
  }

  return response.json();
}

/**
 * Pause trading.
 */
export async function pauseTrading(): Promise<ControlResponse> {
  return postControl("pause");
}

/**
 * Resume trading.
 */
export async function resumeTrading(): Promise<ControlResponse> {
  return postControl("resume");
}

/**
 * Initiate graceful shutdown.
 */
export async function stopBot(): Promise<ControlResponse> {
  return postControl("stop");
}

/**
 * Reset the circuit breaker.
 */
export async function resetCircuitBreaker(): Promise<ControlResponse> {
  return postControl("reset-circuit-breaker");
}

/**
 * Cancel all pending orders.
 */
export async function cancelAllOrders(): Promise<ControlResponse> {
  return postControl("cancel-all-orders");
}

/**
 * Switch between paper and live trading modes.
 */
export async function switchMode(mode: BotModeValue): Promise<ControlResponse> {
  return postControl("switch-mode", { mode });
}

/**
 * Get current control status.
 */
export async function getControlStatus(): Promise<{
  trading_enabled: boolean;
  circuit_breaker_tripped: boolean;
  consecutive_failures: number;
  shutdown_requested: boolean;
  bot_status: string;
  current_mode: string;
  allowance_status: string;
  pending_orders_count: number;
  error_message: string | null;
}> {
  const response = await fetch(`${API_BASE}/api/control/status`);
  if (!response.ok) {
    throw new Error("Failed to fetch control status");
  }
  return response.json();
}
