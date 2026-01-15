import { Wifi, WifiOff, AlertTriangle, Loader2 } from "lucide-react";
import type { ConnectionStatus } from "@/lib/types";

interface HeaderProps {
  connectionStatus: ConnectionStatus;
  onConnect: () => void;
  onDisconnect: () => void;
}

/**
 * Header component with connection status indicator.
 */
export function Header({
  connectionStatus,
  onConnect,
  onDisconnect,
}: HeaderProps) {
  return (
    <header className="border-b border-border bg-card px-6 py-4">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-4">
          <h1 className="text-xl font-semibold text-foreground">
            Polymarket Arb Bot
          </h1>
          <span className="text-sm text-muted-foreground">Dashboard</span>
        </div>
        <ConnectionStatusIndicator
          status={connectionStatus}
          onConnect={onConnect}
          onDisconnect={onDisconnect}
        />
      </div>
    </header>
  );
}

interface ConnectionStatusIndicatorProps {
  status: ConnectionStatus;
  onConnect: () => void;
  onDisconnect: () => void;
}

function ConnectionStatusIndicator({
  status,
  onConnect,
  onDisconnect,
}: ConnectionStatusIndicatorProps) {
  const statusConfig = {
    connected: {
      icon: Wifi,
      text: "Connected",
      className: "text-green-500",
      bgClassName: "bg-green-500/10",
    },
    connecting: {
      icon: Loader2,
      text: "Connecting...",
      className: "text-yellow-500 animate-spin",
      bgClassName: "bg-yellow-500/10",
    },
    disconnected: {
      icon: WifiOff,
      text: "Disconnected",
      className: "text-muted-foreground",
      bgClassName: "bg-muted",
    },
    error: {
      icon: AlertTriangle,
      text: "Connection Error",
      className: "text-red-500",
      bgClassName: "bg-red-500/10",
    },
  };

  const config = statusConfig[status];
  const Icon = config.icon;

  const handleClick = () => {
    if (status === "connected") {
      onDisconnect();
    } else if (status === "disconnected" || status === "error") {
      onConnect();
    }
  };

  const isClickable = status !== "connecting";

  return (
    <button
      onClick={handleClick}
      disabled={!isClickable}
      className={`flex items-center gap-2 rounded-full px-3 py-1.5 text-sm transition-colors ${config.bgClassName} ${isClickable ? "cursor-pointer hover:opacity-80" : "cursor-default"}`}
    >
      <Icon className={`h-4 w-4 ${config.className}`} />
      <span className={config.className}>{config.text}</span>
    </button>
  );
}

export default Header;
