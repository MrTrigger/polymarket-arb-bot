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
  ControlPanel,
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
    <div className="flex flex-col gap-4 p-4">
      {/* Alert banner section */}
      <section>
        <AnomalyAlerts />
      </section>

      {/* Top section: Metrics Cards */}
      <section>
        <MetricsCards />
      </section>

      {/* Middle section: 2-column layout for desktop */}
      <section className="grid grid-cols-1 gap-4 xl:grid-cols-12">
        {/* Left column: Markets Grid (main content) */}
        <div className="flex flex-col xl:col-span-8">
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
          <MarketsGrid />
        </div>

        {/* Right column: Control Panel, Account Info, Equity Curve, Session Metrics, Circuit Breaker stacked */}
        <div className="flex flex-col gap-4 xl:col-span-4">
          <ControlPanel />
          <AccountInfo />
          <EquityCurve />
          <SessionMetrics />
          <CircuitBreakerStatus />
        </div>
      </section>

      {/* Bottom section: Logs (collapsible) */}
      <section>
        <LogWindow />
      </section>
    </div>
  );
}

export default Dashboard;
