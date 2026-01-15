import { useDashboardState } from "@/hooks";
import {
  MetricsCards,
  EquityCurve,
  MarketsGrid,
  CircuitBreakerStatus,
} from "@/components/dashboard";

/**
 * Main dashboard page showing trading metrics, markets, and logs.
 * Components will be added in subsequent tasks.
 */
export function Dashboard() {
  const { initialized, arbOpportunities } = useDashboardState();

  if (!initialized) {
    return (
      <div className="flex h-[calc(100vh-73px)] items-center justify-center">
        <div className="text-center">
          <div className="mb-4 text-lg text-muted-foreground">
            Waiting for data...
          </div>
          <div className="text-sm text-muted-foreground">
            Connect to the bot to see live data
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="p-6">
      {/* Metrics Cards */}
      <div className="mb-6">
        <MetricsCards />
      </div>

      {/* Status indicators row */}
      <div className="mb-6 flex items-start gap-4">
        {/* Circuit Breaker Status Card */}
        <div className="w-64">
          <CircuitBreakerStatus />
        </div>

        {/* Arb opportunities indicator */}
        {arbOpportunities.length > 0 && (
          <div className="rounded-full bg-yellow-500/10 px-3 py-1 text-sm text-yellow-500">
            {arbOpportunities.length} Arb Opportunit
            {arbOpportunities.length === 1 ? "y" : "ies"}
          </div>
        )}
      </div>

      {/* Markets Grid */}
      <div className="mb-6">
        <MarketsGrid />
      </div>

      {/* Equity Curve and Logs */}
      <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
        <EquityCurve />
        <div className="rounded-lg border border-border bg-card p-4">
          <h3 className="mb-2 font-semibold">Recent Logs</h3>
          <div className="flex h-48 items-center justify-center text-muted-foreground">
            Log window component coming soon
          </div>
        </div>
      </div>
    </div>
  );
}

export default Dashboard;
