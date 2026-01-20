import { useState, useCallback } from "react";
import { useDashboardState } from "@/hooks";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Play,
  Pause,
  Square,
  RotateCcw,
  X,
  Loader2,
  AlertTriangle,
  CheckCircle2,
  Circle,
  Zap,
} from "lucide-react";
import type { BotModeValue, BotStatusValue } from "@/lib/types";
import {
  pauseTrading,
  resumeTrading,
  stopBot,
  resetCircuitBreaker,
  cancelAllOrders,
  switchMode,
} from "@/lib/api";

/**
 * Status badge configuration.
 */
const STATUS_CONFIG: Record<
  BotStatusValue,
  { label: string; color: string; icon: typeof Circle }
> = {
  initializing: {
    label: "Initializing",
    color: "bg-yellow-500/10 text-yellow-500",
    icon: Loader2,
  },
  setting_allowances: {
    label: "Setting Allowances",
    color: "bg-yellow-500/10 text-yellow-500",
    icon: Loader2,
  },
  connecting: {
    label: "Connecting",
    color: "bg-yellow-500/10 text-yellow-500",
    icon: Loader2,
  },
  ready: {
    label: "Ready",
    color: "bg-blue-500/10 text-blue-500",
    icon: CheckCircle2,
  },
  trading: {
    label: "Trading",
    color: "bg-green-500/10 text-green-500",
    icon: Zap,
  },
  paused: {
    label: "Paused",
    color: "bg-orange-500/10 text-orange-500",
    icon: Pause,
  },
  circuit_breaker: {
    label: "Circuit Breaker",
    color: "bg-red-500/10 text-red-500",
    icon: AlertTriangle,
  },
  shutting_down: {
    label: "Shutting Down",
    color: "bg-gray-500/10 text-gray-500",
    icon: Square,
  },
  error: {
    label: "Error",
    color: "bg-red-500/10 text-red-500",
    icon: AlertTriangle,
  },
};

/**
 * Control panel for managing the trading bot.
 *
 * Features:
 * - Start/Stop/Pause trading
 * - Mode switching (Live/Paper)
 * - Clear orders, reset circuit breaker
 * - Status display with visual indicators
 */
