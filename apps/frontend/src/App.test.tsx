// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { BrowserRouter, MemoryRouter, useNavigate } from "react-router-dom";
import { useState } from "react";
import { LocaleProvider } from "./i18n/provider";
import { ProfileProvider, type Profile } from "./lib/profile";

const adminSurface = vi.hoisted(() => ({
  value: "available" as "available" | "requires-password" | "local-only",
}));
const pluginUi = vi.hoisted(() => ({ plugins: [] as unknown[] }));

/**
 * Hiding the menu item is not blocking: the address is still typeable, and an old
 * bookmark goes straight in. This test is what proves that the closed route
 * **does not render the screen**, and that the open one keeps rendering.
 *
 * That is why it mounts the real `App`, with the screens swapped for markers:
 * what is under test is the guard, not the content of each screen.
 */
vi.mock("./lib/queries", () => ({
  useAdminSurface: () => ({ data: adminSurface.value, isLoading: false, isError: false }),
  useAdminAccess: () => ({ data: { default_password: false } }),
  useNetworkChat: () => ({ data: { notices: [] } }),
  usePluginUi: () => ({ data: { plugins: pluginUi.plugins }, isLoading: false, isError: false }),
  // Node complete enough for the top bar and the menu footer to
  // render: a `data: null` brings down the `NodeChip` at `node.data!` and the
  // test ends up measuring the `ErrorBoundary` instead of the guard.
  useNode: () => ({
    data: {
      hostname: "salem",
      short_id: "a1b2",
      status: { state: "idle" },
      config: null,
      tls_required: false,
      build_commit: "abc1234",
    },
    isLoading: false,
    isError: false,
  }),
  useCluster: () => ({ data: null }),
  useInvalidate: () => vi.fn(),
  usePublicPluginCatalog: () => ({ data: undefined, isLoading: false, isError: false }),
}));

vi.mock("./components/Notifications", () => ({ Notifications: () => null }));
vi.mock("./components/Sidebar", () => ({ Sidebar: () => <aside data-testid="admin-sidebar" /> }));
vi.mock("./components/FirstRunGuide", () => ({ FirstRunGuide: () => null }));
vi.mock("./components/FirstRunWizard", () => ({
  FirstRunWizard: () => null,
  useFirstRunWizard: () => ({ loading: false, step: null }),
}));
vi.mock("./components/Topbar", () => ({ Topbar: () => <header data-testid="admin-topbar" /> }));
vi.mock("./components/Logo", () => ({
  LogoFull: () => <div aria-hidden="true" />,
  LogoMark: () => <div aria-hidden="true" />,
}));

// `vi.mock` is hoisted to the top of the file, before any `const`, so each
// marker is written by hand instead of coming from a shared helper.
vi.mock("./routes/DeployModel", () => ({ DeployModel: () => <div>tela-implantar</div> }));
vi.mock("./routes/Deployments", () => ({ Deployments: () => <div>tela-implantacoes</div> }));
vi.mock("./components/PluginScreen", () => ({
  PluginScreen: ({ resolved }: { resolved: { code: string; route: { path: string; screen: string } } }) => {
    // Simulates the real screen keeping the component loaded while the parent swaps
    // the route. App must remount it by route identity so as not to leave
    // the previous screen stuck when two destinations share the same module.
    const [initialPath] = useState(resolved.route.path);
    const [initialScreen] = useState(resolved.route.screen);
    return (
      <>
        <div>{`tela-plugin:${resolved.code}:${initialScreen}`}</div>
        <div>{`tela-plugin:${resolved.code}:${initialPath}`}</div>
      </>
    );
  },
}));
vi.mock("./routes/WholeModels", () => ({ WholeModels: () => <div>tela-modelos</div> }));
vi.mock("./routes/Plugins", () => ({ Plugins: () => <div>tela-plugins</div> }));
vi.mock("./routes/Dashboard", () => ({ Dashboard: () => <div>tela-painel</div> }));
vi.mock("./routes/Chat", () => ({ Chat: () => <div>tela-chat</div> }));
vi.mock("./routes/Preferences", () => ({ Preferences: () => <div>tela-preferencias</div> }));
vi.mock("./routes/SetupWizard", () => ({ SetupWizard: () => <div>tela-wizard</div> }));

