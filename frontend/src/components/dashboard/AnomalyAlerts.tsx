import { useState, useCallback, useMemo } from "react";
import { useDashboardState } from "@/hooks";
import { Alert, AlertTitle, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  AlertTriangle,
  AlertOctagon,
  Info,
  X,
  ChevronDown,
  ChevronUp,
  History,
  Bell,
  BellOff,
} from "lucide-react";
import type { Anomaly, AnomalySeverity } from "@/lib/types";

/**
 * Configuration for each anomaly severity level.
 */
interface SeverityConfig {
  icon: React.ComponentType<{ className?: string }>;
  bgColor: string;
  borderColor: string;
  textColor: string;
  badgeColor: string;
  label: string;
}

const SEVERITY_CONFIG: Record<AnomalySeverity, SeverityConfig> = {
  low: {
    icon: Info,
    bgColor: "bg-blue-500/10",
    borderColor: "border-blue-500/30",
    textColor: "text-blue-400",
    badgeColor: "bg-blue-500/20 text-blue-400",
    label: "Low",
  },
  medium: {
    icon: AlertTriangle,
    bgColor: "bg-yellow-500/10",
    borderColor: "border-yellow-500/30",
    textColor: "text-yellow-400",
    badgeColor: "bg-yellow-500/20 text-yellow-400",
    label: "Medium",
  },
  high: {
    icon: AlertTriangle,
    bgColor: "bg-orange-500/10",
    borderColor: "border-orange-500/30",
    textColor: "text-orange-400",
    badgeColor: "bg-orange-500/20 text-orange-400",
    label: "High",
  },
  critical: {
    icon: AlertOctagon,
    bgColor: "bg-red-500/10",
    borderColor: "border-red-500/30",
    textColor: "text-red-400",
    badgeColor: "bg-red-500/20 text-red-400",
    label: "Critical",
  },
};

/**
 * Format timestamp for display (HH:MM:SS).
 */
