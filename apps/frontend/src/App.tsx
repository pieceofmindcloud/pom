import { lazy, Suspense, useEffect, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { Navigate, Route, Routes, useLocation } from "react-router-dom";
import { canAccessRoute, homeRoute } from "./lib/access";
import {
  api,
  clearAdminAccessToken,
  onAdminSessionLost,
  setAdminAccessToken,
} from "./lib/api";
import { useProfile } from "./lib/profile";
import { ChatSessionProvider } from "./lib/chatStore";
import { useAdminSurface } from "./lib/queries";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { Sidebar } from "./components/Sidebar";
import { Topbar } from "./components/Topbar";
import { useI18n } from "./i18n/provider";
import { Button, Input, Panel, PanelHeader } from "./components/ui";

function AdminAccessGate({ onAuthenticated }: { onAuthenticated: () => void }) {
  const { t } = useI18n();
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState(false);

  async function authenticate(event: React.FormEvent) {
    event.preventDefault();
    if (!draft) {
      setFailed(true);
      return;
    }
    setBusy(true);
    setFailed(false);
    setAdminAccessToken(draft);
    try {
      await api.get("/api/ui/node", { auth: false });
      onAuthenticated();
    } catch {
      clearAdminAccessToken();
      setFailed(true);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-full items-center justify-center bg-ink px-5 py-10">
      <Panel className="w-full max-w-md">
        <PanelHeader
          eyebrow={t("accessGate.eyebrow")}
          title={t("accessGate.passwordTitle")}
          subtitle={t("accessGate.passwordSubtitle")}
        />
        <form
          className="space-y-4 px-5 py-5"
          autoComplete="on"
          onSubmit={(event) => void authenticate(event)}
        >
          <label className="block">
            <span className="mb-1.5 block text-[0.8rem] font-medium text-muted">
              {t("accessGate.passwordLabel")}
            </span>
            <Input
              autoFocus
              type="password"
              name="password"
              id="admin-access-password"
              value={draft}
              onChange={(event) => setDraft(event.target.value)}
              aria-label={t("accessGate.passwordLabel")}
              autoComplete="current-password"
            />
          </label>
          {failed && <p className="text-xs text-rose">{t("accessGate.passwordInvalid")}</p>}
          <Button variant="primary" loading={busy} type="submit" className="w-full">
            {t("accessGate.submit")}
          </Button>
        </form>
      </Panel>
    </div>
  );
}

function AdminLocalOnly() {
  return (
    <main className="flex min-h-full items-center justify-center bg-ink px-5 py-10 text-center">
      <section className="max-w-2xl">
        <p className="mb-8 text-2xl font-semibold text-fg">POM</p>
        <p className="text-base leading-relaxed text-muted">
          This administrative interface is available only through the local address{" "}
          <strong>127.0.0.1</strong> on this machine, not through its network IP address or the
          internet.
        </p>
        <p className="mt-4 text-base leading-relaxed text-muted" lang="pt-BR">
          Esta interface administrativa está disponível apenas pelo endereço local{" "}
          <strong>127.0.0.1</strong> desta máquina, não pelo IP de rede nem pela internet.
        </p>
      </section>
    </main>
  );
}

function AdminAccessUnavailable() {
  const { t } = useI18n();
  return (
    <div className="flex min-h-full items-center justify-center bg-ink px-5 py-10">
      <Panel className="w-full max-w-md">
        <PanelHeader
          eyebrow={t("accessGate.eyebrow")}
          title={t("accessGate.unavailableTitle")}
          subtitle={t("accessGate.unavailable")}
        />
      </Panel>
    </div>
  );
}

/**
 * Route modules are loaded on demand so the dashboard does not pay for every
 * screen, editor and visualization before the operator opens it.
 */
export const routeLoaders = {
  setup: () => import("./routes/Setup").then(({ Setup }) => Setup),
  setupWizard: () => import("./routes/SetupWizard").then(({ SetupWizard }) => SetupWizard),
  publicNetworks: () => import("./routes/PublicNetworks").then(({ PublicNetworks }) => PublicNetworks),
  chat: () => import("./routes/Chat").then(({ Chat }) => Chat),
  networkChat: () => import("./routes/NetworkChat").then(({ NetworkChat }) => NetworkChat),
  dashboard: () => import("./routes/Dashboard").then(({ Dashboard }) => Dashboard),
  logs: () => import("./routes/Logs").then(({ Logs }) => Logs),
  deploymentLogs: () => import("./routes/DeploymentLogs").then(({ DeploymentLogs }) => DeploymentLogs),
  statistics: () => import("./routes/Statistics").then(({ Statistics }) => Statistics),
  modelPaths: () => import("./routes/ModelPaths").then(({ ModelPaths }) => ModelPaths),
  networkSettings: () => import("./routes/NetworkSettings").then(({ NetworkSettings }) => NetworkSettings),
  networkParticipants: () => import("./routes/NetworkSettings").then(({ NetworkParticipants }) => NetworkParticipants),
  myMachine: () => import("./routes/MyMachine").then(({ MyMachine }) => MyMachine),
  adminAuthority: () => import("./routes/AdminAuthority").then(({ AdminAuthority }) => AdminAuthority),
  trustAnchor: () => import("./routes/TrustAnchor").then(({ TrustAnchor }) => TrustAnchor),
  networkSharing: () => import("./routes/NetworkSharing").then(({ NetworkSharing }) => NetworkSharing),
  preferences: () => import("./routes/Preferences").then(({ Preferences }) => Preferences),
  about: () => import("./routes/About").then(({ About }) => About),
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
const SetupWizard = lazyRoute(routeLoaders.setupWizard);
const PublicNetworks = lazyRoute(routeLoaders.publicNetworks);
const Chat = lazyRoute(routeLoaders.chat);
const NetworkChat = lazyRoute(routeLoaders.networkChat);
const Dashboard = lazyRoute(routeLoaders.dashboard);
const Logs = lazyRoute(routeLoaders.logs);
const DeploymentLogs = lazyRoute(routeLoaders.deploymentLogs);
const Statistics = lazyRoute(routeLoaders.statistics);
const ModelPaths = lazyRoute(routeLoaders.modelPaths);
const NetworkSettings = lazyRoute(routeLoaders.networkSettings);
const NetworkParticipants = lazyRoute(routeLoaders.networkParticipants);
const MyMachine = lazyRoute(routeLoaders.myMachine);
const AdminAuthority = lazyRoute(routeLoaders.adminAuthority);
const TrustAnchor = lazyRoute(routeLoaders.trustAnchor);
const NetworkSharing = lazyRoute(routeLoaders.networkSharing);
const Preferences = lazyRoute(routeLoaders.preferences);
const About = lazyRoute(routeLoaders.about);
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
  const isEmbeddedChat = allowed && location.pathname === "/admin-ui/chat";
  // Screens that own the whole viewport and scroll inside themselves, instead
  // of sitting in the centred, padded column the other pages use.
  const isDashboard = location.pathname === "/";
  // Barrada, a tela volta para a coluna central: o aviso é um cartão pequeno, e
  // o layout de tela cheia não rola por dentro.
  const isFullBleed =
    allowed &&
    (isDashboard ||
      location.pathname === "/documentacao" ||
      location.pathname === "/implantar" ||
      location.pathname === "/implantacoes" ||
      location.pathname === "/logs" ||
      location.pathname === "/implantacoes/logs" ||
      location.pathname === "/redes-publicas" ||
      location.pathname === "/network-chat" ||
      location.pathname === "/estatisticas" ||
      location.pathname === "/config/keys/api");
  /*
   * O assistente é a única tela que sai da moldura inteira: ele guia a
   * configuração do zero, com barra de progresso e uma pergunta por vez, e o
   * menu ao lado convida a sair pelo meio para uma tela que ainda não faz
   * sentido. Renderiza como o `Setup` do primeiro arranque - sem Sidebar, sem
   * Topbar -, e não só sem enchimento como as outras de tela cheia.
   */
  const isWizard = allowed && location.pathname === "/setup";
  /*
   * A node with no `pom.json` runs a router carrying only `/api/setup/*`,
   * so `/api/ui/node` answers 404. That miss is the signal.
   *
   * Deliberately not probed with a setup mutation endpoint. Setup mode is
   * detected by the normal surface probe, while `/api/setup/*` is reserved for
   * the real first-access wizard.
   */
  const adminSurface = useAdminSurface();
  const [adminReady, setAdminReady] = useState(false);

  /*
   * A sessão administrativa pode morrer com a aba aberta: ela vive na memória
   * do nó, e um reinício ou as 8 h de validade a levam. Quem refaz a sonda é
   * o próprio `useAdminSurface`; aqui só se esquece que esta aba já passou
   * pelo portão, senão a resposta "requires-password" continuaria escondida
   * atrás de um `adminReady` que ficou de uma sessão que não existe mais.
   */
  useEffect(() => onAdminSessionLost(() => setAdminReady(false)), []);

  // Undefined means the probe has not answered yet. Rendering either branch on
  // a guess flashes the wrong screen, so hold the paint for one request.
  if (adminSurface.isLoading) return <div className="h-full bg-ink" />;
  if (adminSurface.isError || !adminSurface.data) return <AdminAccessUnavailable />;
  if (adminSurface.data === "setup") {
    return (
      <ErrorBoundary>
        <Suspense fallback={<RouteFallback fullScreen />}>
          <Setup />
        </Suspense>
      </ErrorBoundary>
    );
  }

  if (adminSurface.data === "local-only") return <AdminLocalOnly />;
  if (adminSurface.data === "requires-password" && !adminReady) {
    return <AdminAccessGate onAuthenticated={() => setAdminReady(true)} />;
  }

  /*
   * O painel nativo da barra de status do macOS carrega esta rota dentro de um
   * `WKWebView` estreito. Ela renderiza o mesmo Chat sem Sidebar nem Topbar,
   * porque o chrome administrativo não cabe na largura de um telemóvel.
   *
   * Fica deliberadamente **depois** do portão de acesso acima: o painel usa a
   * mesma sessão com cookie `HttpOnly` que o navegador, e não um caminho de
   * credencial próprio.
   */
  if (isEmbeddedChat) {
    return (
      <ErrorBoundary>
        <Suspense fallback={<RouteFallback fullScreen />}>
          <div className="h-full min-h-0 w-full overflow-hidden bg-ink">
            <Chat />
          </div>
        </Suspense>
      </ErrorBoundary>
    );
  }

  if (isWizard) {
    return (
      <ErrorBoundary>
        <Suspense fallback={<RouteFallback fullScreen />}>
          <SetupWizard />
        </Suspense>
      </ErrorBoundary>
    );
  }

  return (
    <ChatSessionProvider>
      <div className="flex h-full min-h-0 w-full overflow-hidden">
        {/*
          The chrome gets its own boundaries. The routed page below is the part
          that is expected to fail, but a toolbar that throws used to blank the
          whole window - including the menu that gets the operator out of it.
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
                {!allowed && location.pathname === "/" ? (
                  /*
                   * O `/` é o Painel, que o `infer` não abre. Mandá-lo para a
                   * recusa faria a interface abrir num beco: a tela inicial do
                   * perfil é quem responde por ele.
                   */
                  <Navigate to={homeRoute(profile)} replace />
                ) : !allowed ? (
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
                        <Route path="/redes-publicas" element={<Page><PublicNetworks /></Page>} />
                        <Route path="/network-chat" element={<Page><NetworkChat /></Page>} />
                        <Route path="/modelos" element={<Page><WholeModels /></Page>} />
                        <Route path="/modelos/remotos" element={<Page><RemoteModels /></Page>} />
                        <Route path="/modelos/fatiar" element={<Page><SliceModels /></Page>} />
                        <Route path="/fatias" element={<Page><Shards /></Page>} />
                        <Route path="/implantar" element={<Page><DeployModel /></Page>} />
                        <Route path="/implantacoes" element={<Page><Deployments /></Page>} />
                        <Route path="/implantacoes/logs" element={<Page><DeploymentLogs /></Page>} />
                        {/* Keep the former deploy URL working for old bookmarks. */}
                        <Route path="/fatias/implantar" element={<Navigate to="/implantar" replace />} />
                        <Route path="/transferencias" element={<Page><Transfers /></Page>} />
                        <Route path="/transferencias/enviar" element={<Page><SendFile /></Page>} />
                        <Route path="/logs" element={<Page><Logs /></Page>} />
                        <Route path="/estatisticas" element={<Page><Statistics /></Page>} />
                        <Route path="/logs/estatisticas" element={<Navigate to="/estatisticas" replace />} />
                        <Route path="/documentacao" element={<Page><Documentation /></Page>} />
                        <Route path="/config/rede" element={<Page><NetworkSettings /></Page>} />
                        <Route path="/config/rede/participantes" element={<Page><NetworkParticipants /></Page>} />
                        <Route path="/config/minha-maquina" element={<Page><MyMachine /></Page>} />
                        <Route path="/config/administracao" element={<Page><AdminAuthority /></Page>} />
                        <Route path="/config/compartilhamento" element={<Page><NetworkSharing /></Page>} />
                        <Route path="/config/confianca" element={<Page><TrustAnchor /></Page>} />
                        <Route path="/config/preferencias" element={<Page><Preferences /></Page>} />
                        <Route path="/config/preferencias/admin" element={<Page><Preferences administrative /></Page>} />
                        <Route path="/config/sobre" element={<Page><About /></Page>} />
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
    </ChatSessionProvider>
  );
}
