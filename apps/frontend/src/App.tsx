import { lazy, Suspense } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { Navigate, Route, Routes, useLocation } from "react-router-dom";
import { canAccessRoute } from "./lib/access";
import { useProfile } from "./lib/profile";
import { useIsSetupMode } from "./lib/queries";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { Sidebar } from "./components/Sidebar";
import { Topbar } from "./components/Topbar";
import { useI18n } from "./i18n/provider";

/**
 * Route modules are loaded on demand so the dashboard does not pay for every
 * screen, editor and visualization before the operator opens it.
 */
export const routeLoaders = {
  setup: () => import("./routes/Setup").then(({ Setup }) => Setup),
  chat: () => import("./routes/Chat").then(({ Chat }) => Chat),
  dashboard: () => import("./routes/Dashboard").then(({ Dashboard }) => Dashboard),
  logs: () => import("./routes/Logs").then(({ Logs }) => Logs),
  modelPaths: () => import("./routes/ModelPaths").then(({ ModelPaths }) => ModelPaths),
  networkSettings: () => import("./routes/NetworkSettings").then(({ NetworkSettings }) => NetworkSettings),
  preferences: () => import("./routes/Preferences").then(({ Preferences }) => Preferences),
  notFound: () => import("./routes/NotFound").then(({ NotFound }) => NotFound),
  deployModel: () => import("./routes/DeployModel").then(({ DeployModel }) => DeployModel),
  deployments: () => import("./routes/Deployments").then(({ Deployments }) => Deployments),
  shards: () => import("./routes/Shards").then(({ Shards }) => Shards),
  sliceModels: () => import("./routes/SliceModels").then(({ SliceModels }) => SliceModels),
  tokenKeys: () => import("./routes/Keys").then(({ TokenKeys }) => TokenKeys),
  connectionKeys: () => import("./routes/Keys").then(({ ConnectionKeys }) => ConnectionKeys),
  sendFile: () => import("./routes/SendFile").then(({ SendFile }) => SendFile),
  transfers: () => import("./routes/Transfers").then(({ Transfers }) => Transfers),
  wholeModels: () => import("./routes/WholeModels").then(({ WholeModels }) => WholeModels),
  remoteModels: () => import("./routes/RemoteModels").then(({ RemoteModels }) => RemoteModels),
  documentation: () => import("./routes/Documentation").then(({ Documentation }) => Documentation),
  forbidden: () => import("./routes/Forbidden").then(({ Forbidden }) => Forbidden),
};

function lazyRoute<T extends React.ComponentType<any>>(loader: () => Promise<T>) {
  return lazy(() => loader().then((Component) => ({ default: Component })));
}

const Setup = lazyRoute(routeLoaders.setup);
const Chat = lazyRoute(routeLoaders.chat);
const Dashboard = lazyRoute(routeLoaders.dashboard);
const Logs = lazyRoute(routeLoaders.logs);
const ModelPaths = lazyRoute(routeLoaders.modelPaths);
const NetworkSettings = lazyRoute(routeLoaders.networkSettings);
const Preferences = lazyRoute(routeLoaders.preferences);
const NotFound = lazyRoute(routeLoaders.notFound);
const DeployModel = lazyRoute(routeLoaders.deployModel);
const Deployments = lazyRoute(routeLoaders.deployments);
const Shards = lazyRoute(routeLoaders.shards);
const SliceModels = lazyRoute(routeLoaders.sliceModels);
const TokenKeys = lazyRoute(routeLoaders.tokenKeys);
const ConnectionKeys = lazyRoute(routeLoaders.connectionKeys);
const SendFile = lazyRoute(routeLoaders.sendFile);
const Transfers = lazyRoute(routeLoaders.transfers);
const WholeModels = lazyRoute(routeLoaders.wholeModels);
const RemoteModels = lazyRoute(routeLoaders.remoteModels);
const Documentation = lazyRoute(routeLoaders.documentation);
const Forbidden = lazyRoute(routeLoaders.forbidden);

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

function RouteFallback({ fullScreen = false }: { fullScreen?: boolean }) {
  return (
    <div
      className={fullScreen ? "min-h-screen bg-ink" : "h-full min-h-[12rem] bg-ink"}
      aria-busy="true"
    />
  );
}

