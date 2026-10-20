// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { Plugins } from "./Plugins";
import { ApiError } from "../lib/api";

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
    installation_error: null,
    public_install: false,
    development_install: false,
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
    github_repository: null,
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
  queryClient: { invalidateQueries: vi.fn() },
  githubRepositories: {
    data: [] as Array<Record<string, unknown>>,
    isLoading: false,
    isError: false,
    error: null as Error | null,
    refetch: vi.fn(),
  },
  developmentSources: {
    data: undefined as { mount_available: boolean; sources: Array<Record<string, unknown>> } | undefined,
    isLoading: false,
    isError: false,
    refetch: vi.fn(),
  },
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
      installation_error?: string | null;
      public_install?: boolean;
      development_install?: boolean;
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
  catalogQueries: [] as unknown[],
}));

vi.mock("@tanstack/react-query", () => ({ useQueryClient: () => mocks.queryClient }));
vi.mock("../lib/api", () => ({
  api: { post: mocks.post, del: mocks.del },
  ApiError: class extends Error {
    constructor(readonly status: number, message: string, readonly body?: unknown) { super(message); }
  },
}));
vi.mock("../lib/queries", () => ({
  keys: { pluginUi: ["plugin-ui"] },
  usePlugins: () => mocks.plugins,
  useGithubRepositories: () => mocks.githubRepositories,
  useDevelopmentPluginSources: () => mocks.developmentSources,
  usePublicPluginCatalog: (query?: unknown) => {
    mocks.catalogQueries.push(query);
    return mocks.catalog;
  },
  usePublicPlugin: () => ({ data: null, isLoading: false }),
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
    mocks.githubRepositories.data = [];
    mocks.githubRepositories.isLoading = false;
    mocks.githubRepositories.isError = false;
    mocks.githubRepositories.error = null;
    mocks.githubRepositories.refetch.mockReset();
    mocks.developmentSources.data = undefined;
    mocks.developmentSources.isLoading = false;
    mocks.developmentSources.isError = false;
    mocks.developmentSources.refetch.mockReset();
    mocks.catalog.data = { plugins: [] };
    mocks.catalog.isLoading = false;
    mocks.catalog.isError = false;
    mocks.catalog.error = null;
    mocks.catalog.refetch.mockReset();
    mocks.catalogQueries.length = 0;
    mocks.node.data = { system: { os: "linux", arch: "x86_64" } };
    mocks.navigateToPluginPurchase.mockReset();
    mocks.queryClient.invalidateQueries.mockReset();
    localStorage.clear();
  });

  it("sends the license key once, clears the field and never writes it to storage", async () => {
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
    await waitFor(() => expect(mocks.queryClient.invalidateQueries).toHaveBeenCalledWith({
      queryKey: ["plugin-ui"],
    }));
  });

  it("explains a refusal without displaying the key in the message", async () => {
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

  it("shows an installation failure without presenting the plugin as ready", () => {
    mocks.catalog.data = { plugins: [publicPlugin("logs", { display_name: "Enterprise" })] };
    mocks.plugins.data = [pluginStatus("logs", {
      state: "active",
      licensed_plugins: ["logs"],
      installation_error: "handshake ready do plugin host falhou: plugin recusou configuração",
    })];
    renderPage();

    const card = screen.getByRole("heading", { name: "Enterprise" }).closest(".panel");
    expect(within(card as HTMLElement).getByText("Licença ativa")).toBeTruthy();
    expect(within(card as HTMLElement).getByRole("alert").textContent).toContain(
      "handshake ready do plugin host falhou: plugin recusou configuração",
    );
    expect(within(card as HTMLElement).queryByRole("button", { name: "Desativar" })).toBeNull();
    expect(screen.getByLabelText("Chave de licença")).toBeTruthy();
  });

  it("lists several public plugins and shows the available metadata", () => {
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
    expect(screen.getByText("1.2.0")).toBeTruthy();
    expect(screen.getAllByRole("img", { name: "Linux" }).length).toBeGreaterThan(0);
    expect(screen.queryByText("alpha_beta")).toBeNull();
    expect(screen.queryByText("-")).toBeNull();
    expect(screen.getByText("R$ 14,99/mês")).toBeTruthy();
    expect(screen.getByAltText("Alpha & Beta")).toBeTruthy();
    const screenshotGroup = screen.getByRole("group", { name: "Capturas de tela" });
    expect(within(screenshotGroup).getAllByRole("img", { name: /captura de tela/i })).toHaveLength(4);
    expect(screen.getByText("Sigma")).toBeTruthy();
    expect(screen.getByText("Licença ativa")).toBeTruthy();
    expect(screen.getByText("1.1.0")).toBeTruthy();

    expect(screen.getByRole("button", { name: "Comprar" })).toBeTruthy();
    expect(screen.queryByRole("link", { name: "Comprar" })).toBeNull();
    expect(screen.queryByRole("link", { name: "Gerenciar" })).toBeNull();
    expect(screen.getByRole("button", { name: "Desativar" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Instalar atualização" })).toBeTruthy();
  });

  it("registers the GitHub repository of a free catalog plugin only when the operator clicks", async () => {
    mocks.post.mockResolvedValue({});
    mocks.catalog.data = {
      plugins: [
        publicPlugin("community_tool", {
          display_name: "Community Tool", requires_license: false, featured: true, pom_team: false,
          github_url: "https://github.com/dev/pom-plugin-community",
        }),
      ],
    };
    renderPage();
    expect(screen.getByText("Destaque")).toBeTruthy();
    expect(screen.getByText("Comunidade")).toBeTruthy();
    expect(mocks.post).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Instalar" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/plugins/github-repositories", {
      repository_url: "https://github.com/dev/pom-plugin-community",
    }));
  });

  it("installs the latest release of a POM team plugin in one click, without picking a version", async () => {
    const repository = {
      id: "repo-q",
      repository_url: "https://github.com/pieceofmindcloud/pom-plugin-qdrant",
      manifest: { schema: 1, plugin_code: "qdrant", display_name: "Qdrant", description: null, plugin_abi: 1, feature_set: [], artifacts: {}, preferences: null },
      releases: [
        { release_id: "1", tag_name: "v1.0.0", name: null, changelog: null, published_at: "2026-01-01T00:00:00Z", prerelease: false, asset_name: "q.so", asset_size: 1, available_for_node: true },
        { release_id: "2", tag_name: "v1.1.0", name: null, changelog: null, published_at: "2026-02-01T00:00:00Z", prerelease: false, asset_name: "q.so", asset_size: 1, available_for_node: true },
      ],
      checked_at: "2026-09-24T00:00:00Z",
      check_error: null,
    };
    mocks.post.mockImplementation(async (path: string) => path === "/api/ui/plugins/github-repositories" ? repository : []);
    mocks.catalog.data = { plugins: [publicPlugin("qdrant", {
      display_name: "Qdrant", requires_license: false, pom_team: true, github_url: repository.repository_url,
    })] };
    renderPage();
    expect(screen.queryByRole("combobox")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Instalar" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/github-repositories/repo-q/install",
      { release_id: "2" },
    ));
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("leaves the example plugin out of the store list", () => {
    mocks.catalog.data = { plugins: [publicPlugin("base", { display_name: "POM Plugin Base" }), publicPlugin("alpha")] };
    renderPage();
    expect(screen.queryByRole("heading", { name: "POM Plugin Base" })).toBeNull();
    expect(screen.getByRole("heading", { name: "alpha" })).toBeTruthy();
  });

  it("shows the release of the local system instead of the latest global release", () => {
    mocks.catalog.data = {
      plugins: [publicPlugin("cross-platform", {
        releases: [
          release({ os: "macos", arch: "aarch64", version: "9.0.0" }),
          release({ os: "linux", arch: "x86_64", version: "1.5.0" }),
        ],
      })],
    };
    renderPage();

    expect(screen.getByText("1.5.0")).toBeTruthy();
    expect(screen.queryByText("9.0.0")).toBeNull();
    expect(screen.getByRole("img", { name: "Linux" })).toBeTruthy();
    expect(screen.getByRole("img", { name: "macOS" })).toBeTruthy();
    expect(screen.queryByRole("img", { name: "Windows" })).toBeNull();
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

  it("keeps local plugins active even if they do not come in the public catalog", () => {
    mocks.catalog.data = { plugins: [publicPlugin("public-plugin")] };
    mocks.plugins.data = [pluginStatus("local-active", { active: true, state: "active" })];
    renderPage();

    expect(screen.getByRole("heading", { name: "local-active" })).toBeTruthy();
    expect(screen.getByRole("heading", { name: "public-plugin" })).toBeTruthy();
    expect(screen.getAllByRole("button", { name: "Desativar" })).toHaveLength(1);
  });

  it("shows an empty state when there is no public plugin with a valid release", () => {
    mocks.catalog.data = { plugins: [] };
    mocks.plugins.data = [pluginStatus("local-inactive")];
    renderPage();

    expect(screen.getByText("Nenhum plugin disponível no momento.")).toBeTruthy();
    expect(screen.queryByText("local-inactive")).toBeNull();
  });

  it("shows a loading state while the public catalog is in progress", () => {
    mocks.catalog.data = undefined;
    mocks.catalog.isLoading = true;
    mocks.plugins.data = [];
    renderPage();

    expect(screen.getByRole("status", { name: "Carregando o catálogo público de plugins" })).toBeTruthy();
    expect(screen.queryByText("Nenhum plugin disponível no momento.")).toBeNull();
  });

  it("waits for the local license state before announcing an empty catalog", () => {
    mocks.catalog.data = { plugins: [] };
    mocks.plugins.data = [];
    mocks.plugins.isLoading = true;
    renderPage();

    expect(screen.queryByText("Nenhum plugin disponível no momento.")).toBeNull();
  });

  it("reports a catalog error and preserves the active local plugins", () => {
    mocks.catalog.data = undefined;
    mocks.catalog.isError = true;
    mocks.catalog.error = new Error("catalog offline");
    mocks.plugins.data = [pluginStatus("local-active", { active: true, state: "active" })];
    renderPage();

    expect(screen.getByText("Não foi possível carregar o catálogo público.")).toBeTruthy();
    expect(screen.getByRole("heading", { name: "local-active" })).toBeTruthy();
  });

  it("draws no images when the catalog does not provide them", () => {
    mocks.catalog.data = { plugins: [publicPlugin("no-images")] };
    mocks.plugins.data = [pluginStatus("no-images")];
    renderPage();

    const card = screen.getByRole("heading", { name: "no-images" }).closest(".panel");
    expect(card).toBeTruthy();
    expect((card as HTMLElement).querySelectorAll("img")).toHaveLength(0);
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
    expect(within(partial as HTMLElement).getByText("Licença ativa")).toBeTruthy();
    expect(within(full as HTMLElement).getByText("5 de 5 ativações usadas")).toBeTruthy();
    expect(within(full as HTMLElement).getByText("Limite atingido")).toBeTruthy();
  });

  it("uses a licensed plugin's authenticated status for its inactive activation candidate", () => {
    mocks.catalog.data = { plugins: [publicPlugin("analytics_plus")] };
    mocks.plugins.data = [
      pluginStatus("logs", {
        active: true,
        state: "active",
        licensed_plugins: ["logs", "analytics_plus"],
        plugin_usage: [
          { plugin_code: "logs", activation_count: 1, max_activations: 5 },
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
      pluginStatus("logs", {
        active: true,
        state: "active",
        licensed_plugins: ["logs"],
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
      pluginStatus("logs", {
        active: true,
        state: "active",
        licensed_plugins: ["logs"],
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
      pluginStatus("logs", {
        active: true,
        state: "active",
        licensed_plugins: ["logs"],
      }),
      pluginStatus("expired_plugin", { state: "expired" }),
      pluginStatus("revoked_plugin", { state: "revoked" }),
    ];
    renderPage();

    expect(screen.getByLabelText("Chave de licença")).toBeTruthy();
  });

  it("shows license expiration separately from the lease and install time in a compact card", () => {
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

    // The compact card does not repeat the last update lookup nor the features.
    expect(within(card as HTMLElement).queryByText(updateDate)).toBeNull();
    expect(within(card as HTMLElement).queryByText("Funcionalidades")).toBeNull();
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

  it("reloads the catalog after an error", () => {
    mocks.catalog.data = undefined;
    mocks.catalog.isError = true;
    mocks.catalog.error = new Error("catalog offline");
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Tentar de novo" }));

    expect(mocks.catalog.refetch).toHaveBeenCalled();
  });

  it("shows and applies the update of the active plugin", async () => {
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
    await waitFor(() => expect(mocks.queryClient.invalidateQueries).toHaveBeenCalledWith({
      queryKey: ["plugin-ui"],
    }));
  });

  it("adds any GitHub repository from the card below license activation", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("licensed")] };
    mocks.post.mockResolvedValue({});
    renderPage();

    const githubHeading = screen.getByRole("heading", { name: "Adicionar do GitHub" });
    const activationHeading = screen.getByRole("heading", { name: "Digite sua chave de licença" });
    expect(activationHeading.compareDocumentPosition(githubHeading) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    fireEvent.change(screen.getByLabelText("URL do repositório público do GitHub"), {
      target: { value: "https://github.com/acme/new-plugin" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Adicionar repositório" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/github-repositories",
      { repository_url: "https://github.com/acme/new-plugin" },
    ));
    expect(mocks.post).not.toHaveBeenCalledWith(expect.stringContaining("license"), expect.anything());
  });

  it("offers only the newer compatible releases of an installed plugin as updates", async () => {
    const repositoryUrl = "https://github.com/acme/new-plugin";
    const githubRelease = (release_id: string, tag_name: string, published_at: string, available_for_node = true) => ({
      release_id,
      tag_name,
      name: null,
      changelog: null,
      published_at,
      prerelease: false,
      asset_name: available_for_node ? "acme_plugin.so" : null,
      asset_size: available_for_node ? 1000 : null,
      available_for_node,
    });
    mocks.githubRepositories.data = [{
      id: "repo-id",
      repository_url: repositoryUrl,
      manifest: {
        schema: 1,
        plugin_code: "acme_plugin",
        display_name: "Acme Plugin",
        description: null,
        plugin_abi: 1,
        feature_set: ["acme_plugin.core"],
        artifacts: { "linux-x86_64": "acme_plugin.so" },
        preferences: null,
      },
      description: { en: "A local plugin release source.", "pt-BR": "Uma fonte local de releases." },
      screenshot_urls: ["https://raw.githubusercontent.com/acme/new-plugin/HEAD/docs/shot.png"],
      releases: [
        githubRelease("101", "v0.9.0", "2025-01-01T00:00:00Z"),
        githubRelease("103", "v2.0.0", "2026-09-24T00:00:00Z"),
        githubRelease("104", "v3.0.0-macos", "2026-09-25T00:00:00Z", false),
      ],
      checked_at: "2026-09-24T00:00:00Z",
      check_error: null,
    }];
    mocks.plugins.data = [pluginStatus("acme_plugin", {
      active: true,
      state: "active",
      public_install: true,
      github_repository: repositoryUrl,
      plugin_version: "0.9.0",
    })];
    mocks.post.mockResolvedValue({});
    renderPage();

    expect(screen.getByText("Uma fonte local de releases.")).toBeTruthy();
    expect(screen.queryByText("Ver detalhes do manifest.json")).toBeNull();
    expect(screen.getByAltText("Captura de tela 1 de Acme Plugin").getAttribute("src"))
      .toBe("https://raw.githubusercontent.com/acme/new-plugin/HEAD/docs/shot.png");
    const select = screen.getByRole("combobox", { name: "Atualização para instalar" }) as HTMLSelectElement;
    expect(Array.from(select.options).map((option) => option.value)).toEqual(["103"]);
    expect(select.value).toBe("103");

    fireEvent.click(screen.getByRole("button", { name: "Atualizar" }));
    const dialog = screen.getByRole("dialog", { name: "Atualizar Acme Plugin?" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Atualizar" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/github-repositories/repo-id/install",
      { release_id: "103" },
    ));
    await waitFor(() => expect(mocks.queryClient.invalidateQueries).toHaveBeenCalledWith({
      queryKey: ["plugin-ui"],
    }));
  });

  it("offers Install for a repository that is not installed and explains when no release fits", () => {
    mocks.githubRepositories.data = [
      {
        id: "repo-a",
        repository_url: "https://github.com/acme/a",
        manifest: { schema: 1, plugin_code: "alpha", display_name: "Alpha", description: null, plugin_abi: 1, feature_set: [], artifacts: {}, preferences: null },
        releases: [{ release_id: "1", tag_name: "v1.0.0", name: null, changelog: null, published_at: null, prerelease: false, asset_name: "a.so", asset_size: 1, available_for_node: true }],
        checked_at: "2026-09-24T00:00:00Z",
        check_error: null,
      },
      {
        id: "repo-b",
        repository_url: "https://github.com/acme/b",
        manifest: { schema: 1, plugin_code: "beta", display_name: "Beta", description: null, plugin_abi: 1, feature_set: [], artifacts: {}, preferences: null },
        releases: [{ release_id: "2", tag_name: "v1.0.0", name: null, changelog: null, published_at: null, prerelease: false, asset_name: null, asset_size: null, available_for_node: false }],
        checked_at: "2026-09-24T00:00:00Z",
        check_error: null,
      },
    ];
    renderPage();

    const alpha = screen.getByRole("heading", { name: "Alpha" }).closest(".panel") as HTMLElement;
    expect(within(alpha).getByRole("button", { name: "Instalar" })).toBeTruthy();
    const beta = screen.getByRole("heading", { name: "Beta" }).closest(".panel") as HTMLElement;
    expect(within(beta).getByText("Nenhuma release publicada para esta máquina ainda.")).toBeTruthy();
    expect(within(beta).queryByRole("combobox")).toBeNull();
  });

  it("refreshes and removes a local GitHub repository by repository id", async () => {
    mocks.githubRepositories.data = [{
      id: "repo-id",
      repository_url: "https://github.com/acme/new-plugin",
      manifest: { schema: 1, plugin_code: "acme_plugin", display_name: "Acme", description: null, plugin_abi: 1, feature_set: [], artifacts: {}, preferences: null },
      releases: [],
      checked_at: "2026-09-24T00:00:00Z",
      check_error: null,
    }];
    mocks.post.mockResolvedValue({});
    mocks.del.mockResolvedValue({});
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Atualizar releases" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/github-repositories/repo-id/refresh",
    ));
    fireEvent.click(screen.getByRole("button", { name: "Remover repositório" }));
    await waitFor(() => expect(mocks.del).toHaveBeenCalledWith(
      "/api/ui/plugins/github-repositories/repo-id",
    ));
    await waitFor(() => expect(mocks.githubRepositories.refetch).toHaveBeenCalled());
    expect(mocks.plugins.refetch).toHaveBeenCalled();
    await waitFor(() => expect(mocks.queryClient.invalidateQueries).toHaveBeenCalledWith({
      queryKey: ["plugin-ui"],
    }));
  });

  it("opens preferences from the settings icon and submits only to the local preferences route", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("alpha")] };
    mocks.plugins.data = [pluginStatus("alpha", {
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
      preferences: { features: { include_headers: false } },
    })];
    mocks.post.mockResolvedValue({});
    renderPage();

    expect(screen.queryByRole("checkbox", { name: /Incluir cabeçalhos/ })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Abrir preferências de alpha" }));
    fireEvent.click(await screen.findByRole("checkbox", { name: /Incluir cabeçalhos/ }));
    fireEvent.change(await screen.findByLabelText("Pasta de dados"), {
      target: { value: "/tmp/plugin-data" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Salvar preferências" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/alpha/preferences",
      { preferences: { features: { include_headers: true }, storage_directory: "/tmp/plugin-data" } },
    ));
    expect(mocks.post).not.toHaveBeenCalledWith("/api/ui/plugins/activate", expect.anything());
  });

  it("shows the node's reason when saving preferences fails", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("alpha")] };
    mocks.plugins.data = [pluginStatus("alpha", {
      active: true,
      state: "active",
      preferences_schema: { features: [{ key: "flag", label: "Flag", default: false }] },
      preferences: { features: { flag: false } },
    })];
    mocks.post.mockRejectedValue(new Error("preferência booleana desconhecida para este plugin"));
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Abrir preferências de alpha" }));
    fireEvent.click(await screen.findByRole("button", { name: "Salvar preferências" }));

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Não foi possível salvar estas preferências localmente.");
    expect(alert.textContent).toContain("preferência booleana desconhecida para este plugin");
  });

  it("rebuilds and reinstalls only an active mounted source without using the GitHub release route", async () => {
    mocks.node.data = { profile: "admin", pom_dev_mode: true, system: { os: "linux", arch: "x86_64" } };
    mocks.developmentSources.data = {
      mount_available: true,
      sources: [{
        repository: "pom-plugin-alpha",
        plugin_code: "alpha",
        display_name: "POM Plugin Alpha",
        active: true,
        install_kind: "licensed",
      }],
    };
    mocks.post.mockResolvedValue({
      repository: "pom-plugin-alpha",
      plugin_code: "alpha",
      build_ms: 1234,
      installed_at: "2026-10-01T10:00:00Z",
    });
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Recompilar e reinstalar" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/dev-sources/pom-plugin-alpha/rebuild-install",
    ));
    expect(await screen.findByText(/foi compilado e instalado localmente em .* ms/)).toBeTruthy();
    expect(mocks.post).not.toHaveBeenCalledWith(
      expect.stringContaining("github-repositories"),
      expect.anything(),
    );
    expect(mocks.developmentSources.refetch).toHaveBeenCalled();
    expect(mocks.plugins.refetch).toHaveBeenCalled();
  });

  it("builds and installs an inactive mounted source without requiring a license", async () => {
    mocks.node.data = { profile: "admin", pom_dev_mode: true, system: { os: "linux", arch: "x86_64" } };
    mocks.developmentSources.data = {
      mount_available: true,
      sources: [{
        repository: "pom-plugin-harness",
        plugin_code: "harness",
        display_name: "POM Plugin Harness",
        active: false,
        install_kind: null,
      }],
    };
    mocks.post.mockResolvedValue({
      repository: "pom-plugin-harness",
      plugin_code: "harness",
      build_ms: 1234,
      installed_at: "2026-10-01T10:00:00Z",
    });
    renderPage();

    const install = screen.getByRole("button", { name: "Compilar e instalar localmente" });
    expect((install as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(install);
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/dev-sources/pom-plugin-harness/rebuild-install",
    ));
    expect(await screen.findByText(/foi compilado e instalado localmente/)).toBeTruthy();
  });

  it("keeps license activation available for a running local development build", () => {
    mocks.node.data = { profile: "admin", pom_dev_mode: true, system: { os: "linux", arch: "x86_64" } };
    mocks.catalog.data = { plugins: [publicPlugin("logs")] };
    mocks.plugins.data = [pluginStatus("logs", {
      active: true,
      development_install: true,
      state: "needs_activation",
    })];
    renderPage();

    expect(screen.getByText("Build local")).toBeTruthy();
    expect(screen.getByText("Digite sua chave de licença")).toBeTruthy();
  });

  it("shows a safe build failure stage and exit code without compiler output", async () => {
    mocks.node.data = { profile: "admin", pom_dev_mode: true, system: { os: "linux", arch: "x86_64" } };
    mocks.developmentSources.data = {
      mount_available: true,
      sources: [{
        repository: "pom-plugin-alpha",
        plugin_code: "alpha",
        display_name: "POM Plugin Alpha",
        active: true,
        install_kind: "github",
      }],
    };
    mocks.post.mockRejectedValue(new ApiError(422, "build failed", {
      error: { code: "build_failed", stage: "compile", exit_code: 101, stderr: "secret build output" },
    }));
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Recompilar e reinstalar" }));
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("falhou durante compile");
    expect(alert.textContent).toContain("código de saída 101");
    expect(alert.textContent).not.toContain("secret build output");
  });

  it("offers local rebuilds on the plugin's card to every profile when development mode is on", () => {
    mocks.catalog.data = { plugins: [publicPlugin("harness", { display_name: "Harness" })] };
    mocks.developmentSources.data = {
      mount_available: true,
      sources: [{ repository: "pom-plugin-harness", plugin_code: "harness", display_name: "POM Plugin Harness", active: true, install_kind: "development" }],
    };
    for (const profile of ["worker", "visitor"]) {
      mocks.node.data = { profile, pom_dev_mode: true, system: { os: "linux", arch: "x86_64" } };
      const { unmount } = renderPage();
      const card = screen.getByRole("heading", { name: "Harness" }).closest(".panel") as HTMLElement;
      expect(within(card).getByRole("button", { name: "Recompilar e reinstalar" })).toBeTruthy();
      unmount();
    }
    mocks.node.data = { profile: "visitor", pom_dev_mode: false, system: { os: "linux", arch: "x86_64" } };
    renderPage();
    expect(screen.queryByRole("button", { name: "Recompilar e reinstalar" })).toBeNull();
  });

  it("deactivates a plugin without calling the pom_core update route", async () => {
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
  it("warns that a plugin not made by the POM team is unverified before installing it", async () => {
    const repository = {
      id: "repo-a",
      repository_url: "https://github.com/acme/a",
      manifest: { schema: 1, plugin_code: "alpha", display_name: "Alpha", description: null, plugin_abi: 1, feature_set: [], artifacts: {}, preferences: null },
      releases: [{ release_id: "1", tag_name: "v1.0.0", name: null, changelog: null, published_at: null, prerelease: false, asset_name: "a.so", asset_size: 1, available_for_node: true }],
      checked_at: "2026-09-24T00:00:00Z",
      check_error: null,
    };
    mocks.githubRepositories.data = [repository];
    renderPage();
    fireEvent.click(screen.getByRole("button", { name: "Instalar" }));
    expect(await screen.findByText("Código não verificado")).toBeTruthy();
    expect(screen.getByText(/por sua conta e risco/)).toBeTruthy();
    cleanup();

    mocks.catalog.data = { plugins: [publicPlugin("alpha", { display_name: "Alpha", requires_license: false, pom_team: true })] };
    mocks.post.mockResolvedValue([]);
    renderPage();
    fireEvent.click(screen.getByRole("button", { name: "Instalar" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith(
      "/api/ui/plugins/github-repositories/repo-a/install",
      { release_id: "1" },
    ));
    expect(screen.queryByText("Código não verificado")).toBeNull();
  });

  it("searches and pages the store on the license server", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("first")], total: 25 } as { plugins: Array<Record<string, unknown>> };
    renderPage();
    expect(mocks.catalogQueries).toContainEqual({ q: undefined, page: 1, perPage: 10 });
    expect(screen.getByText("Página 1 de 3")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Próxima" }));
    expect(mocks.catalogQueries.at(-1)).toEqual({ q: undefined, page: 2, perPage: 10 });

    fireEvent.change(screen.getByLabelText("Buscar plugins"), { target: { value: "  qdrant " } });
    await waitFor(() => expect(mocks.catalogQueries.at(-1)).toEqual({ q: "qdrant", page: 1, perPage: 10 }));
  });

  it("puts the example plugin and the publishing link in the Develop your plugin tab", async () => {
    mocks.post.mockResolvedValue({});
    renderPage();
    fireEvent.click(screen.getByRole("tab", { name: "Desenvolva seu plugin" }));
    expect(screen.getByRole("heading", { name: "Comece pelo pom-plugin-base" })).toBeTruthy();
    const publish = screen.getByRole("link", { name: /Enviar meu plugin/ });
    expect(publish.getAttribute("href")).toBe("https://pieceofmind.cloud/plugins/publicar/");
    expect(screen.getByText(/processo de revisão/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Instalar" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/plugins/github-repositories", {
      repository_url: "https://github.com/pieceofmindcloud/pom-plugin-base",
    }));
  });
  it("checks every installed plugin and POM at once and installs all the updates, POM last", async () => {
    const repository = {
      id: "repo-q",
      repository_url: "https://github.com/pieceofmindcloud/pom-plugin-qdrant",
      manifest: { schema: 1, plugin_code: "qdrant", display_name: "Qdrant", description: null, plugin_abi: 1, feature_set: [], artifacts: {}, preferences: null },
      releases: [
        { release_id: "1", tag_name: "v1.0.0", name: null, changelog: null, published_at: "2026-01-01T00:00:00Z", prerelease: false, asset_name: "q.so", asset_size: 1, available_for_node: true },
        { release_id: "2", tag_name: "v1.1.0", name: null, changelog: "Faster", published_at: "2026-02-01T00:00:00Z", prerelease: false, asset_name: "q.so", asset_size: 1, available_for_node: true },
      ],
      checked_at: "2026-09-24T00:00:00Z",
      check_error: null,
    };
    mocks.githubRepositories.data = [repository];
    mocks.plugins.data = [
      pluginStatus("qdrant", { active: true, state: "active", public_install: true, github_repository: repository.repository_url, plugin_version: "1.0.0" }),
      pluginStatus("logs", { active: true, state: "active", licensed_plugins: ["logs"], plugin_version: "2.0.0" }),
    ];
    mocks.post.mockImplementation(async (path: string) => {
      if (path.endsWith("/refresh")) return repository;
      if (path === "/api/ui/plugins/logs/update/check") {
        return [pluginStatus("logs", { active: true, available_update: { release_id: "logs-3", version: "3.0.0", changelog: null } })];
      }
      if (path === "/api/ui/pom-update/check") {
        return {
          current_version: "v0.5.5",
          core_update: { release_id: "v0.5.6", version: "0.5.6", changelog: null, published_at: null, source: "github", asset: "pom.AppImage", size: 1, installable: true, blocked: null },
        };
      }
      return {};
    });
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Verificar atualizações" }));
    const dialog = await screen.findByRole("dialog", { name: "Atualizações disponíveis" });
    expect(within(dialog).getByText("1.0.0 → v1.1.0")).toBeTruthy();
    expect(within(dialog).getByText("2.0.0 → 3.0.0")).toBeTruthy();
    expect(within(dialog).getByText("v0.5.5 → v0.5.6")).toBeTruthy();

    mocks.post.mockClear();
    fireEvent.click(within(dialog).getByRole("button", { name: "Instalar todas as 3 atualizações" }));
    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/pom-update/apply", { release_id: "v0.5.6" }));
    const installs = mocks.post.mock.calls.map(([path]) => path).filter((path: string) => !path.endsWith("/check") && !path.endsWith("/refresh"));
    expect(installs).toEqual([
      "/api/ui/plugins/github-repositories/repo-q/install",
      "/api/ui/plugins/logs/update/apply",
      "/api/ui/pom-update/apply",
    ]);
    expect(await within(dialog).findByText("Reiniciando")).toBeTruthy();
  });

  it("says everything is up to date when no source has anything newer", async () => {
    mocks.post.mockResolvedValue({ core_update: null, current_version: "v0.5.6" });
    renderPage();
    fireEvent.click(screen.getByRole("button", { name: "Verificar atualizações" }));
    expect(await screen.findByText("O POM e todos os plugins instalados estão atualizados.")).toBeTruthy();
  });
});
