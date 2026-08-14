import { AnimatePresence, motion } from "framer-motion";
import { Navigate, Route, Routes, useLocation } from "react-router-dom";
import { useIsSetupMode } from "./lib/queries";
import { Setup } from "./routes/Setup";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { Sidebar } from "./components/Sidebar";
import { Topbar } from "./components/Topbar";
import { Chat } from "./routes/Chat";
import { Dashboard } from "./routes/Dashboard";
import { LogsByToken } from "./routes/LogsByToken";
import { LogsLive } from "./routes/LogsLive";
import { ModelPaths } from "./routes/ModelPaths";
import { NetworkSettings } from "./routes/NetworkSettings";
import { NotFound } from "./routes/NotFound";
import { DeployModel } from "./routes/DeployModel";
import { Shards } from "./routes/Shards";
import { SliceModels } from "./routes/SliceModels";
import { SlicingJobs } from "./routes/SlicingJobs";
import { ConnectionKeys, TokenKeys } from "./routes/Keys";
import { SendFile } from "./routes/SendFile";
import { Transfers } from "./routes/Transfers";
import { WholeModels } from "./routes/WholeModels";
import { useI18n } from "./i18n/provider";

/**
 * Route changes cross-fade upward by a few pixels — enough to read as a new
 * screen arriving, short enough that it never delays the operator.
 */
function Page({ children }: { children: React.ReactNode }) {
  return (
    <motion.div
      initial={{ opacity: 0, y: 8 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0, y: -6 }}
      transition={{ duration: 0.19, ease: [0.22, 1, 0.36, 1] }}
      className="h-full min-w-0"
    >
      {children}
    </motion.div>
  );
}

/**
 * The chat holds a live stream and a scroll position, so it renders outside the
 * page transition: unmounting it to animate would drop tokens mid-answer.
 */
export default function App() {
  const { t } = useI18n();
  const location = useLocation();
  const isChat = location.pathname === "/chat";
  const isDashboard = location.pathname === "/";
  /*
   * A node with no `rdma-p2p.json` runs a router carrying only `/api/setup/*`,
   * so `/api/ui/node` answers 404. That miss is the signal.
   *
   * Deliberately not probed with `/api/setup/key`: that route *mints* a new
   * connection key, so asking it "are we in setup?" would rotate the cluster
   * credential on every page load.
   */
  const isSetup = useIsSetupMode();

  // Undefined means the probe has not answered yet. Rendering either branch on
  // a guess flashes the wrong screen, so hold the paint for one request.
  if (isSetup === undefined) return <div className="h-full bg-ink" />;
  if (isSetup) return <Setup />;

  return (
    <div className="flex h-full min-h-0 w-full overflow-hidden">
      {/*
        The chrome gets its own boundaries. The routed page below is the part
        that is expected to fail, but a toolbar that throws used to blank the
        whole window — including the menu that gets the operator out of it.
      */}
      <ErrorBoundary fallback={null}>
        <Sidebar />
      </ErrorBoundary>
      <div className="flex min-w-0 flex-1 flex-col">
        <ErrorBoundary
          fallback={
            <div className="border-b border-line px-4 py-3 text-xs text-faint">
              {t("app.topbarFailed")}
            </div>
          }
        >
          <Topbar />
        </ErrorBoundary>
        <main
          className={
            isChat || isDashboard
              ? "min-h-0 flex-1 overflow-hidden"
              : "min-h-0 flex-1 overflow-y-auto overflow-x-hidden"
          }
        >
          {isChat ? (
            <ErrorBoundary resetKey={location.pathname}>
              <Chat />
            </ErrorBoundary>
          ) : (
            <div className={isDashboard ? "h-full w-full" : "mx-auto w-full max-w-[1400px] px-4 py-6 sm:px-6 lg:px-8"}>
              <ErrorBoundary resetKey={location.pathname}>
                <AnimatePresence mode="wait">
                  <Routes location={location} key={location.pathname}>
                    <Route path="/" element={<Page><Dashboard /></Page>} />
                    <Route path="/modelos" element={<Page><WholeModels /></Page>} />
                    <Route path="/modelos/fatiar" element={<Page><SliceModels /></Page>} />
                    <Route path="/fatias" element={<Page><Shards /></Page>} />
                    <Route path="/implantar" element={<Page><DeployModel /></Page>} />
                    {/* Where deploying used to live, back when it was a leaf of
                        the shards section. Kept so links and bookmarks the
                        operator already has do not land on "page not found". */}
                    <Route path="/fatias/implantar" element={<Navigate to="/implantar" replace />} />
                    <Route path="/fatias/andamento" element={<Page><SlicingJobs /></Page>} />
                    <Route path="/transferencias" element={<Page><Transfers /></Page>} />
                    <Route path="/transferencias/enviar" element={<Page><SendFile /></Page>} />
                    <Route path="/logs" element={<Page><LogsByToken /></Page>} />
                    <Route path="/logs/ao-vivo" element={<Page><LogsLive /></Page>} />
                    <Route path="/config/rede" element={<Page><NetworkSettings /></Page>} />
                    <Route path="/config/keys/api" element={<Page><TokenKeys /></Page>} />
                    <Route path="/config/keys/conexao" element={<Page><ConnectionKeys /></Page>} />
                    <Route path="/config/pastas" element={<Page><ModelPaths /></Page>} />
                    <Route path="*" element={<Page><NotFound /></Page>} />
                  </Routes>
                </AnimatePresence>
              </ErrorBoundary>
            </div>
          )}
        </main>
      </div>
    </div>
  );
}
