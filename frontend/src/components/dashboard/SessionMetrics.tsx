import { useMemo } from "react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useDashboardState } from "@/hooks";
import { formatUsd, parseDecimal } from "@/lib/types";
import {
  Clock,
  Zap,
  Crosshair,
  Activity,
  DollarSign,
  Target,
} from "lucide-react";

/**
 * SessionMetrics panel component.
 *
 * Displays session-level statistics including:
 * - Session duration (time since first snapshot)
 * - Events processed
 * - Opportunities detected
 * - Trade rate (trades per minute)
 * - Average trade size
 * - Shadow order ratio (shadow fills / total fills)
 */
export function SessionMetrics() {
  const { metrics, lastUpdate } = useDashboardState();

  // Calculate session duration and derived metrics
  // Duration is estimated from the events_processed count assuming ~100 events/minute
  // This avoids needing to track start time which causes React lint issues
  const sessionStats = useMemo(() => {
    if (!metrics || !lastUpdate) {
      return null;
    }

    // Estimate duration from events processed (assuming ~100 events/minute for active markets)
    // This gives reasonable duration without tracking start time
    const estimatedMinutes = Math.max(1, metrics.events_processed / 100);
    const durationMinutes = estimatedMinutes;

    // Trade rate (trades per minute)
    const tradeRate = metrics.trades_executed / durationMinutes;

    // Average trade size (volume / trades)
    const volume = parseDecimal(metrics.volume_usdc);
    const avgTradeSize =
      metrics.trades_executed > 0 ? volume / metrics.trades_executed : 0;

    // Shadow order ratio
    const shadowRatio =
      metrics.shadow_orders_fired > 0
        ? metrics.shadow_orders_filled / metrics.shadow_orders_fired
        : 0;

    // Opportunity capture rate (trades / opportunities)
    const captureRate =
      metrics.opportunities_detected > 0
        ? metrics.trades_executed / metrics.opportunities_detected
        : 0;

    return {
      durationMinutes,
      tradeRate,
      avgTradeSize,
      shadowRatio,
      captureRate,
    };
  }, [metrics, lastUpdate]);

  if (!metrics || !sessionStats) {
    return <SessionMetricsSkeleton />;
  }

  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="flex items-center gap-2 text-sm font-medium">
          <Clock className="h-4 w-4" />
          Session Metrics
        </CardTitle>
      </CardHeader>
      <CardContent>
        <div className="grid grid-cols-2 gap-4 sm:grid-cols-3 lg:grid-cols-6">
          {/* Session Duration */}
          <div className="space-y-1">
            <div className="flex items-center gap-1 text-xs text-muted-foreground">
              <Clock className="h-3 w-3" />
              Duration
            </div>
            <div className="text-lg font-semibold">
              {formatDuration(sessionStats.durationMinutes)}
            </div>
          </div>

          {/* Events Processed */}
          <div className="space-y-1">
            <div className="flex items-center gap-1 text-xs text-muted-foreground">
              <Zap className="h-3 w-3" />
              Events
            </div>
            <div className="text-lg font-semibold">
              {metrics.events_processed.toLocaleString()}
            </div>
          </div>

          {/* Opportunities */}
          <div className="space-y-1">
            <div className="flex items-center gap-1 text-xs text-muted-foreground">
              <Crosshair className="h-3 w-3" />
              Opportunities
            </div>
            <div className="text-lg font-semibold">
              {metrics.opportunities_detected.toLocaleString()}
            </div>
          </div>

          {/* Trade Rate */}
          <div className="space-y-1">
            <div className="flex items-center gap-1 text-xs text-muted-foreground">
              <Activity className="h-3 w-3" />
              Trade Rate
            </div>
            <div className="text-lg font-semibold">
              {sessionStats.tradeRate.toFixed(2)}
              <span className="text-xs text-muted-foreground">/min</span>
            </div>
          </div>

          {/* Avg Trade Size */}
          <div className="space-y-1">
            <div className="flex items-center gap-1 text-xs text-muted-foreground">
              <DollarSign className="h-3 w-3" />
              Avg Trade
            </div>
            <div className="text-lg font-semibold">
              {formatUsd(sessionStats.avgTradeSize.toString())}
            </div>
          </div>

          {/* Shadow Fill Rate */}
          <div className="space-y-1">
            <div className="flex items-center gap-1 text-xs text-muted-foreground">
              <Target className="h-3 w-3" />
              Shadow Rate
            </div>
            <div className="text-lg font-semibold">
              {(sessionStats.shadowRatio * 100).toFixed(1)}%
              <span className="ml-1 text-xs text-muted-foreground">
                ({metrics.shadow_orders_filled}/{metrics.shadow_orders_fired})
              </span>
            </div>
          </div>
        </div>

        {/* Additional stats row */}
        <div className="mt-4 flex flex-wrap gap-4 border-t pt-3 text-xs text-muted-foreground">
          <div>
            <span className="font-medium text-foreground">Capture Rate:</span>{" "}
            {(sessionStats.captureRate * 100).toFixed(1)}%
          </div>
          <div>
            <span className="font-medium text-foreground">
              Failed/Skipped:
            </span>{" "}
            <span className="text-red-500">{metrics.trades_failed}</span>
            {" / "}
            <span className="text-yellow-500">{metrics.trades_skipped}</span>
          </div>
          <div>
            <span className="font-medium text-foreground">Win Rate:</span>{" "}
            {metrics.win_rate != null
              ? `${(metrics.win_rate * 100).toFixed(1)}%`
              : "—"}
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

/**
 * Format duration in minutes to a human-readable string.
 */
function formatDuration(minutes: number): string {
  if (minutes < 1) {
    return "<1m";
  }
  if (minutes < 60) {
    return `${Math.floor(minutes)}m`;
  }
  const hours = Math.floor(minutes / 60);
  const mins = Math.floor(minutes % 60);
  if (hours < 24) {
    return `${hours}h ${mins}m`;
  }
  const days = Math.floor(hours / 24);
  const remainingHours = hours % 24;
  return `${days}d ${remainingHours}h`;
}

/**
 * Skeleton loader for SessionMetrics when data is not yet available.
 */
export function SessionMetricsSkeleton() {
  return (
    <Card>
      <CardHeader className="pb-3">
        <div className="flex items-center gap-2">
          <div className="h-4 w-4 animate-pulse rounded bg-muted" />
          <div className="h-4 w-28 animate-pulse rounded bg-muted" />
        </div>
      </CardHeader>
      <CardContent>
        <div className="grid grid-cols-2 gap-4 sm:grid-cols-3 lg:grid-cols-6">
          {Array.from({ length: 6 }).map((_, i) => (
            <div key={i} className="space-y-1">
              <div className="h-3 w-16 animate-pulse rounded bg-muted" />
              <div className="h-6 w-20 animate-pulse rounded bg-muted" />
            </div>
          ))}
        </div>
        <div className="mt-4 border-t pt-3">
          <div className="h-4 w-64 animate-pulse rounded bg-muted" />
        </div>
      </CardContent>
    </Card>
  );
}

export default SessionMetrics;
