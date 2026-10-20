import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { Navigate, Route, Routes, useLocation } from "react-router-dom";
import { canAccessRoute, homeRoute, isEnterprisePath } from "./lib/access";
import {
  api,
  clearAdminAccessToken,
  onAdminSessionLost,
  setAdminAccessToken,
} from "./lib/api";
import { useProfile } from "./lib/profile";
import { ChatSessionProvider } from "./lib/chatStore";
import { useAdminSurface, usePluginUi } from "./lib/queries";
import { pluginUiSnapshot, resolvePluginRoute, setPluginUi } from "./lib/pluginUi";
import { PluginScreen } from "./components/PluginScreen";
import { PluginLoading } from "./components/PluginLoading";
import { PluginBridge } from "./components/PluginBridge";
import { syncPluginCatalogs } from "./lib/pluginI18n";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { Sidebar } from "./components/Sidebar";
import { Topbar } from "./components/Topbar";
import { FirstRunGuide } from "./components/FirstRunGuide";
import { FirstRunWizard, useFirstRunWizard } from "./components/FirstRunWizard";
import { EnterpriseFeatureShowcase } from "./components/EnterpriseFeatureShowcase";
import { useI18n } from "./i18n/provider";
import pomMark from "./assets/pom-mark-dark.png";
import { BrandLanguageSwitch } from "./components/BrandLanguageSwitch";
import { Panel, PanelHeader } from "./components/ui";