function formatTime(iso: string): string {
  const date = new Date(iso);
  return date.toLocaleTimeString("en-US", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

/**
 * Format anomaly type for display.
 * Converts snake_case to Title Case.
 */
function formatAnomalyType(type: string): string {
  return type
    .split("_")
    .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
    .join(" ");
}

/**
 * Individual anomaly alert item.
 */
interface AnomalyItemProps {
  anomaly: Anomaly;
  onDismiss: (id: string) => void;
}

function AnomalyItem({ anomaly, onDismiss }: AnomalyItemProps) {
  const config = SEVERITY_CONFIG[anomaly.severity];
  const Icon = config.icon;

  return (
    <Alert
      className={`${config.bgColor} ${config.borderColor} mb-2 last:mb-0`}
    >
      <Icon className={`h-4 w-4 ${config.textColor}`} />
      <AlertTitle className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <span className={config.textColor}>
            {formatAnomalyType(anomaly.anomaly_type)}
          </span>
          <Badge variant="outline" className={config.badgeColor}>
            {config.label}
          </Badge>
          <span className="text-xs text-muted-foreground">
            {formatTime(anomaly.detected_at)}
          </span>
        </div>
        {!anomaly.dismissed && (
          <Button
            variant="ghost"
            size="icon"
            className="h-6 w-6 hover:bg-zinc-800"
            onClick={() => onDismiss(anomaly.id)}
          >
            <X className="h-4 w-4" />
            <span className="sr-only">Dismiss</span>
          </Button>
        )}
      </AlertTitle>
      <AlertDescription>
        <p className={anomaly.dismissed ? "text-muted-foreground" : ""}>
          {anomaly.message}
        </p>
        {anomaly.event_id && (
          <span className="text-xs text-muted-foreground">
            Event: {anomaly.event_id.substring(0, 8)}...
          </span>
        )}
      </AlertDescription>
    </Alert>
  );
}

/**
 * AnomalyAlerts component - shows anomaly notifications with dismiss functionality.
 *
 * Features:
 * - Displays active (non-dismissed) anomalies as alert banners
 * - Color-coded by severity (low=blue, medium=yellow, high=orange, critical=red)
 * - Dismissable alerts (local state, since server doesn't track dismiss)
 * - Collapsible alert history showing all anomalies
 * - Critical anomalies highlighted with prominent styling
 * - Count badges for active alerts by severity
 */
export function AnomalyAlerts() {
  const { anomalies } = useDashboardState();

  // Local state for dismissed anomaly IDs (server doesn't persist dismiss state)
  const [dismissedIds, setDismissedIds] = useState<Set<string>>(new Set());
  const [historyOpen, setHistoryOpen] = useState(false);

  // Filter anomalies based on local dismiss state
  const { activeAlerts, dismissedAlerts, criticalCount, highCount } =
    useMemo(() => {
      const active: Anomaly[] = [];
      const dismissed: Anomaly[] = [];
      let critical = 0;
      let high = 0;

      for (const anomaly of anomalies) {
        const isLocallyDismissed = dismissedIds.has(anomaly.id);
        const isDismissed = anomaly.dismissed || isLocallyDismissed;

        if (isDismissed) {
          dismissed.push(anomaly);
        } else {
          active.push(anomaly);
          if (anomaly.severity === "critical") critical++;
          if (anomaly.severity === "high") high++;
        }
      }

      // Sort by severity (critical first) then by time (newest first)
      const severityOrder: Record<AnomalySeverity, number> = {
        critical: 0,
        high: 1,
        medium: 2,
        low: 3,
      };

      active.sort((a, b) => {
        const severityDiff = severityOrder[a.severity] - severityOrder[b.severity];
        if (severityDiff !== 0) return severityDiff;
        return new Date(b.detected_at).getTime() - new Date(a.detected_at).getTime();
      });

      dismissed.sort(
        (a, b) =>
          new Date(b.detected_at).getTime() - new Date(a.detected_at).getTime()
      );

      return { activeAlerts: active, dismissedAlerts: dismissed, criticalCount: critical, highCount: high };
    }, [anomalies, dismissedIds]);

  // Dismiss handler
  const handleDismiss = useCallback((id: string) => {
    setDismissedIds((prev) => new Set([...prev, id]));
  }, []);

  // Dismiss all active alerts
  const handleDismissAll = useCallback(() => {
    setDismissedIds((prev) => {
      const newSet = new Set(prev);
      for (const alert of activeAlerts) {
        newSet.add(alert.id);
      }
      return newSet;
    });
  }, [activeAlerts]);

  // Don't render anything if no anomalies
  if (anomalies.length === 0) {
    return null;
  }

  return (
    <div className="space-y-2">
      {/* Active alerts banner */}
      {activeAlerts.length > 0 && (
        <div className="space-y-2">
          {/* Header with counts and dismiss all */}
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Bell className="h-4 w-4 text-yellow-500" />
              <span className="text-sm font-medium">
                {activeAlerts.length} Active Alert
                {activeAlerts.length !== 1 ? "s" : ""}
              </span>
              {criticalCount > 0 && (
                <Badge
                  variant="outline"
                  className="bg-red-500/20 text-red-400"
                >
                  {criticalCount} Critical
                </Badge>
              )}
              {highCount > 0 && (
                <Badge
                  variant="outline"
                  className="bg-orange-500/20 text-orange-400"
                >
                  {highCount} High
                </Badge>
              )}
            </div>
            <Button
              variant="ghost"
              size="sm"
              className="h-7 text-xs"
              onClick={handleDismissAll}
            >
              <BellOff className="mr-1 h-3 w-3" />
              Dismiss All
            </Button>
          </div>

          {/* Active alert items */}
          <div className="space-y-2">
            {activeAlerts.map((anomaly) => (
              <AnomalyItem
                key={anomaly.id}
                anomaly={anomaly}
                onDismiss={handleDismiss}
              />
            ))}
          </div>
        </div>
      )}

      {/* No active alerts message when all dismissed */}
      {activeAlerts.length === 0 && dismissedAlerts.length > 0 && (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <BellOff className="h-4 w-4" />
          <span>No active alerts</span>
        </div>
      )}

      {/* Alert history (collapsible) */}
      {dismissedAlerts.length > 0 && (
        <Collapsible open={historyOpen} onOpenChange={setHistoryOpen}>
          <CollapsibleTrigger asChild>
            <Button
              variant="ghost"
              size="sm"
              className="h-7 w-full justify-between text-xs text-muted-foreground"
            >
              <div className="flex items-center gap-1">
                <History className="h-3 w-3" />
                Alert History ({dismissedAlerts.length})
              </div>
              {historyOpen ? (
                <ChevronUp className="h-3 w-3" />
              ) : (
                <ChevronDown className="h-3 w-3" />
              )}
            </Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="mt-2 space-y-2">
            {dismissedAlerts.map((anomaly) => (
              <AnomalyItem
                key={anomaly.id}
                anomaly={{ ...anomaly, dismissed: true }}
                onDismiss={handleDismiss}
              />
            ))}
          </CollapsibleContent>
        </Collapsible>
      )}
    </div>
  );
}

/**
 * Skeleton loader for AnomalyAlerts.
 */
export function AnomalyAlertsSkeleton() {
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2">
        <div className="h-4 w-4 animate-pulse rounded bg-zinc-800" />
        <div className="h-4 w-24 animate-pulse rounded bg-zinc-800" />
      </div>
      <div className="rounded-lg border border-zinc-800 bg-zinc-900 p-4">
        <div className="flex items-center gap-2">
          <div className="h-4 w-4 animate-pulse rounded bg-zinc-800" />
          <div className="h-4 w-32 animate-pulse rounded bg-zinc-800" />
        </div>
        <div className="mt-2 h-4 w-full animate-pulse rounded bg-zinc-800" />
      </div>
    </div>
  );
}

export default AnomalyAlerts;