/**
 * The chat holds a live stream and a scroll position, so it renders outside the
 * page transition: unmounting it to animate would drop tokens mid-answer.
 */
export default function App() {
  const { t } = useI18n();
  const location = useLocation();
  const { profile } = useProfile();
  /*
   * O guarda de perfil fica aqui, e não dentro de cada rota, porque aqui ele
   * é um só: `/chat` renderiza fora do `<Routes>` (ver abaixo), então um guarda
   * por rota deixaria justamente essa de fora. Esconder o item no menu não
   * basta - o endereço continua digitável.
   *
   * Quem decide é `lib/access.ts`, a mesma tabela que filtra o menu.
   */
  const allowed = canAccessRoute(location.pathname, profile);
  const isChat = allowed && location.pathname === "/chat";
  // Screens that own the whole viewport and scroll inside themselves, instead
  // of sitting in the centred, padded column the other pages use.
  const isDashboard = location.pathname === "/";
  // Barrada, a tela volta para a coluna central: o aviso é um cartão pequeno, e
  // o layout de tela cheia não rola por dentro.
  const isFullBleed =
    allowed &&
    (isDashboard || location.pathname === "/documentacao" || location.pathname === "/implantar" || location.pathname === "/logs");
  /*
   * A node with no `pom.json` runs a router carrying only `/api/setup/*`,
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
  if (isSetup) {
    return (
      <ErrorBoundary>
        <Suspense fallback={<RouteFallback fullScreen />}>
          <Setup />
        </Suspense>
      </ErrorBoundary>
    );
  }

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
            isChat || isFullBleed
              ? "min-h-0 flex-1 overflow-hidden"
              : "min-h-0 flex-1 overflow-y-auto overflow-x-hidden"
          }
        >
          <ErrorBoundary resetKey={location.pathname}>
            <Suspense fallback={<RouteFallback />}>
              {!allowed ? (
                <div className="mx-auto w-full max-w-[1400px] px-4 py-6 sm:px-6 lg:px-8">
                  <Page>
                    <Forbidden />
                  </Page>
                </div>
              ) : isChat ? (
                <Chat />
              ) : (
                <div className={isFullBleed ? "h-full w-full" : "mx-auto w-full max-w-[1400px] px-4 py-6 sm:px-6 lg:px-8"}>
                  <AnimatePresence mode="wait">
                    <Routes location={location} key={location.pathname}>
                      <Route path="/" element={<Page><Dashboard /></Page>} />
                      <Route path="/modelos" element={<Page><WholeModels /></Page>} />
                      <Route path="/modelos/remotos" element={<Page><RemoteModels /></Page>} />
                      <Route path="/modelos/fatiar" element={<Page><SliceModels /></Page>} />
                      <Route path="/fatias" element={<Page><Shards /></Page>} />
                      <Route path="/implantar" element={<Page><DeployModel /></Page>} />
                      <Route path="/implantacoes" element={<Page><Deployments /></Page>} />
                      {/* Keep the former deploy URL working for old bookmarks. */}
                      <Route path="/fatias/implantar" element={<Navigate to="/implantar" replace />} />
                      <Route path="/transferencias" element={<Page><Transfers /></Page>} />
                      <Route path="/transferencias/enviar" element={<Page><SendFile /></Page>} />
                      <Route path="/logs" element={<Page><Logs /></Page>} />
                      <Route path="/documentacao" element={<Page><Documentation /></Page>} />
                      <Route path="/config/rede" element={<Page><NetworkSettings /></Page>} />
                      <Route path="/config/preferencias" element={<Page><Preferences /></Page>} />
                      <Route path="/config/keys/api" element={<Page><TokenKeys /></Page>} />
                      <Route path="/config/keys/conexao" element={<Page><ConnectionKeys /></Page>} />
                      <Route path="/config/pastas" element={<Page><ModelPaths /></Page>} />
                      <Route path="/config/armazenamento" element={<Page><ModelPaths /></Page>} />
                      <Route path="*" element={<Page><NotFound /></Page>} />
                    </Routes>
                  </AnimatePresence>
                </div>
              )}
            </Suspense>
          </ErrorBoundary>
        </main>
      </div>
    </div>
  );
}