function AdminAccessGate({ onAuthenticated }: { onAuthenticated: () => void }) {
  const { t } = useI18n();
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState(false);
  const formRef = useRef<HTMLFormElement>(null);

  /** Shakes the form on a refused password, except with reduced motion. */
  function shake() {
    const form = formRef.current;
    if (!form || typeof form.animate !== "function") return;
    if (window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) return;
    form.animate(
      [
        { transform: "translateX(0)" },
        { transform: "translateX(-9px)" },
        { transform: "translateX(8px)" },
        { transform: "translateX(-5px)" },
        { transform: "translateX(3px)" },
        { transform: "translateX(0)" },
      ],
      { duration: 420, easing: "cubic-bezier(.36,.07,.19,.97)" },
    );
  }

  async function authenticate(event: React.FormEvent) {
    event.preventDefault();
    if (!draft) {
      setFailed(true);
      shake();
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
      shake();
    } finally {
      setBusy(false);
    }
  }

  // Same visual signature as the pieceofmind.cloud home page: the floating
  // dark panel with the brand, over the theme background. Only the password is asked.
  return (
    <main className="pom-login">
      {/* The language comes from the browser (English for those not using Portuguese); the
          flags switch instantly, as on the site's home page. */}
      <BrandLanguageSwitch />
      <section className="pom-login-panel">
        <img className="pom-login-mark" src={pomMark} alt="POM" width={815} height={320} />
        <p className="pom-login-sub">PIECE OF MIND</p>
        {/* A piece of mind traveling between machines: decorative. */}
        <span className="pom-login-link" aria-hidden="true"><i /><i /><i /></span>
        <form ref={formRef} className="pom-login-form" autoComplete="on" onSubmit={(event) => void authenticate(event)}>
          <label className="sr-only" htmlFor="admin-access-password">{t("accessGate.passwordLabel")}</label>
          <input
            autoFocus
            className="pom-login-input"
            type="password"
            name="password"
            id="admin-access-password"
            value={draft}
            placeholder={t("accessGate.passwordPlaceholder")}
            onChange={(event) => setDraft(event.target.value)}
            aria-label={t("accessGate.passwordLabel")}
            aria-invalid={failed || undefined}
            autoComplete="current-password"
          />
          <button className="pom-login-button" type="submit" disabled={busy}>
            {busy ? <span className="pom-login-spinner" aria-hidden="true" /> : null}
            {t("accessGate.submit")}
          </button>
          {failed && <p role="alert" className="pom-login-error">{t("accessGate.passwordInvalid")}</p>}
        </form>
      </section>
    </main>
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
  publicNetworks: () => import("./routes/PublicNetworks").then(({ PublicNetworks }) => PublicNetworks),
  chat: () => import("./routes/Chat").then(({ Chat }) => Chat),
  networkChat: () => import("./routes/NetworkChat").then(({ NetworkChat }) => NetworkChat),
  dashboard: () => import("./routes/Dashboard").then(({ Dashboard }) => Dashboard),
  deploymentLogs: () => import("./routes/DeploymentLogs").then(({ DeploymentLogs }) => DeploymentLogs),
  plugins: () => import("./routes/Plugins").then(({ Plugins }) => Plugins),
  modelPaths: () => import("./routes/ModelPaths").then(({ ModelPaths }) => ModelPaths),
  networkSettings: () => import("./routes/NetworkSettings").then(({ NetworkSettings }) => NetworkSettings),
  networkParticipants: () => import("./routes/NetworkSettings").then(({ NetworkParticipants }) => NetworkParticipants),
  myMachine: () => import("./routes/MyMachine").then(({ MyMachine }) => MyMachine),
  myBudget: () => import("./routes/MyBudget").then(({ MyBudget }) => MyBudget),
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

const PublicNetworks = lazyRoute(routeLoaders.publicNetworks);
const Chat = lazyRoute(routeLoaders.chat);
const NetworkChat = lazyRoute(routeLoaders.networkChat);
const Dashboard = lazyRoute(routeLoaders.dashboard);
const DeploymentLogs = lazyRoute(routeLoaders.deploymentLogs);
const Plugins = lazyRoute(routeLoaders.plugins);
const ModelPaths = lazyRoute(routeLoaders.modelPaths);
const NetworkSettings = lazyRoute(routeLoaders.networkSettings);
const NetworkParticipants = lazyRoute(routeLoaders.networkParticipants);
const MyMachine = lazyRoute(routeLoaders.myMachine);
const MyBudget = lazyRoute(routeLoaders.myBudget);
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
  const { t, locale } = useI18n();
  const location = useLocation();
  const { profile, known: profileKnown } = useProfile();
  const pluginUi = usePluginUi();
  // The registry is read by `canAccessRoute` and by the menu; it must be up to date
  // before any child consults the guard.
  setPluginUi(pluginUi.data?.plugins);
  const pluginRoute = resolvePluginRoute(location.pathname);
  // Each plugin brings its own texts; follows active plugins and language.
  const activePlugins = pluginUiSnapshot();
  useEffect(() => {
    void syncPluginCatalogs(activePlugins ?? [], locale);
  }, [activePlugins, locale]);
  /*
   * The profile guard lives here, and not inside each route, because here it
   * is a single one: `/chat` renders outside `<Routes>` (see below), so a guard
   * per route would leave exactly that one out. Hiding the item in the menu is not
   * enough - the address is still typeable.
   *
   * The decision is made by `lib/access.ts`, the same table that filters the menu.
   */
  // Fails closed: until the node says which profile this is, no route opens.
  const allowed = profileKnown && canAccessRoute(location.pathname, profile);
  /*
   * Screens provided by plugins (`lib/pluginUi.ts`) use the namespace of the
   * plugin's own `plugin_code` inside the `/admin-ui` basename. A manifest may
   * claim `/logs` or `/statistics` to replace the corresponding native fallback
   * when there is no other plugin with the same claim.
   */
  const enterprisePath = isEnterprisePath(location.pathname);
  /*
   * `/logs` and `/statistics` are also POM's advertising fallbacks. Until the node
   * answers `GET /api/ui/plugins/ui` the registry cannot know whether a plugin
   * claims them, so the advertising card would flash in front of a plugin that is
   * about to take the screen: the pieces animation is drawn while that answer is
   * still on its way.
   */
  const pluginUiPending = pluginUi.isPending === true;
  const isChat = allowed && location.pathname === "/chat";
  const isEmbeddedChat = allowed && location.pathname === "/admin-ui/chat";
  // Screens that own the whole viewport and scroll inside themselves, instead
  // of sitting in the centred, padded column the other pages use.
  const isDashboard = location.pathname === "/";
  // Barred, the screen goes back to the centred column: the notice is a small card, and
  // the full-screen layout does not scroll internally.
  const isFullBleed =
    allowed &&
    (isDashboard ||
      location.pathname === "/documentation" ||
      location.pathname === "/deploy" ||
      location.pathname === "/deployments" ||
      pluginRoute?.route.full_bleed === true ||
      location.pathname === "/deployments/logs" ||
      location.pathname === "/public-networks" ||
      location.pathname === "/network-chat" ||
      location.pathname === "/config/keys/api" ||
      location.pathname === "/models" ||
      location.pathname === "/models/remote" ||
      location.pathname === "/models/slice" ||
      location.pathname === "/plugins" ||
      location.pathname === "/shards" ||
      location.pathname === "/transfers" ||
      location.pathname === "/transfers/send");
  const adminSurface = useAdminSurface();
  const [adminReady, setAdminReady] = useState(false);
  // A machine on its first run names itself and its network before the panel
  // opens. Asked only past the gate, where the guide route answers.
  const wizard = useFirstRunWizard(
    profile === "admin" &&
      !isEmbeddedChat &&
      (adminSurface.data === "available" || (adminSurface.data === "requires-password" && adminReady)),
  );

  /*
   * The administrative session may die with the tab open: it lives in the node's
   * memory, and a restart or the 8 h validity takes it away. Who redoes the probe is
   * `useAdminSurface` itself; here we only forget that this tab already went
   * through the gate, otherwise the "requires-password" answer would stay hidden
   * behind an `adminReady` left over from a session that no longer exists.
   */
  useEffect(() => onAdminSessionLost(() => setAdminReady(false)), []);

  // Undefined means the probe has not answered yet. Rendering either branch on
  // a guess flashes the wrong screen, so hold the paint for one request.
  if (adminSurface.isLoading) return <div className="h-full bg-ink" />;
  if (adminSurface.isError || !adminSurface.data) return <AdminAccessUnavailable />;
  if (adminSurface.data === "local-only") return <AdminLocalOnly />;
  if (adminSurface.data === "requires-password" && !adminReady) {
    return <AdminAccessGate onAuthenticated={() => setAdminReady(true)} />;
  }

  /*
   * The native macOS status bar panel loads this route inside a narrow
   * `WKWebView`. It renders the same Chat without Sidebar or Topbar,
   * because the administrative chrome does not fit the width of a phone.
   *
   * It deliberately sits **after** the access gate above: the panel uses the
   * same `HttpOnly` cookie session as the browser, and not a credential path
   * of its own.
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

  if (wizard.loading) return <div className="h-full bg-ink" />;
  if (wizard.step) {
    return (
      <ErrorBoundary>
        <FirstRunWizard step={wizard.step} />
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
          <PluginBridge />
        </ErrorBoundary>
        <ErrorBoundary fallback={null}>
          <Sidebar />
        </ErrorBoundary>
        <ErrorBoundary fallback={null}>
          <FirstRunGuide enabled={profile === "admin"} />
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
                : "min-h-0 flex-1 overflow-y-auto overflow-x-hidden overscroll-y-contain"
            }
          >
            <ErrorBoundary resetKey={location.pathname}>
              <Suspense fallback={<RouteFallback />}>
                {!profileKnown ? (
                  <RouteFallback />
                ) : !allowed && location.pathname === "/" ? (
                  /*
                   * The `/` is the Dashboard, which `infer` does not open. Sending it to the
                   * refusal would make the interface open in a dead end: the profile's
                   * home screen is what answers for it.
                   */
                  <Navigate to={homeRoute(profile)} replace />
                ) : !allowed ? (
                  <div className="mx-auto w-full max-w-[1400px] px-4 py-6 sm:px-6 lg:px-8">
                    <Page>
                      <Forbidden />
                    </Page>
                  </div>
                ) : location.pathname === "/logs/statistics" ? (
                  // Old alias, kept outside `<Routes>` because the
                  // destination is also a plugin route or the Enterprise showcase.
                  <Navigate to="/statistics" replace />
                ) : pluginRoute ? (
                  <div className={pluginRoute.route.full_bleed ? "h-full w-full" : "mx-auto w-full max-w-[1400px] px-4 py-6 sm:px-6 lg:px-8"}>
                    <PluginScreen
                      key={`${pluginRoute.code}:${pluginRoute.route.path}`}
                      resolved={pluginRoute}
                    />
                  </div>
                ) : enterprisePath ? (
                  pluginUiPending ? (
                    <PluginLoading />
                  ) : (
                    <EnterpriseFeatureShowcase kind={location.pathname.startsWith("/logs") ? "logs" : "statistics"} />
                  )
                ) : isChat ? (
                  <Chat />
                ) : (
                  <div className={isFullBleed ? "h-full w-full" : "mx-auto w-full max-w-[1400px] px-4 py-6 sm:px-6 lg:px-8"}>
                    <AnimatePresence mode="wait">
                      <Routes location={location} key={location.pathname}>
                        <Route path="/" element={<Page><Dashboard /></Page>} />
                        <Route path="/public-networks" element={<Page><PublicNetworks /></Page>} />
                        <Route path="/network-chat" element={<Page><NetworkChat /></Page>} />
                        <Route path="/models" element={<Page><WholeModels /></Page>} />
                        <Route path="/models/remote" element={<Page><RemoteModels /></Page>} />
                        <Route path="/models/slice" element={<Page><SliceModels /></Page>} />
                        <Route path="/shards" element={<Page><Shards /></Page>} />
                        <Route path="/deploy" element={<Page><DeployModel /></Page>} />
                        <Route path="/deployments" element={<Page><Deployments /></Page>} />
                        <Route path="/deployments/logs" element={<Page><DeploymentLogs /></Page>} />
                        {/* Keep the former deploy URL working for old bookmarks. */}
                        <Route path="/shards/deploy" element={<Navigate to="/deploy" replace />} />
                        <Route path="/transfers" element={<Page><Transfers /></Page>} />
                        <Route path="/transfers/send" element={<Page><SendFile /></Page>} />
                        <Route path="/plugins" element={<Page><Plugins /></Page>} />
                        <Route path="/documentation" element={<Page><Documentation /></Page>} />
                        <Route path="/config/network" element={<Page><NetworkSettings /></Page>} />
                        <Route path="/config/network/participants" element={<Page><NetworkParticipants /></Page>} />
                        <Route path="/config/my-machine" element={<Page><MyMachine /></Page>} />
                        <Route path="/config/usage" element={<Page><MyBudget /></Page>} />
                        <Route path="/config/budget" element={<Navigate to="/config/usage" replace />} />
                        <Route path="/config/administration" element={<Page><AdminAuthority /></Page>} />
                        <Route path="/config/sharing" element={<Page><NetworkSharing /></Page>} />
                        <Route path="/config/trust" element={<Page><TrustAnchor /></Page>} />
                        <Route path="/config/preferences" element={<Page><Preferences /></Page>} />
                        <Route path="/config/preferences/admin" element={<Page><Preferences administrative /></Page>} />
                        <Route path="/config/about" element={<Page><About /></Page>} />
                        <Route path="/config/keys/api" element={<Page><TokenKeys /></Page>} />
                        <Route path="/config/keys/connection" element={<Page><ConnectionKeys /></Page>} />
                        <Route path="/config/folders" element={<Page><ModelPaths /></Page>} />
                        <Route path="/config/storage" element={<Page><ModelPaths /></Page>} />
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
