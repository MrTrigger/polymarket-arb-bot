import { BrowserRouter, Routes, Route } from "react-router-dom";
import { Header } from "@/components/layout";
import { Dashboard, MarketDetail } from "@/pages";
import { useDashboardState } from "@/hooks";

/**
 * Root layout wrapper with header and dark theme.
 */
function RootLayout({ children }: { children: React.ReactNode }) {
  const { connectionStatus, connect, disconnect } = useDashboardState();

  return (
    <div className="dark min-h-screen bg-background text-foreground">
      <Header
        connectionStatus={connectionStatus}
        onConnect={connect}
        onDisconnect={disconnect}
      />
      <main>{children}</main>
    </div>
  );
}

/**
 * Main application component with routing.
 *
 * Routes:
 * - / - Dashboard page (main overview)
 * - /market/:eventId - Market detail page
 */
function App() {
  return (
    <BrowserRouter>
      <RootLayout>
        <Routes>
          <Route path="/" element={<Dashboard />} />
          <Route path="/market/:eventId" element={<MarketDetail />} />
        </Routes>
      </RootLayout>
    </BrowserRouter>
  );
}

export default App;
