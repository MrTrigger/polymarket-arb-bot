import { useEffect, useRef, useMemo } from "react";
import {
  createChart,
  LineSeries,
  type IChartApi,
  type ISeriesApi,
  type LineData,
  ColorType,
  CrosshairMode,
  type Time,
} from "lightweight-charts";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useDashboardStore, selectEquityCurveHistory } from "@/lib/store";
import { parseDecimal } from "@/lib/types";
import { TrendingUp } from "lucide-react";

/**
 * EquityCurve component displays a line chart of cumulative P&L over time.
 *
 * Features:
 * - Dark theme styling to match dashboard
 * - Crosshair with value display
 * - Auto-scaling Y-axis
 * - Real-time updates from WebSocket snapshots
 * - Accumulates P&L history during the session
 *
 * The component maintains its own history of P&L values since lightweight-charts
 * requires the full data series for proper rendering.
 */
export function EquityCurve() {
  const chartContainerRef = useRef<HTMLDivElement>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<"Line"> | null>(null);

  // Get equity curve history from store (persists across remounts)
  const equityCurveHistory = useDashboardStore(selectEquityCurveHistory);
  const pnlUsdc = useDashboardStore((state) => state.snapshot?.metrics?.pnl_usdc);

  // Initialize chart
  useEffect(() => {
    if (!chartContainerRef.current) return;

    // Create chart with dark theme
    const chart = createChart(chartContainerRef.current, {
      layout: {
        background: { type: ColorType.Solid, color: "transparent" },
        textColor: "#a1a1aa", // zinc-400 (muted-foreground)
      },
      grid: {
        vertLines: { color: "#27272a" }, // zinc-800
        horzLines: { color: "#27272a" },
      },
      crosshair: {
        mode: CrosshairMode.Normal,
        vertLine: {
          color: "#52525b", // zinc-600
          width: 1,
          style: 2, // dashed
          labelBackgroundColor: "#27272a",
        },
        horzLine: {
          color: "#52525b",
          width: 1,
          style: 2,
          labelBackgroundColor: "#27272a",
        },
      },
      rightPriceScale: {
        borderColor: "#27272a",
        scaleMargins: {
          top: 0.1,
          bottom: 0.1,
        },
      },
      timeScale: {
        borderColor: "#27272a",
        timeVisible: true,
        secondsVisible: false,
      },
      handleScale: {
        axisPressedMouseMove: {
          time: true,
          price: true,
        },
      },
      handleScroll: {
        vertTouchDrag: true,
      },
    });

    // Create line series for P&L (v5 API: addSeries with LineSeries type)
    const lineSeries = chart.addSeries(LineSeries, {
      color: "#22c55e", // green-500 (will be updated based on value)
      lineWidth: 2,
      crosshairMarkerVisible: true,
      crosshairMarkerRadius: 4,
      priceFormat: {
        type: "custom",
        formatter: (price: number) => `$${price.toFixed(2)}`,
      },
    });

    chartRef.current = chart;
    seriesRef.current = lineSeries;

    // Handle resize
    const handleResize = () => {
      if (chartContainerRef.current && chartRef.current) {
        chartRef.current.applyOptions({
          width: chartContainerRef.current.clientWidth,
        });
      }
    };

    // Use ResizeObserver for responsive sizing
    const resizeObserver = new ResizeObserver(handleResize);
    resizeObserver.observe(chartContainerRef.current);

    // Initial size
    handleResize();

    return () => {
      resizeObserver.disconnect();
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
    };
  }, []);

  // Update chart when equity curve history changes
  useEffect(() => {
    if (!seriesRef.current || equityCurveHistory.length === 0) return;

    // Convert to lightweight-charts format
    const chartData = equityCurveHistory.map((point) => ({
      time: point.time as Time,
      value: point.value,
    }));

    // Update series with all data
    seriesRef.current.setData(chartData as LineData<Time>[]);

    // Update line color based on current P&L
    const lastValue = equityCurveHistory[equityCurveHistory.length - 1]?.value ?? 0;
    seriesRef.current.applyOptions({
      color: lastValue >= 0 ? "#22c55e" : "#ef4444", // green-500 or red-500
    });

    // Scroll to latest data
    chartRef.current?.timeScale().scrollToRealTime();
  }, [equityCurveHistory]);

  // Calculate current P&L for display
  const currentPnl = useMemo(() => {
    if (!pnlUsdc) return null;
    return parseDecimal(pnlUsdc);
  }, [pnlUsdc]);

  return (
    <Card className="h-full">
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <CardTitle className="text-sm font-medium">Equity Curve</CardTitle>
        <div className="flex items-center gap-2">
          {currentPnl !== null && (
            <span
              className={`text-sm font-semibold ${currentPnl >= 0 ? "text-green-500" : "text-red-500"}`}
            >
              ${currentPnl.toFixed(2)}
            </span>
          )}
          <TrendingUp className="h-4 w-4 text-muted-foreground" />
        </div>
      </CardHeader>
      <CardContent className="p-0 pb-4 pr-4">
        <div
          ref={chartContainerRef}
          className="h-80 w-full"
        />
      </CardContent>
    </Card>
  );
}

/**
 * Skeleton loader for EquityCurve when chart is initializing.
 */
export function EquityCurveSkeleton() {
  return (
    <Card className="h-full">
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <div className="h-4 w-24 animate-pulse rounded bg-muted" />
        <div className="h-4 w-4 animate-pulse rounded bg-muted" />
      </CardHeader>
      <CardContent className="p-0 pb-4 pr-4">
        <div className="flex h-80 w-full items-center justify-center">
          <div className="h-64 w-full animate-pulse rounded bg-muted/50" />
        </div>
      </CardContent>
    </Card>
  );
}

export default EquityCurve;
