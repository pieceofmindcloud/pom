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
    license_expires_at: null,
    activation_count: null,
    max_activations: null,
    licensed_plugins: [],
    plugin_usage: [],
    activation_candidates: [],
    installed_at: null,
    update_checked_at: null,
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
    description_pt_br: null,
    requires_license: true,
    icon_url: null,
    screenshot_urls: [],
    pricing: { monthly: null, annual: null },
    releases: [release()],
    ...overrides,
  };
}

function activationCandidate(plugin_code: string) {
  return {
    plugin_code,
    display_name: plugin_code,
    release_id: `${plugin_code}-1`,
    version: "1.0.0",
  };
}

const mocks = vi.hoisted(() => ({
  post: vi.fn(),
  del: vi.fn(),
  navigateToPluginPurchase: vi.fn(),
  plugins: {
    data: [] as Array<{
      code: string;
      active: boolean;
      state: string;
      features: string[];
      manage_url: string;
      plugin_version: string | null;
      lease_expires_at: string | null;
      license_expires_at: string | null;
      activation_count: number | null;
      max_activations: number | null;
      licensed_plugins: string[];
      plugin_usage: Array<{ plugin_code: string; activation_count: number; max_activations: number }>;
      activation_candidates: Array<{ plugin_code: string; display_name: string; release_id: string; version: string }>;
      installed_at: string | null;
      update_checked_at: string | null;
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
  node: {
    data: { system: { os: "linux", arch: "x86_64" } },
  },
}));

vi.mock("../lib/api", () => ({ api: { post: mocks.post, del: mocks.del } }));
vi.mock("../lib/queries", () => ({
  usePlugins: () => mocks.plugins,
  usePublicPluginCatalog: () => mocks.catalog,
  useNode: () => mocks.node,
}));
vi.mock("../lib/pluginPurchase", () => ({ navigateToPluginPurchase: mocks.navigateToPluginPurchase }));

function renderPage(initialLocale: "en" | "pt-BR" = "pt-BR") {
  return render(
    <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
      <LocaleProvider initialLocale={initialLocale}>
        <Plugins />
      </LocaleProvider>
    </MemoryRouter>,
  );
}

describe("Plugins", () => {
  afterEach(() => {
    cleanup();
    mocks.post.mockReset();
    mocks.del.mockReset();
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
    mocks.node.data = { system: { os: "linux", arch: "x86_64" } };
    mocks.navigateToPluginPurchase.mockReset();
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

  it("lista vários plugins públicos e mostra os metadados disponíveis", () => {
    const purchasedCode = "alpha_beta";
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
    const screenshotGroup = screen.getByRole("group", { name: "Capturas de tela" });
    expect(within(screenshotGroup).getAllByRole("img", { name: /captura de tela/i })).toHaveLength(4);
    expect(screen.getByText("Sigma")).toBeTruthy();
    expect(screen.getByText("Ativo")).toBeTruthy();
    expect(screen.getByText("1.1.0")).toBeTruthy();

    expect(screen.getByRole("button", { name: "Comprar" })).toBeTruthy();
    expect(screen.queryByRole("link", { name: "Comprar" })).toBeNull();
    expect(screen.queryByRole("link", { name: "Gerenciar" })).toBeNull();
    expect(screen.getByRole("button", { name: "Desativar" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Instalar atualização" })).toBeTruthy();
  });

  it("mostra a release do sistema local em vez da última release global", () => {
    mocks.catalog.data = {
      plugins: [publicPlugin("cross-platform", {
        releases: [
          release({ os: "macos", arch: "aarch64", version: "9.0.0" }),
          release({ os: "linux", arch: "x86_64", version: "1.5.0" }),
        ],
      })],
    };
    renderPage();

    expect(screen.getByText("1.5.0 · stable · linux / x86_64")).toBeTruthy();
    expect(screen.queryByText("9.0.0 · stable · macos / aarch64")).toBeNull();
  });

  it("opens the public plugin store with the selected plugin code", () => {
    const pluginCode = "analytics_plus";
    const entry = publicPlugin(pluginCode, { display_name: "Analytics Plus" });
    mocks.catalog.data = { plugins: [entry] };
    mocks.plugins.data = [pluginStatus(pluginCode)];
    renderPage("en");

    fireEvent.click(screen.getByRole("button", { name: "Buy" }));

    expect(mocks.navigateToPluginPurchase).toHaveBeenCalledWith(pluginCode);
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

  it("shows a localized Portuguese description and falls back to English when it is missing", () => {
    mocks.catalog.data = {
      plugins: [
        publicPlugin("localized", {
          description: "English catalog description",
          description_pt_br: "Descrição traduzida do catálogo",
        }),
        publicPlugin("fallback", { description: "English fallback description" }),
      ],
    };
    mocks.plugins.data = [pluginStatus("localized"), pluginStatus("fallback")];

    renderPage("pt-BR");

    expect(screen.getByText("Descrição traduzida do catálogo")).toBeTruthy();
    expect(screen.getByText("English fallback description")).toBeTruthy();
    expect(screen.queryByText("English catalog description")).toBeNull();
  });

  it("uses the English description when the selected language is English", () => {
    mocks.catalog.data = { plugins: [publicPlugin("localized", {
      description: "English catalog description",
      description_pt_br: "Descrição traduzida do catálogo",
    })] };
    mocks.plugins.data = [pluginStatus("localized")];

    renderPage("en");

    expect(screen.getByText("English catalog description")).toBeTruthy();
    expect(screen.queryByText("Descrição traduzida do catálogo")).toBeNull();
  });

  it("uses authenticated per-plugin activation usage instead of public catalog aggregates", () => {
    mocks.catalog.data = {
      plugins: [
        publicPlugin("partial", { activation_usage: { active_machines: 99, max_machines: 100 } }),
        publicPlugin("full", { activation_usage: { active_machines: 0, max_machines: 1 } }),
      ],
    };
    mocks.plugins.data = [
      pluginStatus("partial", {
        active: true,
        state: "active",
        licensed_plugins: ["partial", "full"],
        plugin_usage: [
          { plugin_code: "partial", activation_count: 2, max_activations: 5 },
          { plugin_code: "full", activation_count: 5, max_activations: 5 },
        ],
        activation_candidates: [activationCandidate("full")],
        activation_count: 2,
        max_activations: 5,
      }),
      pluginStatus("full", {
        licensed_plugins: ["partial", "full"],
        plugin_usage: [
          { plugin_code: "partial", activation_count: 2, max_activations: 5 },
          { plugin_code: "full", activation_count: 5, max_activations: 5 },
        ],
        activation_candidates: [activationCandidate("full")],
        activation_count: 5,
        max_activations: 5,
      }),
    ];
    renderPage();

    const partial = screen.getByRole("heading", { name: "partial" }).closest(".panel");
    const full = screen.getByRole("heading", { name: "full" }).closest(".panel");
    expect(within(partial as HTMLElement).getByText("2 de 5 ativações usadas")).toBeTruthy();
    expect(within(partial as HTMLElement).getByText("Parcialmente utilizada")).toBeTruthy();
    expect(within(partial as HTMLElement).getByText("Ativo")).toBeTruthy();
    expect(within(full as HTMLElement).getByText("5 de 5 ativações usadas")).toBeTruthy();
    expect(within(full as HTMLElement).getByText("Limite atingido")).toBeTruthy();
  });

  it("uses a licensed plugin's authenticated status for its inactive activation candidate", () => {
    mocks.catalog.data = { plugins: [publicPlugin("analytics_plus")] };
    mocks.plugins.data = [
      pluginStatus("enterprise", {
        active: true,
        state: "active",
        licensed_plugins: ["enterprise", "analytics_plus"],
        plugin_usage: [
          { plugin_code: "enterprise", activation_count: 1, max_activations: 5 },
          { plugin_code: "analytics_plus", activation_count: 2, max_activations: 5 },
        ],
        activation_candidates: [],
        license_expires_at: "2099-11-30T12:00:00.000Z",
        activation_count: 2,
        max_activations: 5,
      }),
      pluginStatus("analytics_plus"),
    ];
    renderPage();

    const card = screen.getByRole("heading", { name: "analytics_plus" }).closest(".panel");
    const expiry = new Intl.DateTimeFormat("pt-BR", { dateStyle: "medium", timeStyle: "short" })
      .format(new Date("2099-11-30T12:00:00.000Z"));
    expect(within(card as HTMLElement).getByText("2 de 5 ativações usadas")).toBeTruthy();
    expect(within(card as HTMLElement).getByText("Licença válida até")).toBeTruthy();
    expect(within(card as HTMLElement).getByText(expiry)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Comprar" })).toBeNull();
    expect(screen.getByLabelText("Chave de licença")).toBeTruthy();
  });

  it("shows the activation form for an inactive public plugin that requires a license", () => {
    mocks.catalog.data = { plugins: [publicPlugin("candidate")] };
    mocks.plugins.data = [
      pluginStatus("enterprise", {
        active: true,
        state: "active",
        licensed_plugins: ["enterprise"],
        activation_candidates: [activationCandidate("candidate")],
      }),
      pluginStatus("candidate"),
    ];
    renderPage();

    expect(screen.getByLabelText("Chave de licença")).toBeTruthy();
  });

  it("keeps activation visible for an inactive plugin outside the current license context", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("not-eligible")] };
    mocks.plugins.data = [
      pluginStatus("enterprise", {
        active: true,
        state: "active",
        licensed_plugins: ["enterprise"],
        activation_candidates: [],
      }),
      pluginStatus("not-eligible"),
    ];
    mocks.post.mockRejectedValue(new Error("feature_not_entitled"));
    renderPage();

    const input = screen.getByLabelText("Chave de licença");
    fireEvent.change(input, { target: { value: "TEST-ONLY-KEY" } });
    fireEvent.click(screen.getByRole("button", { name: "Ativar licença" }));

    expect(await screen.findByText("Não foi possível ativar a licença.")).toBeTruthy();
    expect(screen.queryByText("TEST-ONLY-KEY")).toBeNull();
    expect(screen.getByRole("button", { name: "Comprar" })).toBeTruthy();
  });

  it("shows activation for a licensed public plugin with no local status", () => {
    mocks.catalog.data = { plugins: [publicPlugin("future_plugin")] };
    mocks.plugins.data = [];
    renderPage();

    expect(screen.getByLabelText("Chave de licença")).toBeTruthy();
  });

  it("shows activation when none of the public licensed plugins is active", () => {
    mocks.catalog.data = { plugins: [publicPlugin("base"), publicPlugin("future_plugin")] };
    mocks.plugins.data = [pluginStatus("base"), pluginStatus("future_plugin")];
    renderPage();

    expect(screen.getByLabelText("Chave de licença")).toBeTruthy();
  });

  it("keeps activation available for expired and revoked plugins beside another active plugin", () => {
    mocks.catalog.data = { plugins: [publicPlugin("expired_plugin"), publicPlugin("revoked_plugin")] };
    mocks.plugins.data = [
      pluginStatus("enterprise", {
        active: true,
        state: "active",
        licensed_plugins: ["enterprise"],
      }),
      pluginStatus("expired_plugin", { state: "expired" }),
      pluginStatus("revoked_plugin", { state: "revoked" }),
    ];
    renderPage();

    expect(screen.getByLabelText("Chave de licença")).toBeTruthy();
  });

  it("shows license expiration separately from the lease, install and update-check times", () => {
    mocks.catalog.data = { plugins: [publicPlugin("alpha")] };
    mocks.plugins.data = [pluginStatus("alpha", {
      active: true,
      state: "active",
      // Deliberately different from license_expires_at: the lease is a
      // short technical renewal deadline the heartbeat rolls forward every
      // few days, never the purchased license's own validity.
      lease_expires_at: "2026-09-28T12:00:00.000Z",
      license_expires_at: "2099-11-30T12:00:00.000Z",
      installed_at: "2026-09-20T10:15:00.000Z",
      update_checked_at: "2026-09-21T14:30:00.000Z",
      available_update: { release_id: "alpha-2", version: "2.0.0", changelog: null },
    })];
    renderPage("pt-BR");

    const card = screen.getByRole("heading", { name: "alpha" }).closest(".panel");
    const updateDate = new Intl.DateTimeFormat("pt-BR", { dateStyle: "medium", timeStyle: "short" })
      .format(new Date("2026-09-21T14:30:00.000Z"));
    const installDate = new Intl.DateTimeFormat("pt-BR", { dateStyle: "medium", timeStyle: "short" })
      .format(new Date("2026-09-20T10:15:00.000Z"));
    const expirationDate = new Intl.DateTimeFormat("pt-BR", { dateStyle: "medium", timeStyle: "short" })
      .format(new Date("2099-11-30T12:00:00.000Z"));
    const leaseDate = new Intl.DateTimeFormat("pt-BR", { dateStyle: "medium", timeStyle: "short" })
      .format(new Date("2026-09-28T12:00:00.000Z"));

    expect(within(card as HTMLElement).getByText("Última verificação de atualização")).toBeTruthy();
    expect(within(card as HTMLElement).getByText(updateDate)).toBeTruthy();
    expect(within(card as HTMLElement).getByText("Última instalação")).toBeTruthy();
    expect(within(card as HTMLElement).getByText(installDate)).toBeTruthy();
    expect(within(card as HTMLElement).getByText("Licença válida até")).toBeTruthy();
    expect(within(card as HTMLElement).getByText(expirationDate)).toBeTruthy();
    expect(within(card as HTMLElement).queryByText(leaseDate)).toBeNull();
  });

  it("hides license activation when every public licensed plugin is active", () => {
    mocks.catalog.data = { plugins: [publicPlugin("active-plugin"), publicPlugin("another-active")] };
    mocks.plugins.data = [
      pluginStatus("active-plugin", { active: true, state: "active" }),
      pluginStatus("another-active", { active: true, state: "active" }),
    ];
    renderPage();

    expect(screen.queryByLabelText("Chave de licença")).toBeNull();
    expect(screen.getByRole("heading", { name: "active-plugin" })).toBeTruthy();
  });

  it("does not show license activation when the public catalog has no licensed plugins", () => {
    mocks.catalog.data = { plugins: [publicPlugin("free-plugin", { requires_license: false })] };
    mocks.plugins.data = [pluginStatus("free-plugin")];
    renderPage();

    expect(screen.queryByLabelText("Chave de licença")).toBeNull();
  });

  it("requires confirmation before deactivating a plugin", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("alpha")] };
    mocks.plugins.data = [pluginStatus("alpha", { active: true, state: "active" })];
    mocks.post.mockResolvedValue([]);
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Desativar" }));
    const dialog = screen.getByRole("dialog", { name: "Desativar alpha?" });
    expect(mocks.post).not.toHaveBeenCalled();

    fireEvent.click(within(dialog).getByRole("button", { name: "Desativar" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/plugins/alpha/deactivate"));
  });

  it("opens screenshots in an accessible full-screen dialog and restores focus after Escape", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("alpha", {
      screenshot_urls: ["https://license.pieceofmind.cloud/v1/plugins/alpha/images/screenshot_1"],
    })] };
    mocks.plugins.data = [pluginStatus("alpha")];
    renderPage();

    const opener = screen.getByRole("button", { name: "Abrir captura de tela 1 de alpha" });
    fireEvent.click(opener);

    const dialog = screen.getByRole("dialog", { name: "Capturas de tela de alpha" }) as HTMLDialogElement;
    const close = screen.getByRole("button", { name: "Fechar captura de tela" });
    expect(dialog.open).toBe(true);
    expect(dialog.className).toContain("max-w-none");
    expect(document.activeElement).toBe(close);

    fireEvent.keyDown(document, { key: "Escape" });

    await waitFor(() => expect(dialog.open).toBe(false));
    expect(document.activeElement).toBe(opener);
  });

  it("keeps purchase-button styling and navigates without opening a checkout session", () => {
    const entry = publicPlugin("alpha");
    mocks.catalog.data = { plugins: [entry] };
    mocks.plugins.data = [pluginStatus("alpha")];
    renderPage();

    const buy = screen.getByRole("button", { name: "Comprar" });
    expect(buy.className).toContain("plugin-buy-button");
    expect(buy.className).toContain("focus-visible:ring-2");
    fireEvent.click(buy);

    expect(mocks.navigateToPluginPurchase).toHaveBeenCalledWith("alpha");
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

  it("registers and removes a public GitHub source by plugin code", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("base")] };
    mocks.plugins.data = [pluginStatus("base", {
      github_repository: "https://github.com/example/base-plugin",
    })];
    mocks.post.mockResolvedValue({});
    mocks.del.mockResolvedValue({});
    renderPage();

    const repository = screen.getByLabelText("URL do repositório público do GitHub");
    fireEvent.change(repository, { target: { value: "https://github.com/example/another-plugin" } });
    fireEvent.click(screen.getByRole("button", { name: "Salvar repositório" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/base/github-source",
      { repository_url: "https://github.com/example/another-plugin" },
    ));

    fireEvent.click(screen.getByRole("button", { name: "Remover origem" }));
    await waitFor(() => expect(mocks.del).toHaveBeenCalledWith("/api/ui/plugins/base/github-source"));
  });

  it("shows source registration failures without hiding the existing release status", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("base")] };
    mocks.plugins.data = [pluginStatus("base", {
      github_repository: "https://github.com/example/base-plugin",
      github_update: { release_id: "v1.2.0", version: "1.2.0", changelog: null, downloaded: true },
    })];
    mocks.post.mockRejectedValue(new Error("private repository"));
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Verificar atualizações" }));

    expect(await screen.findByText(/Não foi possível cadastrar ou atualizar/)).toBeTruthy();
    expect(screen.getByText("Atualização verificada do GitHub baixada: 1.2.0")).toBeTruthy();
  });

  it("applies a downloaded GitHub update only through the explicit action", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("base")] };
    mocks.plugins.data = [pluginStatus("base", {
      active: true,
      state: "active",
      github_repository: "https://github.com/example/base-plugin",
      github_update: { release_id: "v1.2.0", version: "1.2.0", changelog: "Update", downloaded: true },
    })];
    mocks.post.mockResolvedValue({});
    renderPage();

    expect(screen.getByText("Atualização verificada do GitHub baixada: 1.2.0")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Instalar atualização baixada" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/base/github-source/apply",
      { release_id: "v1.2.0" },
    ));
  });

  it("edits typed preference defaults and submits only to the local preferences route", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("base")] };
    mocks.plugins.data = [pluginStatus("base", {
      active: true,
      state: "active",
      preferences_schema: {
        features: [{
          key: "include_headers",
          label: "Incluir cabeçalhos",
          default: false,
          description: "Opção de exemplo",
        }],
        storage_directory: { label: "Pasta de dados", default: null },
      },
      preferences: { features: { include_headers: false }, storage_directory: null },
    })];
    mocks.post.mockResolvedValue({});
    renderPage();

    fireEvent.click(screen.getByLabelText("Incluir cabeçalhos"));
    fireEvent.change(screen.getByLabelText("Pasta de dados"), {
      target: { value: "/tmp/plugin-data" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Salvar preferências" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/base/preferences",
      { preferences: { features: { include_headers: true }, storage_directory: "/tmp/plugin-data" } },
    ));
    expect(mocks.post).not.toHaveBeenCalledWith("/api/ui/plugins/activate", expect.anything());
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
    const dialog = screen.getByRole("dialog", { name: "Desativar sigma?" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Desativar" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/plugins/sigma/deactivate"));
    expect(mocks.post).not.toHaveBeenCalledWith(expect.stringContaining("pom-update"));
  });
});
