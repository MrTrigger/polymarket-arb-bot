import { useEffect, useRef, useState, useMemo, useCallback } from "react";
import { useDashboardState } from "@/hooks";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  ChevronDown,
  ChevronUp,
  Search,
  Terminal,
  AlertCircle,
  AlertTriangle,
  Info,
  Bug,
  FileText,
} from "lucide-react";
import type { LogEntry, LogLevel } from "@/lib/types";

/**
 * Log level filter options.
 */
const LOG_LEVELS: LogLevel[] = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"];

/**
 * Log level configuration for styling.
 */
const LOG_LEVEL_CONFIG: Record<
  LogLevel,
  { color: string; bgColor: string; Icon: typeof AlertCircle }
> = {
  ERROR: {
    color: "text-red-500",
    bgColor: "bg-red-500/10",
    Icon: AlertCircle,
  },
  WARN: {
    color: "text-yellow-500",
    bgColor: "bg-yellow-500/10",
    Icon: AlertTriangle,
  },
  INFO: {
    color: "text-zinc-300",
    bgColor: "bg-zinc-500/10",
    Icon: Info,
  },
  DEBUG: {
    color: "text-zinc-500",
    bgColor: "bg-zinc-700/10",
    Icon: Bug,
  },
  TRACE: {
    color: "text-zinc-600",
    bgColor: "bg-zinc-800/10",
    Icon: FileText,
  },
};

/**
 * Collapsible log window showing recent logs with filtering capabilities.
 *
 * Features:
 * - Collapsible panel to save space
 * - Filter by log level (ERROR, WARN, INFO, DEBUG, TRACE)
 * - Text search across message and target
 * - Auto-scroll to latest logs
 * - Color-coded by level
 */
