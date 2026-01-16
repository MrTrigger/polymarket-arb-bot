import { useDashboardState } from "@/hooks";
import {
  MetricsCards,
  EquityCurve,
  MarketsGrid,
  CircuitBreakerStatus,
  LogWindow,
  SessionMetrics,
  AnomalyAlerts,
  AccountInfo,
} from "@/components/dashboard";
import { TrendingUp } from "lucide-react";

/**
 * Main dashboard page - desktop-optimized grid layout.
 * Shows trading metrics, equity curve, markets, circuit breaker, and logs.
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
    <div className="flex h-[calc(100vh-73px)] flex-col gap-4 overflow-hidden p-4">
      {/* Alert banner section */}
      <section className="shrink-0">
        <AnomalyAlerts />
      </section>

      {/* Top section: Metrics Cards */}
      <section className="shrink-0">
        <MetricsCards />
      </section>

      {/* Middle section: 3-column layout for desktop */}
      <section className="grid min-h-0 flex-1 grid-cols-1 gap-4 xl:grid-cols-12">
        {/* Left column: Markets Grid (main content) */}
        <div className="flex min-h-0 flex-col xl:col-span-8">
          <div className="mb-2 flex items-center justify-between">
            <h2 className="text-sm font-medium text-muted-foreground">
              Active Markets
            </h2>
            {arbOpportunities.length > 0 && (
              <div className="flex items-center gap-1 rounded-full bg-yellow-500/10 px-2 py-0.5 text-xs text-yellow-500">
                <TrendingUp className="h-3 w-3" />
                {arbOpportunities.length} Arb
                {arbOpportunities.length === 1 ? "" : "s"}
              </div>
            )}
          </div>
          <div className="min-h-0 flex-1 overflow-auto">
            <MarketsGrid />
          </div>
        </div>

        {/* Right column: Account Info, Equity Curve, Session Metrics, Circuit Breaker stacked */}
        <div className="flex min-h-0 flex-col gap-4 overflow-auto xl:col-span-4">
          <div className="shrink-0">
            <AccountInfo />
          </div>
          <div className="shrink-0">
            <EquityCurve />
          </div>
          <div className="shrink-0">
            <SessionMetrics />
          </div>
          <div className="shrink-0">
            <CircuitBreakerStatus />
          </div>
        </div>
      </section>

      {/* Bottom section: Logs (collapsible) */}
      <section className="shrink-0">
        <LogWindow />
      </section>
    </div>
  );
}

export default Dashboard;
