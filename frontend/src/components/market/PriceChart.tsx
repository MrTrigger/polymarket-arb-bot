import { useEffect, useRef, useMemo } from "react";
import {
  createChart,
  createSeriesMarkers,
  LineSeries,
  type IChartApi,
  type ISeriesApi,
  type LineData,
  ColorType,
  CrosshairMode,
  type Time,
  type SeriesMarker,
  type ISeriesMarkersPluginApi,
} from "lightweight-charts";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import type { ActiveMarket, Trade } from "@/lib/types";
import { parseDecimal } from "@/lib/types";
import { TrendingUp } from "lucide-react";

interface PriceChartProps {
  /** The market to display price data for. */
  market: ActiveMarket;
  /** Trades to display as markers on the chart. */
  trades: Trade[];
}

/**
 * Data point for the price chart.
 */
interface PriceDataPoint {
  time: Time;
  value: number;
}

/**
 * PriceChart component displays spot price history with trade markers.
 *
 * Features:
 * - Dark theme styling to match dashboard
 * - Strike price as horizontal reference line
 * - Trade markers: green up triangles for BUY, red down triangles for SELL
 * - YES trades have solid markers, NO trades have hollow markers
 * - Crosshair with value display
 * - Auto-scaling Y-axis
 * - Real-time updates from WebSocket snapshots
 */
