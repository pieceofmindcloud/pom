// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { Plugins } from "./Plugins";

function pluginStatus(code: string, overrides: Record<string, unknown> = {}) {
  return {
    code,
    active: false,
    state: "needs_activation",
    features: [],
    manage_url: `https://pieceofmind.cloud/plugins/index.html?plugin=${encodeURIComponent(code)}`,
    plugin_version: null,
    lease_expires_at: null,
    available_update: null,
    ...overrides,
  };
}

function release(overrides: Record<string, unknown> = {}) {
  return {
    os: "linux",
    arch: "x86_64",
    channel: "stable",
    version: "1.2.0",
    published_at: "2026-09-21T14:33:00.000Z",
    ...overrides,
  };
}

function publicPlugin(code: string, overrides: Record<string, unknown> = {}) {
  return {
    code,
    display_name: code,
    description: `${code} description`,
    requires_license: true,
    icon_url: null,
    screenshot_urls: [],
    pricing: { monthly: null, annual: null },
    releases: [release()],
    ...overrides,
  };
}

const mocks = vi.hoisted(() => ({
  post: vi.fn(),
  plugins: {
    data: [] as Array<{
      code: string;
      active: boolean;
      state: string;
      features: string[];
      manage_url: string;
      plugin_version: string | null;
      lease_expires_at: string | null;
      available_update: { release_id: string; version: string; changelog: string | null } | null;
    }>,
    isLoading: false,
    isError: false,
    error: null as Error | null,
    refetch: vi.fn(),
  },
  catalog: {
    data: { plugins: [] as Array<Record<string, unknown>> } as { plugins: Array<Record<string, unknown>> } | undefined,
    isLoading: false,
    isError: false,
    error: null as Error | null,
    refetch: vi.fn(),
  },
}));

vi.mock("../lib/api", () => ({ api: { post: mocks.post } }));
vi.mock("../lib/queries", () => ({
  usePlugins: () => mocks.plugins,
  usePublicPluginCatalog: () => mocks.catalog,
}));

function renderPage() {
  return render(
    <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
      <LocaleProvider initialLocale="pt-BR">
        <Plugins />
      </LocaleProvider>
    </MemoryRouter>,
  );
}