export function ControlPanel() {
  const { controlState, initialized } = useDashboardState();
  const [isLoading, setIsLoading] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showStopConfirm, setShowStopConfirm] = useState(false);

  // Get current status with fallbacks for undefined fields
  const botStatus = controlState?.bot_status ?? "initializing";
  const currentMode = controlState?.current_mode ?? "paper";
  const allowanceStatus = controlState?.allowance_status ?? "unknown";
  const pendingOrders = controlState?.pending_orders_count ?? 0;
  const errorMessage = controlState?.error_message ?? null;
  const tradingEnabled = controlState?.trading_enabled ?? false;
  const circuitBreakerTripped = controlState?.circuit_breaker_tripped ?? false;

  const statusConfig = STATUS_CONFIG[botStatus] ?? STATUS_CONFIG.initializing;
  const StatusIcon = statusConfig.icon;

  // Handle API calls with loading state
  const handleAction = useCallback(
    async (
      action: () => Promise<{ success: boolean; message: string }>,
      actionName: string
    ) => {
      setIsLoading(actionName);
      setError(null);
      try {
        const result = await action();
        if (!result.success) {
          setError(result.message);
        }
      } catch (err) {
        setError(err instanceof Error ? err.message : "Action failed");
      } finally {
        setIsLoading(null);
      }
    },
    []
  );

  const handlePause = useCallback(
    () => handleAction(pauseTrading, "pause"),
    [handleAction]
  );

  const handleResume = useCallback(
    () => handleAction(resumeTrading, "resume"),
    [handleAction]
  );

  const handleStop = useCallback(() => {
    setShowStopConfirm(false);
    handleAction(stopBot, "stop");
  }, [handleAction]);

  const handleResetCB = useCallback(
    () => handleAction(resetCircuitBreaker, "reset"),
    [handleAction]
  );

  const handleCancelOrders = useCallback(
    () => handleAction(cancelAllOrders, "cancel"),
    [handleAction]
  );

  const handleModeChange = useCallback(
    (mode: BotModeValue) => {
      handleAction(() => switchMode(mode), "mode");
    },
    [handleAction]
  );

  if (!initialized) {
    return <ControlPanelSkeleton />;
  }

  return (
    <Card className="border">
      <CardHeader className="pb-3">
        <CardTitle className="flex items-center justify-between text-sm font-medium">
          <span>Bot Control</span>
          {/* Status badge */}
          <Badge
            variant="outline"
            className={`flex items-center gap-1.5 ${statusConfig.color}`}
          >
            <StatusIcon
              className={`h-3 w-3 ${
                botStatus === "initializing" ||
                botStatus === "connecting" ||
                botStatus === "setting_allowances"
                  ? "animate-spin"
                  : ""
              }`}
            />
            {statusConfig.label}
          </Badge>
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        {/* Error display */}
        {(error || errorMessage) && (
          <div className="rounded-md bg-red-500/10 p-2 text-xs text-red-500">
            {error || errorMessage}
          </div>
        )}

        {/* Stop confirmation dialog */}
        {showStopConfirm && (
          <div className="rounded-md border border-red-500/50 bg-red-500/10 p-3 space-y-2">
            <div className="text-sm font-medium text-red-500">Stop Bot?</div>
            <p className="text-xs text-muted-foreground">
              This will initiate a graceful shutdown. The bot will cancel pending orders and stop trading.
            </p>
            <div className="flex gap-2">
              <Button
                variant="outline"
                size="sm"
                onClick={() => setShowStopConfirm(false)}
                className="flex-1"
              >
                Cancel
              </Button>
              <Button
                variant="destructive"
                size="sm"
                onClick={handleStop}
                disabled={isLoading !== null}
                className="flex-1"
              >
                {isLoading === "stop" ? (
                  <Loader2 className="mr-1.5 h-3 w-3 animate-spin" />
                ) : null}
                Stop
              </Button>
            </div>
          </div>
        )}

        {/* Mode selector */}
        <div className="space-y-2">
          <label className="text-xs text-muted-foreground">Mode</label>
          <div className="flex gap-2">
            <Button
              variant={currentMode === "paper" ? "default" : "outline"}
              size="sm"
              onClick={() => handleModeChange("paper")}
              disabled={isLoading !== null || tradingEnabled}
              className="flex-1"
            >
              Paper
            </Button>
            <Button
              variant={currentMode === "live" ? "default" : "outline"}
              size="sm"
              onClick={() => handleModeChange("live")}
              disabled={isLoading !== null || tradingEnabled}
              className="flex-1"
            >
              Live
            </Button>
          </div>
          {currentMode === "live" && (
            <div className="text-xs">
              <span className="text-muted-foreground">Allowance: </span>
              <span
                className={
                  allowanceStatus === "sufficient"
                    ? "text-green-500"
                    : allowanceStatus === "failed"
                    ? "text-red-500"
                    : "text-yellow-500"
                }
              >
                {allowanceStatus}
              </span>
            </div>
          )}
        </div>

        {/* Control buttons */}
        <div className="flex flex-wrap gap-2">
          {/* Resume/Pause button */}
          {tradingEnabled ? (
            <Button
              variant="outline"
              size="sm"
              onClick={handlePause}
              disabled={isLoading !== null}
              className="flex-1"
            >
              {isLoading === "pause" ? (
                <Loader2 className="mr-1.5 h-3 w-3 animate-spin" />
              ) : (
                <Pause className="mr-1.5 h-3 w-3" />
              )}
              Pause
            </Button>
          ) : (
            <Button
              variant="outline"
              size="sm"
              onClick={handleResume}
              disabled={isLoading !== null || circuitBreakerTripped}
              className="flex-1"
            >
              {isLoading === "resume" ? (
                <Loader2 className="mr-1.5 h-3 w-3 animate-spin" />
              ) : (
                <Play className="mr-1.5 h-3 w-3" />
              )}
              Resume
            </Button>
          )}

          {/* Reset circuit breaker */}
          <Button
            variant="outline"
            size="sm"
            onClick={handleResetCB}
            disabled={isLoading !== null || !circuitBreakerTripped}
            className="flex-1"
          >
            {isLoading === "reset" ? (
              <Loader2 className="mr-1.5 h-3 w-3 animate-spin" />
            ) : (
              <RotateCcw className="mr-1.5 h-3 w-3" />
            )}
            Reset CB
          </Button>
        </div>

        {/* Secondary actions */}
        <div className="flex flex-wrap gap-2">
          {/* Cancel orders */}
          <Button
            variant="ghost"
            size="sm"
            onClick={handleCancelOrders}
            disabled={isLoading !== null || pendingOrders === 0}
            className="flex-1 text-xs"
          >
            {isLoading === "cancel" ? (
              <Loader2 className="mr-1.5 h-3 w-3 animate-spin" />
            ) : (
              <X className="mr-1.5 h-3 w-3" />
            )}
            Cancel Orders ({pendingOrders})
          </Button>

          {/* Stop bot */}
          <Button
            variant="ghost"
            size="sm"
            onClick={() => setShowStopConfirm(true)}
            disabled={isLoading !== null || showStopConfirm}
            className="flex-1 text-xs text-red-500 hover:text-red-500"
          >
            <Square className="mr-1.5 h-3 w-3" />
            Stop Bot
          </Button>
        </div>

        {/* Quick stats */}
        <div className="flex items-center justify-between border-t pt-3 text-xs text-muted-foreground">
          <span>
            Failures:{" "}
            <span
              className={
                (controlState?.consecutive_failures ?? 0) > 0
                  ? "text-yellow-500"
                  : ""
              }
            >
              {controlState?.consecutive_failures ?? 0}
            </span>
          </span>
          {controlState?.circuit_breaker_trip_time && (
            <span>
              Tripped:{" "}
              {new Date(
                controlState.circuit_breaker_trip_time
              ).toLocaleTimeString()}
            </span>
          )}
        </div>
      </CardContent>
    </Card>
  );
}

/**
 * Skeleton loader for ControlPanel.
 */
export function ControlPanelSkeleton() {
  return (
    <Card className="border">
      <CardHeader className="pb-3">
        <CardTitle className="flex items-center justify-between text-sm font-medium">
          <span>Bot Control</span>
          <div className="h-5 w-20 animate-pulse rounded bg-muted" />
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex gap-3">
          <div className="h-8 flex-1 animate-pulse rounded bg-muted" />
        </div>
        <div className="flex gap-2">
          <div className="h-8 flex-1 animate-pulse rounded bg-muted" />
          <div className="h-8 flex-1 animate-pulse rounded bg-muted" />
        </div>
        <div className="flex gap-2">
          <div className="h-8 flex-1 animate-pulse rounded bg-muted" />
          <div className="h-8 flex-1 animate-pulse rounded bg-muted" />
        </div>
      </CardContent>
    </Card>
  );
}

export default ControlPanel;