import App from "./App";

function open(path: string, profile: Profile) {
  return render(
    // Same flags that `main.tsx` turns on: without them the test runs in a router
    // mode the application does not use, and the console fills with future warnings.
    <MemoryRouter
      initialEntries={[path]}
      future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
    >
      <LocaleProvider initialLocale="pt-BR">
        <ProfileProvider initialProfile={profile}>
          <App />
        </ProfileProvider>
      </LocaleProvider>
    </MemoryRouter>,
  );
}

function openBrowserPath(path: string, profile: Profile) {
  window.history.replaceState({}, "", path);
  return render(
    <BrowserRouter basename="/admin-ui" future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
      <LocaleProvider initialLocale="pt-BR">
        <ProfileProvider initialProfile={profile}>
          <App />
        </ProfileProvider>
      </LocaleProvider>
    </BrowserRouter>,
  );
}

function RouteDriver() {
  const navigate = useNavigate();
  return (
    <>
      <button type="button" onClick={() => navigate("/base/example")}>
        ir para exemplo
      </button>
      <button type="button" onClick={() => navigate("/statistics")}>
        ir para estatísticas
      </button>
    </>
  );
}

function openWithRouteDriver(path: string, profile: Profile) {
  return render(
    <MemoryRouter
      initialEntries={[path]}
      future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
    >
      <RouteDriver />
      <LocaleProvider initialLocale="pt-BR">
        <ProfileProvider initialProfile={profile}>
          <App />
        </ProfileProvider>
      </LocaleProvider>
    </MemoryRouter>,
  );
}