describe("Plugins", () => {
  afterEach(() => {
    cleanup();
    mocks.post.mockReset();
    mocks.plugins.data = [];
    mocks.plugins.isLoading = false;
    mocks.plugins.isError = false;
    mocks.plugins.error = null;
    mocks.plugins.refetch.mockReset();
    mocks.catalog.data = { plugins: [] };
    mocks.catalog.isLoading = false;
    mocks.catalog.isError = false;
    mocks.catalog.error = null;
    mocks.catalog.refetch.mockReset();
    localStorage.clear();
  });

  it("envia a license key uma vez, limpa o campo e nunca a grava no storage", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("alpha")] };
    mocks.plugins.data = [pluginStatus("alpha")];
    mocks.post.mockResolvedValue({ activated: ["alpha"] });
    renderPage();

    const input = screen.getByLabelText("Chave de licença");
    fireEvent.change(input, { target: { value: "PM-secret" } });
    fireEvent.click(screen.getByRole("button", { name: "Ativar licença" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/activate",
      { license_key: "PM-secret" },
    ));
    expect((input as HTMLInputElement).value).toBe("");
    expect(localStorage.getItem("PM-secret")).toBeNull();
    expect(mocks.plugins.refetch).toHaveBeenCalled();
  });

  it("explica uma recusa sem exibir a chave na mensagem", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("alpha")] };
    mocks.plugins.data = [pluginStatus("alpha")];
    mocks.post.mockRejectedValue(new Error("licença inválida"));
    renderPage();

    fireEvent.change(screen.getByLabelText("Chave de licença"), {
      target: { value: "PM-invalid" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Ativar licença" }));

    expect(await screen.findByText("Não foi possível ativar a licença.")).toBeTruthy();
    expect(screen.queryByText("PM-invalid")).toBeNull();
  });

  it("lista vários plugins públicos, mostra metadados e compra com o código codificado", () => {
    const purchasedCode = "alpha&beta";
    mocks.catalog.data = {
      plugins: [
        publicPlugin(purchasedCode, {
          display_name: "Alpha & Beta",
          description: "Descrição pública",
          icon_url: "https://license.pieceofmind.cloud/v1/plugins/alpha/images/icon",
          screenshot_urls: Array.from({ length: 5 }, (_, index) =>
            `https://license.pieceofmind.cloud/v1/plugins/alpha/images/screenshot_${index + 1}`),
          pricing: { monthly: { amount: 1499, currency: "brl", interval: "month" }, annual: null },
        }),
        publicPlugin("sigma", {
          display_name: "Sigma",
          description: "Outro plugin",
          icon_url: null,
          screenshot_urls: [],
          releases: [release({ version: "2.0.0", os: "macos", arch: "aarch64" })],
        }),
      ],
    };
    mocks.plugins.data = [
      pluginStatus(purchasedCode),
      pluginStatus("sigma", {
        active: true,
        state: "active",
        features: ["audit"],
        manage_url: "https://pieceofmind.cloud/plugins/manage/sigma",
        plugin_version: "1.1.0",
        available_update: { release_id: "sigma-2", version: "1.2.0", changelog: null },
      }),
    ];
    renderPage();

    expect(screen.getByText("Alpha & Beta")).toBeTruthy();
    expect(screen.getByText("Descrição pública")).toBeTruthy();
    expect(screen.getByText("1.2.0 · stable · linux / x86_64")).toBeTruthy();
    expect(screen.getByText("R$ 14,99/mês")).toBeTruthy();
    expect(screen.getByAltText("Alpha & Beta")).toBeTruthy();
    expect(screen.getAllByRole("img", { name: /captura de tela/i })).toHaveLength(4);
    expect(screen.getByText("Sigma")).toBeTruthy();
    expect(screen.getByText("Ativo")).toBeTruthy();
    expect(screen.getByText("1.1.0")).toBeTruthy();

    const buyLink = screen.getByRole("link", { name: "Comprar" });
    expect(buyLink.getAttribute("href")).toBe(
      "https://pieceofmind.cloud/plugins/index.html?plugin=alpha%26beta",
    );
    expect(screen.getByRole("link", { name: "Gerenciar" }).getAttribute("href"))
      .toBe("https://pieceofmind.cloud/plugins/manage/sigma");
    expect(screen.getByRole("button", { name: "Desativar" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Instalar atualização" })).toBeTruthy();
  });

  it("mantém plugins locais ativos mesmo se não vierem no catálogo público", () => {
    mocks.catalog.data = { plugins: [publicPlugin("public-plugin")] };
    mocks.plugins.data = [pluginStatus("local-active", { active: true, state: "active" })];
    renderPage();

    expect(screen.getByRole("heading", { name: "local-active" })).toBeTruthy();
    expect(screen.getByRole("heading", { name: "public-plugin" })).toBeTruthy();
    expect(screen.getAllByRole("button", { name: "Desativar" })).toHaveLength(1);
  });

  it("mostra estado vazio quando não há plugin público com release válida", () => {
    mocks.catalog.data = { plugins: [] };
    mocks.plugins.data = [pluginStatus("local-inactive")];
    renderPage();

    expect(screen.getByText("Nenhum plugin disponível no momento.")).toBeTruthy();
    expect(screen.queryByText("local-inactive")).toBeNull();
  });

  it("mostra estado de carregamento enquanto o catálogo público está em andamento", () => {
    mocks.catalog.data = undefined;
    mocks.catalog.isLoading = true;
    mocks.plugins.data = [];
    renderPage();

    expect(screen.getByRole("status", { name: "Carregando o catálogo público de plugins" })).toBeTruthy();
    expect(screen.queryByText("Nenhum plugin disponível no momento.")).toBeNull();
  });

  it("espera o estado local de licença antes de anunciar catálogo vazio", () => {
    mocks.catalog.data = { plugins: [] };
    mocks.plugins.data = [];
    mocks.plugins.isLoading = true;
    renderPage();

    expect(screen.queryByText("Nenhum plugin disponível no momento.")).toBeNull();
  });

  it("informa erro do catálogo e preserva os plugins locais ativos", () => {
    mocks.catalog.data = undefined;
    mocks.catalog.isError = true;
    mocks.catalog.error = new Error("catalog offline");
    mocks.plugins.data = [pluginStatus("local-active", { active: true, state: "active" })];
    renderPage();

    expect(screen.getByText("Não foi possível carregar o catálogo público.")).toBeTruthy();
    expect(screen.getByRole("heading", { name: "local-active" })).toBeTruthy();
  });

  it("não desenha imagens quando o catálogo não as fornece", () => {
    mocks.catalog.data = { plugins: [publicPlugin("no-images")] };
    mocks.plugins.data = [pluginStatus("no-images")];
    renderPage();

    const card = screen.getByRole("heading", { name: "no-images" }).closest(".panel");
    expect(card).toBeTruthy();
    expect(within(card as HTMLElement).queryAllByRole("img")).toHaveLength(0);
    expect(within(card as HTMLElement).getByText("no-images description")).toBeTruthy();
  });

  it("recarrega o catálogo após erro", () => {
    mocks.catalog.data = undefined;
    mocks.catalog.isError = true;
    mocks.catalog.error = new Error("catalog offline");
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Tentar de novo" }));

    expect(mocks.catalog.refetch).toHaveBeenCalled();
  });

  it("mostra e aplica a atualização do plugin ativo", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("sigma")] };
    mocks.plugins.data = [pluginStatus("sigma", {
      active: true,
      state: "active",
      features: ["audit"],
      plugin_version: "1.1.0",
      available_update: { release_id: "sigma-2", version: "1.2.0", changelog: null },
    })];
    mocks.post.mockResolvedValue({});
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Instalar atualização" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/sigma/update/apply",
      { release_id: "sigma-2" },
    ));
  });

  it("desativa um plugin sem chamar rota de atualização do pom_core", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("sigma")] };
    mocks.plugins.data = [pluginStatus("sigma", {
      active: true,
      state: "active",
      features: ["audit"],
      plugin_version: "1.1.0",
    })];
    mocks.post.mockResolvedValue({});
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Desativar" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/plugins/sigma/deactivate"));
    expect(mocks.post).not.toHaveBeenCalledWith(expect.stringContaining("pom-update"));
  });
});
