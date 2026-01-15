import { useDashboardState } from "@/hooks";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Shield,
  ShieldAlert,
  ShieldOff,
  AlertTriangle,
  Clock,
} from "lucide-react";

/**
 * Circuit breaker status indicator showing system health.
 *
 * Status colors:
 * - Green: Trading enabled, circuit breaker healthy
 * - Yellow: Trading disabled manually
 * - Red: Circuit breaker tripped
 *
 * Displays:
 * - Current status with icon
 * - Consecutive failure count
 * - Cooldown remaining (if tripped)
 */
export function CircuitBreakerStatus() {
  const { controlState, tradingEnabled, circuitBreakerTripped, initialized } =
    useDashboardState();

  if (!initialized || !controlState) {
    return <CircuitBreakerStatusSkeleton />;
  }

  // Determine status and styling
  const status = getStatus(tradingEnabled, circuitBreakerTripped);
  const cooldownRemaining = getCooldownRemaining(
    controlState.circuit_breaker_trip_time
  );

  return (
    <Card className={`border-2 ${status.borderColor}`}>
      <CardHeader className="pb-2">
        <CardTitle className="flex items-center gap-2 text-sm font-medium">
          <status.Icon className={`h-4 w-4 ${status.iconColor}`} />
          Circuit Breaker
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        {/* Status badge */}
        <div className="flex items-center gap-2">
          <div
            className={`h-3 w-3 rounded-full ${status.dotColor}`}
            aria-label={`Status: ${status.label}`}
          />
          <span className={`text-sm font-medium ${status.textColor}`}>
            {status.label}
          </span>
        </div>

        {/* Failure count */}
        <div className="flex items-center justify-between text-sm">
          <span className="text-muted-foreground">Consecutive Failures</span>
          <span
            className={`font-mono ${
              controlState.consecutive_failures > 0
                ? "text-yellow-500"
                : "text-muted-foreground"
            }`}
          >
            {controlState.consecutive_failures}
          </span>
        </div>

        {/* Cooldown remaining (only shown when tripped) */}
        {circuitBreakerTripped && cooldownRemaining !== null && (
          <div className="flex items-center gap-2 rounded-md bg-red-500/10 px-3 py-2">
            <Clock className="h-4 w-4 text-red-500" />
            <div className="flex flex-col">
              <span className="text-xs text-muted-foreground">
                Cooldown remaining
              </span>
              <span className="font-mono text-sm text-red-500">
                {formatCooldown(cooldownRemaining)}
              </span>
            </div>
          </div>
        )}

        {/* Warning when failures accumulating */}
        {!circuitBreakerTripped && controlState.consecutive_failures > 0 && (
          <div className="flex items-center gap-2 rounded-md bg-yellow-500/10 px-3 py-2">
            <AlertTriangle className="h-4 w-4 text-yellow-500" />
            <span className="text-xs text-yellow-500">
              {controlState.consecutive_failures} failure
              {controlState.consecutive_failures !== 1 ? "s" : ""} - breaker may
              trip soon
            </span>
          </div>
        )}

        {/* Shutdown requested indicator */}
        {controlState.shutdown_requested && (
          <div className="flex items-center gap-2 rounded-md bg-orange-500/10 px-3 py-2">
            <ShieldOff className="h-4 w-4 text-orange-500" />
            <span className="text-xs text-orange-500">Shutdown requested</span>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

/**
 * Skeleton loader for CircuitBreakerStatus.
 */
export function CircuitBreakerStatusSkeleton() {
  return (
    <Card className="border-2 border-border">
      <CardHeader className="pb-2">
        <CardTitle className="flex items-center gap-2 text-sm font-medium">
          <div className="h-4 w-4 animate-pulse rounded bg-muted" />
          Circuit Breaker
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex items-center gap-2">
          <div className="h-3 w-3 animate-pulse rounded-full bg-muted" />
          <div className="h-4 w-20 animate-pulse rounded bg-muted" />
        </div>
        <div className="flex items-center justify-between">
          <div className="h-4 w-32 animate-pulse rounded bg-muted" />
          <div className="h-4 w-8 animate-pulse rounded bg-muted" />
        </div>
      </CardContent>
    </Card>
  );
}

// =============================================================================
// Helper Types and Functions
// =============================================================================

interface StatusConfig {
  label: string;
  Icon: typeof Shield;
  borderColor: string;
  dotColor: string;
  iconColor: string;
  textColor: string;
}

/**
 * Get status configuration based on current state.
 */
function getStatus(
  tradingEnabled: boolean,
  circuitBreakerTripped: boolean
): StatusConfig {
  if (circuitBreakerTripped) {
    return {
      label: "Tripped",
      Icon: ShieldAlert,
      borderColor: "border-red-500/50",
      dotColor: "bg-red-500 animate-pulse",
      iconColor: "text-red-500",
      textColor: "text-red-500",
    };
  }

  if (!tradingEnabled) {
    return {
      label: "Disabled",
      Icon: ShieldOff,
      borderColor: "border-yellow-500/50",
      dotColor: "bg-yellow-500",
      iconColor: "text-yellow-500",
      textColor: "text-yellow-500",
    };
  }

  return {
    label: "Active",
    Icon: Shield,
    borderColor: "border-green-500/50",
    dotColor: "bg-green-500",
    iconColor: "text-green-500",
    textColor: "text-green-500",
  };
}

/**
 * Calculate cooldown remaining in seconds from trip time.
 * Returns null if trip time is not set.
 */
function getCooldownRemaining(tripTime: string | null): number | null {
  if (!tripTime) return null;

  const tripDate = new Date(tripTime);
  const now = new Date();

  // Assuming a 30-second cooldown period (adjust based on actual bot config)
  const cooldownMs = 30 * 1000;
  const elapsed = now.getTime() - tripDate.getTime();
  const remaining = cooldownMs - elapsed;

  if (remaining <= 0) return 0;
  return Math.ceil(remaining / 1000);
}

/**
 * Format cooldown seconds for display.
 */
function formatCooldown(seconds: number): string {
  if (seconds <= 0) return "Resetting...";
  if (seconds < 60) return `${seconds}s`;
  const mins = Math.floor(seconds / 60);
  const secs = seconds % 60;
  return `${mins}m ${secs}s`;
}

export default CircuitBreakerStatus;