describe("profile guard per route", () => {
  afterEach(() => {
    cleanup();
    adminSurface.value = "available";
    pluginUi.plugins = [];
    window.history.replaceState({}, "", "/");
  });

  it("renders Plugins in the full-bleed container", async () => {
    open("/plugins", "admin");

    const content = await screen.findByText("tela-plugins");
    expect(content.closest(".h-full.w-full")).toBeTruthy();
  });

  const closed = [
    ["/deploy", "tela-implantar"],
    ["/deployments", "tela-implantacoes"],
  ] as const;

  for (const [path, marker] of closed) {
    it(`opens ${path} for the administrator`, async () => {
      open(path, "admin");
      expect(await screen.findByText(marker)).toBeTruthy();
      expect(screen.queryByText("Tela restrita a outro perfil")).toBeNull();
    });

    it(`blocks ${path} for the node profile`, async () => {
      open(path, "node");
      expect(screen.queryByText(marker)).toBeNull();
      expect(await screen.findByText("Tela restrita a outro perfil")).toBeTruthy();
    });
  }

  const enterpriseManifest = (profiles: string[]) => ({
    code: "logs",
    version: "1.0.0",
    manifest: {
      schema: "pom-plugin-ui/v1",
      plugin_code: "logs",
      menu: [
        { id: "logs", label: { en: "Request Logs" }, to: "/logs", profiles },
        { id: "stats", label: { en: "Statistics" }, to: "/statistics", profiles },
      ],
      routes: [
        { path: "/logs", screen: "logs", profiles, full_bleed: true },
        { path: "/statistics", screen: "stats", profiles },
      ],
      screens: { logs: { module: "ui/logs.js" }, stats: { module: "ui/stats.js" } },
      assets: ["ui/logs.js", "ui/stats.js"],
    },
  });

  const baseManifest = {
    code: "base",
    version: "1.0.0",
    manifest: {
      schema: "pom-plugin-ui/v1",
      plugin_code: "base",
      menu: [
        { id: "plugin", label: { en: "Plugin" }, to: "/plugin-de-base", profiles: ["admin"] },
        { id: "example", label: { en: "Example" }, to: "/example", profiles: ["admin", "node"] },
      ],
      routes: [
        { path: "/plugin-de-base", screen: "plugin", profiles: ["admin"] },
        { path: "/example", screen: "example", profiles: ["admin", "node"] },
      ],
      screens: {
        plugin: { module: "ui/plugin.js" },
        example: { module: "ui/example.js" },
      },
      assets: ["ui/plugin.js", "ui/example.js"],
    },
  };

  const analyticsManifest = {
    code: "analytics",
    version: "1.0.0",
    manifest: {
      schema: "pom-plugin-ui/v1",
      plugin_code: "analytics",
      menu: [{ id: "example", label: { en: "Analytics Example" }, to: "/example", profiles: ["infer"] }],
      routes: [{ path: "/example", screen: "example", profiles: ["infer"] }],
      screens: { example: { module: "ui/example.js" } },
      assets: ["ui/example.js"],
    },
  };

  it("shows the advertising fallback when no plugin provides /logs or /statistics", async () => {
    open("/logs", "admin");
    expect(await screen.findAllByRole("button", { name: "Comprar agora" })).toBeTruthy();
    expect(screen.queryByText(/^tela-plugin:/)).toBeNull();
    cleanup();
    open("/statistics", "admin");
    expect(await screen.findAllByRole("button", { name: "Comprar agora" })).toBeTruthy();
  });

  it("renders plugin screens across profiles while retaining native infer restrictions", async () => {
    pluginUi.plugins = [enterpriseManifest(["admin", "node"])];
    open("/logs", "admin");
    expect(await screen.findByText("tela-plugin:logs:logs")).toBeTruthy();
    cleanup();
    open("/statistics", "node");
    expect(await screen.findByText("tela-plugin:logs:stats")).toBeTruthy();
    cleanup();
    // POM keeps Logs closed to infer, even when a plugin provides it.
    open("/logs", "infer");
    expect(await screen.findByText("Tela restrita a outro perfil")).toBeTruthy();
    expect(screen.queryByText(/^tela-plugin:/)).toBeNull();
  });

  it("opens an admin-only manifest route for node and infer profiles", async () => {
    pluginUi.plugins = [baseManifest];
    open("/base/plugin-de-base", "node");
    expect(await screen.findByText("tela-plugin:base:plugin")).toBeTruthy();
    cleanup();

    open("/base/plugin-de-base", "infer");
    expect(await screen.findByText("tela-plugin:base:plugin")).toBeTruthy();
  });

  it("remounts a plugin screen when navigation changes route within the same plugin", async () => {
    pluginUi.plugins = [baseManifest];
    openWithRouteDriver("/base/plugin-de-base", "admin");

    expect(await screen.findByText("tela-plugin:base:/base/plugin-de-base")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "ir para exemplo" }));
    expect(await screen.findByText("tela-plugin:base:/base/example")).toBeTruthy();
    expect(screen.queryByText("tela-plugin:base:/base/plugin-de-base")).toBeNull();

    // The legacy POM branch changes identity and must not retain the plugin
    // screen. This is the transition that previously masked the same-plugin bug.
    fireEvent.click(screen.getByRole("button", { name: "ir para estatísticas" }));
    expect(await screen.findAllByRole("button", { name: "Comprar agora" })).toBeTruthy();
  });

  it("loads a namespaced route on a direct browser request under the router basename", async () => {
    pluginUi.plugins = [baseManifest];
    openBrowserPath("/admin-ui/base/plugin-de-base", "admin");

    expect(await screen.findByText("tela-plugin:base:plugin")).toBeTruthy();
    expect(window.location.pathname).toBe("/admin-ui/base/plugin-de-base");
  });

  it("renders the matching plugin when two manifests declare the same relative route", async () => {
    pluginUi.plugins = [baseManifest, analyticsManifest];
    open("/base/example", "admin");
    expect(await screen.findByText("tela-plugin:base:example")).toBeTruthy();
    cleanup();

    open("/analytics/example", "infer");
    expect(await screen.findByText("tela-plugin:analytics:example")).toBeTruthy();
  });

  it("namespaces competing claims to a native plugin route and keeps the native fallback", async () => {
    const logsPlugin = (code: string) => ({
      code,
      version: "1.0.0",
      manifest: {
        schema: "pom-plugin-ui/v1",
        plugin_code: code,
        menu: [],
        routes: [{ path: "/logs", screen: "logs", profiles: ["admin"] }],
        screens: { logs: { module: "ui/logs.js" } },
        assets: ["ui/logs.js"],
      },
    });
    pluginUi.plugins = [
      logsPlugin("requests"),
      logsPlugin("audit"),
    ];
    open("/requests/logs", "admin");
    expect(await screen.findByText("tela-plugin:requests:logs")).toBeTruthy();
    cleanup();
    open("/audit/logs", "admin");
    expect(await screen.findByText("tela-plugin:audit:logs")).toBeTruthy();
    cleanup();
    open("/logs", "admin");
    expect(await screen.findAllByRole("button", { name: "Comprar agora" })).toBeTruthy();
  });

  it("does not let an invalid plugin path take over a native POM route", async () => {
    pluginUi.plugins = [{
      ...baseManifest,
      manifest: {
        ...baseManifest.manifest,
        menu: [{ ...baseManifest.manifest.menu[0], to: "/../../chat" }],
        routes: [{ ...baseManifest.manifest.routes[0], path: "/../../chat" }],
      },
    }];
    open("/chat", "node");

    expect(await screen.findByText("tela-chat")).toBeTruthy();
  });

  it("leaves the routes nobody restricted alone", async () => {
    pluginUi.plugins = [baseManifest];
    open("/models", "node");
    expect(await screen.findByText("tela-modelos")).toBeTruthy();
  });

  it("the models screen takes the full screen, like Deployments and Dashboard", async () => {
    const { container } = open("/models", "admin");
    expect(await screen.findByText("tela-modelos")).toBeTruthy();
    expect(container.querySelector(".h-full.w-full")).toBeTruthy();
    expect(container.querySelector(".max-w-\\[1400px\\]")).toBeNull();
  });

  it("opens Preferences for the node profile", async () => {
    open("/config/preferences", "node");
    expect(await screen.findByText("tela-preferencias")).toBeTruthy();
  });

  it("has no screen for the deprecated wizard", async () => {
    open("/setup", "admin");
    await screen.findByTestId("admin-sidebar");
    expect(screen.queryByText("tela-wizard")).toBeNull();
  });

  /**
   * The chat renders outside `<Routes>`, so a per-route guard would leave it
   * out. Today it is free, and this test is what warns if the chat exception
   * escapes the guard again when someone restricts it.
   */
  it("passes the chat through the same guard", async () => {
    open("/chat", "node");
    expect(await screen.findByText("tela-chat")).toBeTruthy();
  });

  it("renders a form a password manager can recognize", () => {
    adminSurface.value = "requires-password";
    open("/", "admin");

    const form = screen.getByRole("button", { name: "Entrar" }).closest("form");
    expect(form).not.toBeNull();
    expect(form?.getAttribute("autocomplete")).toBe("on");
    expect(form?.querySelector('input[name="username"]')).toBeNull();
    const password = screen.getByLabelText("Senha de acesso administrativo");
    expect(password.getAttribute("type")).toBe("password");
    expect(password.getAttribute("autocomplete")).toBe("current-password");
    expect(screen.queryByRole("navigation")).toBeNull();

    // The selector shows the flag of the current language and switches through the submenu.
    fireEvent.click(screen.getByRole("button", { name: "Idioma: Português (Brasil)" }));
    fireEvent.click(screen.getByRole("option", { name: "English" }));
    expect(screen.getByRole("button", { name: "Sign in" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Language: English" })).toBeTruthy();
  });

  it("keeps the local screen self-contained and in both languages", () => {
    adminSurface.value = "local-only";
    open("/", "admin");

    expect(screen.getByText(/This administrative interface is available only/)).toBeTruthy();
    expect(screen.getByText(/Esta interface administrativa está disponível apenas/)).toBeTruthy();
    expect(screen.queryByRole("navigation")).toBeNull();
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("opens the compact chat through the path used by the menu bar app", async () => {
    open("/admin-ui/chat", "node");
    const chat = await screen.findByText("tela-chat");
    expect(chat).toBeTruthy();
    expect(chat.parentElement?.className).toContain("h-full");
    expect(chat.parentElement?.className).toContain("min-h-0");
    expect(chat.parentElement?.className).toContain("overflow-hidden");
    expect(screen.queryByTestId("admin-sidebar")).toBeNull();
    expect(screen.queryByTestId("admin-topbar")).toBeNull();
  });
});