export function LogWindow() {
  const { recentLogs, initialized } = useDashboardState();
  const [isOpen, setIsOpen] = useState(true);
  const [selectedLevels, setSelectedLevels] = useState<Set<LogLevel>>(
    new Set(["ERROR", "WARN", "INFO"])
  );
  const [searchQuery, setSearchQuery] = useState("");
  const [autoScroll, setAutoScroll] = useState(true);

  const scrollRef = useRef<HTMLDivElement>(null);
  const lastLogCountRef = useRef(0);

  // Filter logs based on selected levels and search query
  const filteredLogs = useMemo(() => {
    return recentLogs.filter((log) => {
      // Level filter
      if (!selectedLevels.has(log.level)) {
        return false;
      }

      // Search filter
      if (searchQuery) {
        const query = searchQuery.toLowerCase();
        return (
          log.message.toLowerCase().includes(query) ||
          log.target.toLowerCase().includes(query) ||
          (log.event_id && log.event_id.toLowerCase().includes(query))
        );
      }

      return true;
    });
  }, [recentLogs, selectedLevels, searchQuery]);

  // Toggle level filter
  const toggleLevel = useCallback((level: LogLevel) => {
    setSelectedLevels((prev) => {
      const next = new Set(prev);
      if (next.has(level)) {
        next.delete(level);
      } else {
        next.add(level);
      }
      return next;
    });
  }, []);

  // Auto-scroll to bottom when new logs arrive
  useEffect(() => {
    if (autoScroll && scrollRef.current && filteredLogs.length > lastLogCountRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
    lastLogCountRef.current = filteredLogs.length;
  }, [filteredLogs.length, autoScroll]);

  // Handle manual scroll - disable auto-scroll if user scrolls up
  const handleScroll = useCallback(() => {
    if (!scrollRef.current) return;

    const { scrollTop, scrollHeight, clientHeight } = scrollRef.current;
    const isAtBottom = scrollHeight - scrollTop - clientHeight < 50;

    setAutoScroll(isAtBottom);
  }, []);

  if (!initialized) {
    return <LogWindowSkeleton />;
  }

  return (
    <Collapsible open={isOpen} onOpenChange={setIsOpen}>
      <Card className="border">
        <CardHeader className="pb-2">
          <CollapsibleTrigger className="flex w-full items-center justify-between">
            <CardTitle className="flex items-center gap-2 text-sm font-medium">
              <Terminal className="h-4 w-4" />
              Logs
              <Badge variant="secondary" className="ml-2 text-xs">
                {filteredLogs.length} / {recentLogs.length}
              </Badge>
            </CardTitle>
            <div className="flex items-center gap-2">
              {/* Error/warning count badges */}
              {recentLogs.filter((l) => l.level === "ERROR").length > 0 && (
                <Badge variant="destructive" className="text-xs">
                  {recentLogs.filter((l) => l.level === "ERROR").length} errors
                </Badge>
              )}
              {recentLogs.filter((l) => l.level === "WARN").length > 0 && (
                <Badge
                  variant="outline"
                  className="border-yellow-500 text-xs text-yellow-500"
                >
                  {recentLogs.filter((l) => l.level === "WARN").length} warnings
                </Badge>
              )}
              {isOpen ? (
                <ChevronUp className="h-4 w-4 text-muted-foreground" />
              ) : (
                <ChevronDown className="h-4 w-4 text-muted-foreground" />
              )}
            </div>
          </CollapsibleTrigger>
        </CardHeader>

        <CollapsibleContent>
          <CardContent className="space-y-3 pt-0">
            {/* Filters row */}
            <div className="flex flex-wrap items-center gap-2">
              {/* Level filters */}
              <div className="flex gap-1">
                {LOG_LEVELS.map((level) => {
                  const config = LOG_LEVEL_CONFIG[level];
                  const isSelected = selectedLevels.has(level);

                  return (
                    <button
                      key={level}
                      onClick={() => toggleLevel(level)}
                      className={`rounded-md px-2 py-1 text-xs font-medium transition-colors ${
                        isSelected
                          ? `${config.bgColor} ${config.color}`
                          : "bg-muted text-muted-foreground hover:bg-muted/80"
                      }`}
                    >
                      {level}
                    </button>
                  );
                })}
              </div>

              {/* Search input */}
              <div className="relative flex-1 min-w-[200px]">
                <Search className="absolute left-2 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
                <Input
                  type="text"
                  placeholder="Search logs..."
                  value={searchQuery}
                  onChange={(e) => setSearchQuery(e.target.value)}
                  className="h-8 pl-8 text-sm"
                />
              </div>

              {/* Auto-scroll indicator */}
              <button
                onClick={() => {
                  setAutoScroll(true);
                  if (scrollRef.current) {
                    scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
                  }
                }}
                className={`rounded-md px-2 py-1 text-xs ${
                  autoScroll
                    ? "bg-green-500/10 text-green-500"
                    : "bg-muted text-muted-foreground hover:bg-muted/80"
                }`}
                title={autoScroll ? "Auto-scroll enabled" : "Click to enable auto-scroll"}
              >
                {autoScroll ? "Auto" : "Manual"}
              </button>
            </div>

            {/* Log entries */}
            <div
              ref={scrollRef}
              onScroll={handleScroll}
              className="h-[300px] overflow-y-auto rounded-md border bg-zinc-950 p-2 font-mono text-xs"
            >
              {filteredLogs.length === 0 ? (
                <div className="flex h-full items-center justify-center text-muted-foreground">
                  {recentLogs.length === 0
                    ? "No logs yet"
                    : "No logs match filters"}
                </div>
              ) : (
                <div className="space-y-1">
                  {filteredLogs.map((log, index) => (
                    <LogLine key={`${log.timestamp}-${index}`} log={log} />
                  ))}
                </div>
              )}
            </div>
          </CardContent>
        </CollapsibleContent>
      </Card>
    </Collapsible>
  );
}

/**
 * Single log line component.
 */
function LogLine({ log }: { log: LogEntry }) {
  const config = LOG_LEVEL_CONFIG[log.level];
  const formattedTime = formatLogTime(log.timestamp);
  const shortTarget = shortenTarget(log.target);

  return (
    <div
      className={`flex gap-2 rounded px-1 py-0.5 hover:bg-zinc-800/50 ${
        log.level === "ERROR" ? "bg-red-500/5" : ""
      }`}
    >
      {/* Timestamp */}
      <span className="shrink-0 text-zinc-600">{formattedTime}</span>

      {/* Level badge */}
      <span className={`shrink-0 w-12 ${config.color}`}>
        [{log.level.padEnd(5)}]
      </span>

      {/* Target module */}
      <span className="shrink-0 w-28 truncate text-zinc-500" title={log.target}>
        {shortTarget}
      </span>

      {/* Message */}
      <span className={`flex-1 ${config.color} break-words`}>
        {log.message}
        {log.event_id && (
          <span className="ml-2 text-zinc-500">
            event={log.event_id.slice(0, 8)}
          </span>
        )}
      </span>
    </div>
  );
}

/**
 * Format timestamp for log display (HH:MM:SS.mmm).
 */
function formatLogTime(iso: string): string {
  const date = new Date(iso);
  const hours = date.getHours().toString().padStart(2, "0");
  const minutes = date.getMinutes().toString().padStart(2, "0");
  const seconds = date.getSeconds().toString().padStart(2, "0");
  const ms = date.getMilliseconds().toString().padStart(3, "0");
  return `${hours}:${minutes}:${seconds}.${ms}`;
}

/**
 * Shorten target module path for display.
 * e.g., "poly_bot::strategy::arb" -> "strategy::arb"
 */
function shortenTarget(target: string): string {
  const parts = target.split("::");
  if (parts.length <= 2) return target;
  // Keep last 2 parts
  return parts.slice(-2).join("::");
}

/**
 * Skeleton loader for LogWindow.
 */
export function LogWindowSkeleton() {
  return (
    <Card className="border">
      <CardHeader className="pb-2">
        <CardTitle className="flex items-center gap-2 text-sm font-medium">
          <Terminal className="h-4 w-4" />
          Logs
          <div className="ml-2 h-5 w-12 animate-pulse rounded bg-muted" />
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3 pt-0">
        <div className="flex gap-2">
          {[1, 2, 3, 4, 5].map((i) => (
            <div
              key={i}
              className="h-6 w-14 animate-pulse rounded bg-muted"
            />
          ))}
          <div className="h-8 flex-1 animate-pulse rounded bg-muted" />
        </div>
        <div className="h-[300px] animate-pulse rounded-md bg-zinc-950" />
      </CardContent>
    </Card>
  );
}

export default LogWindow;
