// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
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
  PluginScreen: ({ resolved }: { resolved: { code: string; route: { screen: string } } }) => (
    <div>{`tela-plugin:${resolved.code}:${resolved.route.screen}`}</div>
  ),
}));
vi.mock("./routes/WholeModels", () => ({ WholeModels: () => <div>tela-modelos</div> }));
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

describe("guarda de perfil por rota", () => {
  afterEach(() => {
    cleanup();
    adminSurface.value = "available";
    pluginUi.plugins = [];
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
    code: "enterprise",
    version: "1.0.0",
    manifest: {
      schema: "pom-plugin-ui/v1",
      plugin_code: "enterprise",
      menu: [],
      routes: [
        { path: "/logs", screen: "logs", profiles, full_bleed: true },
        { path: "/estatisticas", screen: "stats", profiles },
      ],
      screens: { logs: { module: "ui/logs.js" }, stats: { module: "ui/stats.js" } },
      assets: ["ui/logs.js", "ui/stats.js"],
    },
  });

  it("shows the advertising fallback when no plugin provides /logs or /estatisticas", async () => {
    open("/logs", "admin");
    expect(await screen.findByRole("link", { name: "Comprar e instalar" })).toBeTruthy();
    expect(screen.queryByText(/^tela-plugin:/)).toBeNull();
    cleanup();
    open("/estatisticas", "admin");
    expect(await screen.findByRole("link", { name: "Comprar e instalar" })).toBeTruthy();
  });

  it("renders the plugin-provided screen instead of the fallback, per manifest profile", async () => {
    pluginUi.plugins = [enterpriseManifest(["admin", "node"])];
    open("/logs", "admin");
    expect(await screen.findByText("tela-plugin:enterprise:logs")).toBeTruthy();
    expect(screen.queryByRole("link", { name: "Comprar e instalar" })).toBeNull();
    cleanup();
    open("/estatisticas", "node");
    expect(await screen.findByText("tela-plugin:enterprise:stats")).toBeTruthy();
    cleanup();
    // infer não está nos perfis do manifesto: recusa, e não cai na propaganda.
    open("/logs", "infer");
    expect(await screen.findByText("Tela restrita a outro perfil")).toBeTruthy();
    expect(screen.queryByText(/^tela-plugin:/)).toBeNull();
  });

  it("não mexe nas rotas que ninguém restringiu", async () => {
    open("/modelos", "node");
    expect(await screen.findByText("tela-modelos")).toBeTruthy();
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

    const form = screen.getByRole("button", { name: "Abrir interface" }).closest("form");
    expect(form).not.toBeNull();
    expect(form?.getAttribute("autocomplete")).toBe("on");
    expect(form?.querySelector('input[name="username"]')).toBeNull();
    const password = screen.getByLabelText("Senha de acesso administrativo");
    expect(password.getAttribute("type")).toBe("password");
    expect(password.getAttribute("autocomplete")).toBe("current-password");
    expect(screen.queryByRole("navigation")).toBeNull();
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
