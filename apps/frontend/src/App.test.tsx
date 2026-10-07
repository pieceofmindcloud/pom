// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { BrowserRouter, MemoryRouter, useNavigate } from "react-router-dom";
import { useState } from "react";
import { LocaleProvider } from "./i18n/provider";
import { ProfileProvider, type Profile } from "./lib/profile";

const adminSurface = vi.hoisted(() => ({
  value: "available" as "setup" | "available" | "requires-password" | "local-only",
}));
const pluginUi = vi.hoisted(() => ({ plugins: [] as unknown[] }));

/**
 * Esconder o item do menu não é bloquear: o endereço continua digitável, e um
 * favorito antigo entra direto. Este teste é o que prova que a rota fechada
 * **não renderiza a tela**, e que a aberta continua renderizando.
 *
 * Por isso ele monta o `App` de verdade, com as telas trocadas por marcadores:
 * o que está sob prova é o guarda, não o conteúdo de cada tela.
 */
vi.mock("./lib/queries", () => ({
  useAdminSurface: () => ({ data: adminSurface.value, isLoading: false, isError: false }),
  useAdminAccess: () => ({ data: { default_password: false } }),
  useNetworkChat: () => ({ data: { notices: [] } }),
  usePluginUi: () => ({ data: { plugins: pluginUi.plugins }, isLoading: false, isError: false }),
  // Nó completo o bastante para a barra superior e o rodapé do menu
  // renderizarem: um `data: null` derruba o `NodeChip` no `node.data!` e o
  // teste passa a medir o `ErrorBoundary` em vez do guarda.
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
vi.mock("./components/Topbar", () => ({ Topbar: () => <header data-testid="admin-topbar" /> }));
vi.mock("./components/Logo", () => ({
  LogoFull: () => <div aria-hidden="true" />,
  LogoMark: () => <div aria-hidden="true" />,
}));

// `vi.mock` sobe para o topo do arquivo, antes de qualquer `const`, então cada
// marcador é escrito à mão em vez de sair de um auxiliar compartilhado.
vi.mock("./routes/DeployModel", () => ({ DeployModel: () => <div>tela-implantar</div> }));
vi.mock("./routes/Deployments", () => ({ Deployments: () => <div>tela-implantacoes</div> }));
vi.mock("./components/PluginScreen", () => ({
  PluginScreen: ({ resolved }: { resolved: { code: string; route: { path: string; screen: string } } }) => {
    // Simula a tela real mantendo o componente carregado enquanto o pai troca
    // a rota. O App precisa remontá-la por identidade de rota para não deixar
    // a tela anterior presa quando dois destinos compartilham o mesmo módulo.
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
    // Mesmas flags que o `main.tsx` liga: sem elas o teste roda num modo de
    // roteador que a aplicação não usa, e o console enche de aviso de futuro.
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
      <button type="button" onClick={() => navigate("/estatisticas")}>
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

describe("guarda de perfil por rota", () => {
  afterEach(() => {
    cleanup();
    adminSurface.value = "available";
    pluginUi.plugins = [];
    window.history.replaceState({}, "", "/");
  });

  it("renderiza Plugins no contêiner full-bleed", async () => {
    open("/plugins", "admin");

    const content = await screen.findByText("tela-plugins");
    expect(content.closest(".h-full.w-full")).toBeTruthy();
  });

  const fechadas = [
    ["/implantar", "tela-implantar"],
    ["/implantacoes", "tela-implantacoes"],
  ] as const;

  for (const [path, marker] of fechadas) {
    it(`abre ${path} para o administrador`, async () => {
      open(path, "admin");
      expect(await screen.findByText(marker)).toBeTruthy();
      expect(screen.queryByText("Tela restrita a outro perfil")).toBeNull();
    });

    it(`barra ${path} para o perfil do nó`, async () => {
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
        { id: "stats", label: { en: "Statistics" }, to: "/estatisticas", profiles },
      ],
      routes: [
        { path: "/logs", screen: "logs", profiles, full_bleed: true },
        { path: "/estatisticas", screen: "stats", profiles },
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

  it("shows the advertising fallback when no plugin provides /logs or /estatisticas", async () => {
    open("/logs", "admin");
    expect(await screen.findAllByRole("button", { name: "Comprar agora" })).toBeTruthy();
    expect(screen.queryByText(/^tela-plugin:/)).toBeNull();
    cleanup();
    open("/estatisticas", "admin");
    expect(await screen.findAllByRole("button", { name: "Comprar agora" })).toBeTruthy();
  });

  it("renders the plugin-provided screen instead of the fallback, per manifest profile", async () => {
    pluginUi.plugins = [enterpriseManifest(["admin", "node"])];
    open("/logs", "admin");
    expect(await screen.findByText("tela-plugin:logs:logs")).toBeTruthy();
    cleanup();
    open("/estatisticas", "node");
    expect(await screen.findByText("tela-plugin:logs:stats")).toBeTruthy();
    cleanup();
    // infer não está nos perfis do manifesto: recusa, e não cai na propaganda.
    open("/logs", "infer");
    expect(await screen.findByText("Tela restrita a outro perfil")).toBeTruthy();
    expect(screen.queryByText(/^tela-plugin:/)).toBeNull();
  });

  it("opens the Base manifest route after applying the plugin namespace", async () => {
    pluginUi.plugins = [baseManifest];
    open("/base/plugin-de-base", "admin");

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

  it("keeps a plugin route closed for profiles excluded by its manifest", async () => {
    pluginUi.plugins = [baseManifest];
    open("/base/plugin-de-base", "node");

    expect(await screen.findByText("Tela restrita a outro perfil")).toBeTruthy();
    expect(screen.queryByText(/^tela-plugin:/)).toBeNull();
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

  it("não mexe nas rotas que ninguém restringiu", async () => {
    pluginUi.plugins = [baseManifest];
    open("/chat", "node");
    expect(await screen.findByText("tela-chat")).toBeTruthy();
  });

  it("a tela de modelos ocupa a tela cheia, como Implantações e Painel", async () => {
    const { container } = open("/modelos", "admin");
    expect(await screen.findByText("tela-modelos")).toBeTruthy();
    expect(container.querySelector(".h-full.w-full")).toBeTruthy();
    expect(container.querySelector(".max-w-\\[1400px\\]")).toBeNull();
  });

  it("abre Preferências para o perfil do nó", async () => {
    open("/config/preferencias", "node");
    expect(await screen.findByText("tela-preferencias")).toBeTruthy();
  });

  it("mantém o wizard acessível depois do primeiro acesso", async () => {
    open("/setup", "admin");
    expect(await screen.findByText("tela-wizard")).toBeTruthy();
  });

  /**
   * O chat renderiza fora do `<Routes>`, então um guarda por rota o deixaria
   * de fora. Hoje ele é livre, e este teste é o que avisa se a exceção do chat
   * voltar a escapar do guarda quando alguém o restringir.
   */
  it("passa o chat pelo mesmo guarda", async () => {
    open("/chat", "node");
    expect(await screen.findByText("tela-chat")).toBeTruthy();
  });

  it("renderiza um formulário reconhecível pelo gestor de senhas", () => {
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

    // As bandeiras trocam o idioma da própria tela de login.
    expect(screen.getByRole("button", { name: "Português (Brasil)" }).getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(screen.getByRole("button", { name: "English" }));
    expect(screen.getByRole("button", { name: "Sign in" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "English" }).getAttribute("aria-pressed")).toBe("true");
  });

  it("mantém a tela local autônoma e nas duas línguas", () => {
    adminSurface.value = "local-only";
    open("/", "admin");

    expect(screen.getByText(/This administrative interface is available only/)).toBeTruthy();
    expect(screen.getByText(/Esta interface administrativa está disponível apenas/)).toBeTruthy();
    expect(screen.queryByRole("navigation")).toBeNull();
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("abre o chat compacto pelo caminho usado pelo app da barra de menus", async () => {
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
