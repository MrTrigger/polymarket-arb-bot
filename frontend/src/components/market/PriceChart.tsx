import { useEffect, useRef, useMemo, useState } from "react";
import {
  createChart,
  createSeriesMarkers,
  LineSeries,
  AreaSeries,
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
import { TrendingUp, Loader2, Activity } from "lucide-react";

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
 * Data point for the confidence indicator.
 */
interface ConfidenceDataPoint {
  time: Time;
  value: number;
}

/** API response for historical spot prices. */
interface HistoricalPrice {
  price: number;
  timestamp: number; // milliseconds from API
}

/**
 * Fetch historical spot prices from the API.
 * @param asset - Asset symbol (BTC, ETH, SOL)
 * @param minutes - Number of minutes of history (default: 60, max: 480)
 */
async function fetchHistoricalPrices(asset: string, minutes: number = 60): Promise<PriceDataPoint[]> {
  try {
    const response = await fetch(`/api/prices/${asset}?minutes=${minutes}`);
    if (!response.ok) {
      console.error("Failed to fetch historical prices:", response.statusText);
      return [];
    }
    const data: HistoricalPrice[] = await response.json();
    // Timestamp is in milliseconds from API, convert to seconds for chart
    return data.map((p) => ({
      time: Math.floor(p.timestamp / 1000) as Time,
      value: p.price,
    }));
  } catch (error) {
    console.error("Error fetching historical prices:", error);
    return [];
  }
}

/**
 * Shared chart options for consistent styling.
 */
const getChartOptions = (height: number) => ({
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
    fixRightEdge: true, // Keep right edge fixed during zoom - only left side moves
    rightOffset: 10, // Add margin between last data point and Y-axis
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
  height,
});

/**
 * PriceChart component displays spot price history with trade markers
 * and a confidence indicator subgraph.
 *
 * Features:
 * - Main chart: Spot price with strike price reference line
 * - Indicator subgraph: Confidence level and threshold line
 * - Trade markers: green up triangles for BUY, red down triangles for SELL
 * - Real-time updates from WebSocket snapshots
 */
export function PriceChart({ market, trades }: PriceChartProps) {
  // Price chart refs
  const priceChartContainerRef = useRef<HTMLDivElement>(null);
  const priceChartRef = useRef<IChartApi | null>(null);
  const priceSeriesRef = useRef<ISeriesApi<"Line"> | null>(null);
  const markersRef = useRef<ISeriesMarkersPluginApi<Time> | null>(null);
  const priceDataRef = useRef<PriceDataPoint[]>([]);
  const lastSpotPriceRef = useRef<string | null>(null);
  const historicalLoadedRef = useRef<boolean>(false);
  const loadedMinutesRef = useRef<number>(0);
  const isLoadingMoreRef = useRef<boolean>(false);

  // Confidence chart refs
  const confChartContainerRef = useRef<HTMLDivElement>(null);
  const confChartRef = useRef<IChartApi | null>(null);
  const confSeriesRef = useRef<ISeriesApi<"Area"> | null>(null);
  const thresholdSeriesRef = useRef<ISeriesApi<"Line"> | null>(null);
  const confDataRef = useRef<ConfidenceDataPoint[]>([]);
  const thresholdDataRef = useRef<ConfidenceDataPoint[]>([]);

  const [loadingHistory, setLoadingHistory] = useState(true);

  // Parse spot and strike prices
  const spotPrice = useMemo(() => parseDecimal(market.spot_price), [market.spot_price]);
  const strikePrice = useMemo(() => parseDecimal(market.strike_price), [market.strike_price]);

  // Parse confidence data
  const confidenceData = useMemo(() => {
    if (!market.confidence) return null;
    return {
      confidence: parseDecimal(market.confidence.confidence),
      timeConfidence: parseDecimal(market.confidence.time_confidence),
      distanceConfidence: parseDecimal(market.confidence.distance_confidence),
      threshold: parseDecimal(market.confidence.threshold),
      ev: parseDecimal(market.confidence.ev),
      wouldTrade: market.confidence.would_trade,
      distanceDollars: parseDecimal(market.confidence.distance_dollars),
      atrMultiple: parseDecimal(market.confidence.atr_multiple),
      favorablePrice: parseDecimal(market.confidence.favorable_price),
    };
  }, [market.confidence]);

  // Initialize price chart
  useEffect(() => {
    if (!priceChartContainerRef.current) return;

    const chart = createChart(priceChartContainerRef.current, getChartOptions(280) as Parameters<typeof createChart>[1]);

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

    priceChartRef.current = chart;
    priceSeriesRef.current = lineSeries;
    markersRef.current = createSeriesMarkers(lineSeries, []);

    const handleResize = () => {
      if (priceChartContainerRef.current && priceChartRef.current) {
        priceChartRef.current.applyOptions({
          width: priceChartContainerRef.current.clientWidth,
        });
      }
    };

    const resizeObserver = new ResizeObserver(handleResize);
    resizeObserver.observe(priceChartContainerRef.current);
    handleResize();

    return () => {
      resizeObserver.disconnect();
      chart.remove();
      priceChartRef.current = null;
      priceSeriesRef.current = null;
      markersRef.current = null;
    };
  }, []);

  // Initialize confidence chart
  useEffect(() => {
    if (!confChartContainerRef.current) return;

    const chart = createChart(confChartContainerRef.current, {
      ...getChartOptions(180),
      rightPriceScale: {
        borderColor: "#27272a",
        scaleMargins: { top: 0.05, bottom: 0.05 },
      },
      // Hide time scale on indicator chart (shared with price chart above)
      timeScale: {
        visible: false,
        borderColor: "#27272a",
      },
      // Disable independent scrolling/scaling - this chart follows the price chart
      handleScroll: false,
      handleScale: false,
    } as Parameters<typeof createChart>[1]);

    // Confidence area series (filled)
    const confSeries = chart.addSeries(AreaSeries, {
      lineColor: "#22c55e", // green-500
      topColor: "rgba(34, 197, 94, 0.4)",
      bottomColor: "rgba(34, 197, 94, 0.0)",
      lineWidth: 2,
      lastValueVisible: false, // Hide the horizontal "last value" line
      priceLineVisible: false, // Hide price line extension to Y-axis
      priceFormat: {
        type: "custom",
        formatter: (value: number) => `${(value * 100).toFixed(0)}%`,
      },
    });

    // Threshold line series
    const thresholdSeries = chart.addSeries(LineSeries, {
      color: "#f59e0b", // amber-500
      lineWidth: 2,
      lastValueVisible: false, // Hide the horizontal "last value" line
      priceLineVisible: false, // Hide price line extension to Y-axis
      priceFormat: {
        type: "custom",
        formatter: (value: number) => `${(value * 100).toFixed(0)}%`,
      },
    });

    confChartRef.current = chart;
    confSeriesRef.current = confSeries;
    thresholdSeriesRef.current = thresholdSeries;

    const handleResize = () => {
      if (confChartContainerRef.current && confChartRef.current) {
        confChartRef.current.applyOptions({
          width: confChartContainerRef.current.clientWidth,
        });
      }
    };

    const resizeObserver = new ResizeObserver(handleResize);
    resizeObserver.observe(confChartContainerRef.current);
    handleResize();

    return () => {
      resizeObserver.disconnect();
      chart.remove();
      confChartRef.current = null;
      confSeriesRef.current = null;
      thresholdSeriesRef.current = null;
    };
  }, []);

  // Sync time scales between charts - price chart is the master
  // Uses logical range sync which works when both charts have data
  useEffect(() => {
    if (!priceChartRef.current || !confChartRef.current) return;

    const priceTimeScale = priceChartRef.current.timeScale();
    const confTimeScale = confChartRef.current.timeScale();

    // Flag to prevent sync loops
    let isSyncing = false;

    const syncFromPrice = () => {
      if (isSyncing) return;
      // Only sync if confidence chart has data
      if (confDataRef.current.length === 0) return;

      isSyncing = true;
      const range = priceTimeScale.getVisibleLogicalRange();
      if (range) {
        // Calculate the time offset between charts due to different data lengths
        // Price chart has historical data, confidence chart starts from page load
        const priceDataLen = priceDataRef.current.length;
        const confDataLen = confDataRef.current.length;
        const offset = priceDataLen - confDataLen;

        // Adjust the logical range to account for the offset
        confTimeScale.setVisibleLogicalRange({
          from: range.from - offset,
          to: range.to - offset,
        });
      }
      isSyncing = false;
    };

    // Also sync confidence to price (in case confidence chart auto-fits on data update)
    const syncFromConfidence = () => {
      if (isSyncing) return;
      // Only sync if confidence chart has data
      if (confDataRef.current.length === 0) return;

      isSyncing = true;
      // Get price chart range and re-apply it to confidence chart
      // This ensures price chart is always the master
      const range = priceTimeScale.getVisibleLogicalRange();
      if (range) {
        const priceDataLen = priceDataRef.current.length;
        const confDataLen = confDataRef.current.length;
        const offset = priceDataLen - confDataLen;

        confTimeScale.setVisibleLogicalRange({
          from: range.from - offset,
          to: range.to - offset,
        });
      }
      isSyncing = false;
    };

    priceTimeScale.subscribeVisibleLogicalRangeChange(syncFromPrice);
    confTimeScale.subscribeVisibleLogicalRangeChange(syncFromConfidence);

    // Initial sync
    syncFromPrice();

    return () => {
      priceTimeScale.unsubscribeVisibleLogicalRangeChange(syncFromPrice);
      confTimeScale.unsubscribeVisibleLogicalRangeChange(syncFromConfidence);
    };
  }, []);

  // Fetch historical data (60 minutes initially)
  useEffect(() => {
    if (!priceSeriesRef.current || historicalLoadedRef.current) return;

    const loadHistory = async () => {
      setLoadingHistory(true);
      const initialMinutes = 60;
      const historicalData = await fetchHistoricalPrices(market.asset, initialMinutes);

      if (historicalData.length > 0 && priceSeriesRef.current) {
        priceDataRef.current = historicalData;
        priceSeriesRef.current.setData(historicalData as LineData<Time>[]);
        priceChartRef.current?.timeScale().fitContent();
        historicalLoadedRef.current = true;
        loadedMinutesRef.current = initialMinutes;
      }
      setLoadingHistory(false);
    };

    loadHistory();
  }, [market.asset]);

  // Lazy load more data when scrolling/zooming to earlier times
  useEffect(() => {
    if (!priceChartRef.current) return;

    const timeScale = priceChartRef.current.timeScale();

    const handleVisibleRangeChange = async () => {
      if (isLoadingMoreRef.current || !priceSeriesRef.current) return;

      const logicalRange = timeScale.getVisibleLogicalRange();
      if (!logicalRange) return;

      // Check if user scrolled to the left edge (viewing oldest data)
      const dataLength = priceDataRef.current.length;
      if (logicalRange.from < 10 && dataLength > 0) {
        // Load more data if we haven't loaded the max yet
        const currentMinutes = loadedMinutesRef.current;
        if (currentMinutes < 480) {
          isLoadingMoreRef.current = true;
          const newMinutes = Math.min(currentMinutes + 60, 480);

          const moreData = await fetchHistoricalPrices(market.asset, newMinutes);
          if (moreData.length > priceDataRef.current.length) {
            // Merge new data (older points) with existing
            priceDataRef.current = moreData;
            priceSeriesRef.current?.setData(moreData as LineData<Time>[]);
            loadedMinutesRef.current = newMinutes;
          }

          isLoadingMoreRef.current = false;
        }
      }
    };

    timeScale.subscribeVisibleLogicalRangeChange(handleVisibleRangeChange);

    return () => {
      timeScale.unsubscribeVisibleLogicalRangeChange(handleVisibleRangeChange);
    };
  }, [market.asset]);

  // Update strike price line
  useEffect(() => {
    if (!priceSeriesRef.current || !priceChartRef.current) return;

    const existingLines = priceSeriesRef.current.priceLines();
    existingLines.forEach((line) => priceSeriesRef.current?.removePriceLine(line));

    priceSeriesRef.current.createPriceLine({
      price: strikePrice,
      color: "#f59e0b", // amber-500
      lineWidth: 2,
      lineStyle: 2,
      axisLabelVisible: true,
      title: "Strike",
    });

    // Dynamically compute the price range from VISIBLE data points + strike price
    // This ensures the Y-axis scales to show only visible price data and the strike line
    priceSeriesRef.current.applyOptions({
      autoscaleInfoProvider: (original: () => { priceRange: { minValue: number; maxValue: number } } | null) => {
        // Get the default autoscale info (based on visible data)
        const defaultInfo = original();
        if (!defaultInfo) return null;

        // Include strike price in the range if it's set
        if (strikePrice > 0) {
          const minPrice = Math.min(defaultInfo.priceRange.minValue, strikePrice);
          const maxPrice = Math.max(defaultInfo.priceRange.maxValue, strikePrice);

          // Add 5% margin for visual breathing room
          const range = maxPrice - minPrice || maxPrice * 0.01;
          const margin = range * 0.05;

          return {
            priceRange: {
              minValue: minPrice - margin,
              maxValue: maxPrice + margin,
            },
          };
        }

        return defaultInfo;
      },
    });
  }, [strikePrice]);

  // Update spot price data
  useEffect(() => {
    if (!priceSeriesRef.current || market.spot_price === lastSpotPriceRef.current) return;

    lastSpotPriceRef.current = market.spot_price;
    const timeValue = Math.floor(Date.now() / 1000) as Time;

    const lastPoint = priceDataRef.current[priceDataRef.current.length - 1];
    if (lastPoint && lastPoint.time === timeValue) {
      lastPoint.value = spotPrice;
    } else {
      priceDataRef.current.push({ time: timeValue, value: spotPrice });
    }

    if (priceDataRef.current.length > 500) {
      priceDataRef.current = priceDataRef.current.slice(-500);
    }

    priceSeriesRef.current.setData(priceDataRef.current as LineData<Time>[]);
    // Don't auto-scroll - let user see full historical range
  }, [market.spot_price, spotPrice]);

  // Update confidence data
  useEffect(() => {
    if (!confSeriesRef.current || !thresholdSeriesRef.current || !confidenceData) return;

    const timeValue = Math.floor(Date.now() / 1000) as Time;

    // Update confidence series
    const lastConfPoint = confDataRef.current[confDataRef.current.length - 1];
    if (lastConfPoint && lastConfPoint.time === timeValue) {
      lastConfPoint.value = confidenceData.confidence;
    } else {
      confDataRef.current.push({ time: timeValue, value: confidenceData.confidence });
    }

    // Update threshold series
    const lastThresholdPoint = thresholdDataRef.current[thresholdDataRef.current.length - 1];
    const thresholdLevel = confidenceData.favorablePrice + confidenceData.threshold;
    if (lastThresholdPoint && lastThresholdPoint.time === timeValue) {
      lastThresholdPoint.value = thresholdLevel;
    } else {
      thresholdDataRef.current.push({ time: timeValue, value: thresholdLevel });
    }

    // Keep only last 500 points
    if (confDataRef.current.length > 500) {
      confDataRef.current = confDataRef.current.slice(-500);
    }
    if (thresholdDataRef.current.length > 500) {
      thresholdDataRef.current = thresholdDataRef.current.slice(-500);
    }

    confSeriesRef.current.setData(confDataRef.current as LineData<Time>[]);
    thresholdSeriesRef.current.setData(thresholdDataRef.current as LineData<Time>[]);

    // Auto-scale the confidence chart to show ALL historical data points
    confSeriesRef.current.applyOptions({
      autoscaleInfoProvider: () => {
        const confData = confDataRef.current;
        const threshData = thresholdDataRef.current;

        if (confData.length === 0 && threshData.length === 0) return null;

        // Find min/max from all confidence and threshold data
        let minValue = Infinity;
        let maxValue = -Infinity;

        for (const point of confData) {
          if (point.value < minValue) minValue = point.value;
          if (point.value > maxValue) maxValue = point.value;
        }

        for (const point of threshData) {
          if (point.value < minValue) minValue = point.value;
          if (point.value > maxValue) maxValue = point.value;
        }

        // Handle edge case where no valid data exists
        if (!isFinite(minValue) || !isFinite(maxValue)) {
          return null;
        }

        // Add 20% padding on each side for visual breathing room
        const dataRange = maxValue - minValue;
        const padding = Math.max(dataRange * 0.2, 0.05);

        return {
          priceRange: {
            minValue: Math.max(0, minValue - padding),
            maxValue: Math.min(1.0, maxValue + padding),
          },
        };
      },
    });

    // Color confidence line based on whether it's above threshold
    confSeriesRef.current.applyOptions({
      lineColor: confidenceData.wouldTrade ? "#22c55e" : "#ef4444", // green or red
      topColor: confidenceData.wouldTrade ? "rgba(34, 197, 94, 0.4)" : "rgba(239, 68, 68, 0.4)",
    });

    // Sync confidence chart time scale with price chart to prevent drift
    // Use logical range with offset adjustment for different data lengths
    if (priceChartRef.current && confChartRef.current && confDataRef.current.length > 0) {
      const priceRange = priceChartRef.current.timeScale().getVisibleLogicalRange();
      if (priceRange) {
        const priceDataLen = priceDataRef.current.length;
        const confDataLen = confDataRef.current.length;
        const offset = priceDataLen - confDataLen;

        confChartRef.current.timeScale().setVisibleLogicalRange({
          from: priceRange.from - offset,
          to: priceRange.to - offset,
        });
      }
    }
  }, [confidenceData]);

  // Update trade markers
  useEffect(() => {
    if (!markersRef.current) return;

    if (trades.length === 0) {
      markersRef.current.setMarkers([]);
      return;
    }

    const markers: SeriesMarker<Time>[] = trades.map((trade) => {
      const isBuy = trade.side === "BUY";
      const isYes = trade.outcome === "yes";
      const timeValue = Math.floor(new Date(trade.fill_time).getTime() / 1000) as Time;

      const position = isBuy ? "belowBar" : "aboveBar";
      const shape = isBuy ? "arrowUp" : "arrowDown";
      const color = isBuy ? "#22c55e" : "#ef4444";

      return {
        time: timeValue,
        position: position as "belowBar" | "aboveBar",
        shape: shape as "arrowUp" | "arrowDown",
        color: color,
        size: isYes ? 2 : 1,
        text: `${trade.side} ${trade.outcome.toUpperCase()}`,
      };
    });

    markers.sort((a, b) => (a.time as number) - (b.time as number));
    markersRef.current.setMarkers(markers);
  }, [trades]);

  // Calculate price vs strike
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
          {market.asset} Price & Confidence
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
        {/* Price Chart */}
        <div className="relative">
          <div
            ref={priceChartContainerRef}
            className="w-full"
            style={{ height: 280 }}
          />
          {loadingHistory && (
            <div className="absolute inset-0 flex items-center justify-center bg-background/50">
              <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
            </div>
          )}
        </div>

        {/* Confidence Indicator Header */}
        <div className="flex items-center gap-2 px-4 py-1 text-xs text-muted-foreground">
          <Activity className="h-3 w-3" />
          <span>Confidence Indicator</span>
          {confidenceData && (
            <>
              <span className="mx-2">|</span>
              <span className={confidenceData.wouldTrade ? "text-green-500" : "text-red-500"}>
                {(confidenceData.confidence * 100).toFixed(0)}%
              </span>
              <span className="text-muted-foreground/60">
                (need {((confidenceData.favorablePrice + confidenceData.threshold) * 100).toFixed(0)}%)
              </span>
              <span className="mx-2">|</span>
              <span>EV: {(confidenceData.ev * 100).toFixed(1)}%</span>
              <span className="mx-2">|</span>
              <span>Dist: ${confidenceData.distanceDollars.toFixed(0)} ({confidenceData.atrMultiple.toFixed(2)} ATR)</span>
            </>
          )}
        </div>

        {/* Confidence Chart */}
        <div
          ref={confChartContainerRef}
          className="w-full"
          style={{ height: 180 }}
        />

        {/* Legend */}
        <div className="mt-2 flex items-center justify-center gap-4 text-xs text-muted-foreground">
          <div className="flex items-center gap-1">
            <div className="h-0.5 w-4 bg-blue-500" />
            <span>Spot Price</span>
          </div>
          <div className="flex items-center gap-1">
            <div className="h-0.5 w-4 border-t-2 border-dashed border-amber-500" />
            <span>Strike</span>
          </div>
          <div className="flex items-center gap-1">
            <div className="h-2 w-4 rounded bg-green-500/40" />
            <span>Confidence</span>
          </div>
          <div className="flex items-center gap-1">
            <div className="h-0.5 w-4 bg-amber-500" />
            <span>Threshold</span>
          </div>
          <div className="flex items-center gap-1">
            <div className="h-0 w-0 border-b-4 border-l-4 border-r-4 border-b-green-500 border-l-transparent border-r-transparent" />
            <span>Buy</span>
          </div>
          <div className="flex items-center gap-1">
            <div className="h-0 w-0 border-l-4 border-r-4 border-t-4 border-l-transparent border-r-transparent border-t-red-500" />
            <span>Sell</span>
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
        <div className="h-4 w-32 animate-pulse rounded bg-muted" />
        <div className="flex items-center gap-3">
          <div className="h-4 w-20 animate-pulse rounded bg-muted" />
          <div className="h-4 w-20 animate-pulse rounded bg-muted" />
          <div className="h-4 w-4 animate-pulse rounded bg-muted" />
        </div>
      </CardHeader>
      <CardContent className="p-0 pb-4 pr-4">
        <div className="flex h-72 w-full items-center justify-center">
          <div className="h-64 w-full animate-pulse rounded bg-muted/50" />
        </div>
        <div className="mt-2 flex h-44 w-full items-center justify-center">
          <div className="h-40 w-full animate-pulse rounded bg-muted/50" />
        </div>
      </CardContent>
    </Card>
  );
}

export default PriceChart;