export function PriceChart({ market, trades }: PriceChartProps) {
  const chartContainerRef = useRef<HTMLDivElement>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<ISeriesApi<"Line"> | null>(null);
  const markersRef = useRef<ISeriesMarkersPluginApi<Time> | null>(null);
  const dataRef = useRef<PriceDataPoint[]>([]);
  const lastSpotPriceRef = useRef<string | null>(null);

  // Parse spot and strike prices
  const spotPrice = useMemo(() => parseDecimal(market.spot_price), [market.spot_price]);
  const strikePrice = useMemo(() => parseDecimal(market.strike_price), [market.strike_price]);

  // Initialize chart
  useEffect(() => {
    if (!chartContainerRef.current) return;

    // Create chart with dark theme
    const chart = createChart(chartContainerRef.current, {
      layout: {
        background: { type: ColorType.Solid, color: "transparent" },
        textColor: "#a1a1aa", // zinc-400
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
        secondsVisible: true,
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

    // Create line series for price
    const lineSeries = chart.addSeries(LineSeries, {
      color: "#3b82f6", // blue-500
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

    // Create markers primitive (v5 API)
    markersRef.current = createSeriesMarkers(lineSeries, []);

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
      markersRef.current = null;
    };
  }, []);

  // Update strike price line when market changes
  useEffect(() => {
    if (!seriesRef.current) return;

    // Remove any existing price lines
    const existingLines = seriesRef.current.priceLines();
    existingLines.forEach((line) => seriesRef.current?.removePriceLine(line));

    // Add strike price as horizontal line
    seriesRef.current.createPriceLine({
      price: strikePrice,
      color: "#f59e0b", // amber-500
      lineWidth: 2,
      lineStyle: 2, // dashed
      axisLabelVisible: true,
      title: "Strike",
    });
  }, [strikePrice]);

  // Update spot price data when it changes
  useEffect(() => {
    if (!seriesRef.current || market.spot_price === lastSpotPriceRef.current) return;

    lastSpotPriceRef.current = market.spot_price;
    const timeValue = Math.floor(Date.now() / 1000) as Time;

    // Check if we already have a point at this timestamp
    const lastPoint = dataRef.current[dataRef.current.length - 1];

    if (lastPoint && lastPoint.time === timeValue) {
      // Update existing point
      lastPoint.value = spotPrice;
    } else {
      // Add new point
      dataRef.current.push({ time: timeValue, value: spotPrice });
    }

    // Keep only last 500 points
    if (dataRef.current.length > 500) {
      dataRef.current = dataRef.current.slice(-500);
    }

    // Update series with all data
    seriesRef.current.setData(dataRef.current as LineData<Time>[]);

    // Scroll to latest
    chartRef.current?.timeScale().scrollToRealTime();
  }, [market.spot_price, spotPrice]);

  // Update trade markers
  useEffect(() => {
    if (!markersRef.current) return;

    if (trades.length === 0) {
      markersRef.current.setMarkers([]);
      return;
    }

    // Convert trades to markers
    const markers: SeriesMarker<Time>[] = trades.map((trade) => {
      const isBuy = trade.side === "BUY";
      const isYes = trade.outcome === "yes";
      const timeValue = Math.floor(new Date(trade.fill_time).getTime() / 1000) as Time;

      // Use different shapes for YES vs NO
      // YES: circle/arrowUp, NO: square/arrowDown base shape
      // BUY: up arrow (belowBar), SELL: down arrow (aboveBar)
      const position = isBuy ? "belowBar" : "aboveBar";
      const shape = isBuy ? "arrowUp" : "arrowDown";
      const color = isBuy ? "#22c55e" : "#ef4444"; // green-500 or red-500

      return {
        time: timeValue,
        position: position as "belowBar" | "aboveBar",
        shape: shape as "arrowUp" | "arrowDown",
        color: color,
        size: isYes ? 2 : 1, // YES trades are larger
        text: `${trade.side} ${trade.outcome.toUpperCase()}`,
      };
    });

    // Sort markers by time
    markers.sort((a, b) => (a.time as number) - (b.time as number));

    // Set markers using v5 API
    markersRef.current.setMarkers(markers);
  }, [trades]);

  // Calculate price relative to strike
  const priceVsStrike = useMemo(() => {
    const diff = spotPrice - strikePrice;
    const pctDiff = ((diff / strikePrice) * 100).toFixed(2);
    return {
      diff,
      pctDiff,
      label: diff >= 0 ? `+${pctDiff}%` : `${pctDiff}%`,
      color: diff >= 0 ? "text-green-500" : "text-red-500",
    };
  }, [spotPrice, strikePrice]);

  return (
    <Card className="h-full">
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <CardTitle className="text-sm font-medium">
          {market.asset} Price
        </CardTitle>
        <div className="flex items-center gap-3">
          <span className="text-sm text-muted-foreground">
            Spot: <span className="font-semibold text-foreground">${spotPrice.toFixed(2)}</span>
          </span>
          <span className="text-sm text-muted-foreground">
            Strike: <span className="font-semibold text-amber-500">${strikePrice.toFixed(2)}</span>
          </span>
          <span className={`text-sm font-semibold ${priceVsStrike.color}`}>
            {priceVsStrike.label}
          </span>
          <TrendingUp className="h-4 w-4 text-muted-foreground" />
        </div>
      </CardHeader>
      <CardContent className="p-0 pb-4 pr-4">
        <div
          ref={chartContainerRef}
          className="h-64 w-full"
        />
        {/* Legend */}
        <div className="mt-2 flex items-center justify-center gap-4 text-xs text-muted-foreground">
          <div className="flex items-center gap-1">
            <div className="h-0 w-0 border-b-4 border-l-4 border-r-4 border-b-green-500 border-l-transparent border-r-transparent" />
            <span>Buy</span>
          </div>
          <div className="flex items-center gap-1">
            <div className="h-0 w-0 border-l-4 border-r-4 border-t-4 border-l-transparent border-r-transparent border-t-red-500" />
            <span>Sell</span>
          </div>
          <div className="flex items-center gap-1">
            <div className="h-2 w-2 rounded-full bg-amber-500" />
            <span>Strike</span>
          </div>
          <div className="flex items-center gap-1">
            <div className="h-0.5 w-4 bg-blue-500" />
            <span>Spot</span>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

/**
 * Skeleton loader for PriceChart when data is loading.
 */
export function PriceChartSkeleton() {
  return (
    <Card className="h-full">
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <div className="h-4 w-24 animate-pulse rounded bg-muted" />
        <div className="flex items-center gap-3">
          <div className="h-4 w-20 animate-pulse rounded bg-muted" />
          <div className="h-4 w-20 animate-pulse rounded bg-muted" />
          <div className="h-4 w-4 animate-pulse rounded bg-muted" />
        </div>
      </CardHeader>
      <CardContent className="p-0 pb-4 pr-4">
        <div className="flex h-64 w-full items-center justify-center">
          <div className="h-48 w-full animate-pulse rounded bg-muted/50" />
        </div>
      </CardContent>
    </Card>
  );
}

export default PriceChart;
