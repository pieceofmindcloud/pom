import { AnimatePresence, motion } from "framer-motion";
import { Route, Routes, useLocation } from "react-router-dom";
import { Sidebar } from "./components/Sidebar";
import { Topbar } from "./components/Topbar";
import { Chat } from "./routes/Chat";
import { Dashboard } from "./routes/Dashboard";
import { LogsByToken } from "./routes/LogsByToken";
import { LogsLive } from "./routes/LogsLive";
import { ModelPaths } from "./routes/ModelPaths";
import { NetworkSettings } from "./routes/NetworkSettings";
import { NotFound } from "./routes/NotFound";
import { Shards } from "./routes/Shards";
import { SliceModels } from "./routes/SliceModels";
import { SlicingJobs } from "./routes/SlicingJobs";
import { ConnectionKeys, TokenKeys } from "./routes/Keys";
import { SendFile } from "./routes/SendFile";
import { Transfers } from "./routes/Transfers";
import { WholeModels } from "./routes/WholeModels";

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
      className="min-w-0"
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
  const location = useLocation();
  const isChat = location.pathname === "/chat";

  return (
    <div className="flex h-full min-h-0 w-full overflow-hidden">
      <Sidebar />
      <div className="flex min-w-0 flex-1 flex-col">
        <Topbar />
        <main
          className={
            isChat
              ? "min-h-0 flex-1 overflow-hidden"
              : "min-h-0 flex-1 overflow-y-auto overflow-x-hidden"
          }
        >
          {isChat ? (
            <Chat />
          ) : (
            <div className="mx-auto w-full max-w-[1400px] px-4 py-6 sm:px-6 lg:px-8">
              <AnimatePresence mode="wait">
                <Routes location={location} key={location.pathname}>
                  <Route path="/" element={<Page><Dashboard /></Page>} />
                  <Route path="/modelos" element={<Page><WholeModels /></Page>} />
                  <Route path="/modelos/fatiar" element={<Page><SliceModels /></Page>} />
                  <Route path="/fatias" element={<Page><Shards /></Page>} />
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
            </div>
          )}
        </main>
      </div>
    </div>
  );
}
