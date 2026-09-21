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
    activation_usage: null,
    releases: [release()],
    ...overrides,
  };
}

const mocks = vi.hoisted(() => ({
  post: vi.fn(),
  createCheckout: vi.fn(),
  navigateToCheckout: vi.fn(),
  plugins: {
    data: [] as Array<{
      code: string;
      active: boolean;
      state: string;
      features: string[];
      manage_url: string;
      plugin_version: string | null;
      lease_expires_at: string | null;
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
}));

vi.mock("../lib/api", () => ({ api: { post: mocks.post } }));
vi.mock("../lib/queries", () => ({
  usePlugins: () => mocks.plugins,
  usePublicPluginCatalog: () => mocks.catalog,
}));
vi.mock("../lib/pluginCheckout", () => ({
  createPluginCheckout: mocks.createCheckout,
  navigateToCheckout: mocks.navigateToCheckout,
}));

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
    mocks.createCheckout.mockReset();
    mocks.navigateToCheckout.mockReset();
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

  it("abre a sessão do Stripe diretamente ao comprar um plugin", async () => {
    const entry = publicPlugin("analytics_plus", { display_name: "Analytics Plus" });
    mocks.catalog.data = { plugins: [entry] };
    mocks.plugins.data = [pluginStatus("analytics_plus")];
    mocks.createCheckout.mockResolvedValue("https://checkout.stripe.com/c/pay/cs_test_12345678");
    renderPage("en");

    fireEvent.click(screen.getByRole("button", { name: "Buy" }));

    await waitFor(() => {
      expect(mocks.createCheckout).toHaveBeenCalledWith(entry, "en");
      expect(mocks.navigateToCheckout).toHaveBeenCalledWith(
        "https://checkout.stripe.com/c/pay/cs_test_12345678",
      );
    });
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

  it("shows machine activation usage and distinguishes partial use from a full limit", () => {
    mocks.catalog.data = {
      plugins: [
        publicPlugin("partial", { activation_usage: { active_machines: 2, max_machines: 5 } }),
        publicPlugin("full", { activation_usage: { active_machines: 5, max_machines: 5 } }),
      ],
    };
    mocks.plugins.data = [
      pluginStatus("partial", { active: true, state: "active" }),
      pluginStatus("full"),
    ];
    renderPage();

    const partial = screen.getByRole("heading", { name: "partial" }).closest(".panel");
    const full = screen.getByRole("heading", { name: "full" }).closest(".panel");
    expect(within(partial as HTMLElement).getByText("2 de 5 máquinas ativadas")).toBeTruthy();
    expect(within(partial as HTMLElement).getByText("Parcialmente utilizada")).toBeTruthy();
    expect(within(partial as HTMLElement).getByText("Ativo")).toBeTruthy();
    expect(within(full as HTMLElement).getByText("5 de 5 máquinas ativadas")).toBeTruthy();
    expect(within(full as HTMLElement).getByText("Limite atingido")).toBeTruthy();
  });

  it("shows license expiration separately from install and update-check times", () => {
    mocks.catalog.data = { plugins: [publicPlugin("alpha")] };
    mocks.plugins.data = [pluginStatus("alpha", {
      active: true,
      state: "active",
      lease_expires_at: "2099-12-31T12:00:00.000Z",
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
      .format(new Date("2099-12-31T12:00:00.000Z"));

    expect(within(card as HTMLElement).getByText("Última verificação de atualização")).toBeTruthy();
    expect(within(card as HTMLElement).getByText(updateDate)).toBeTruthy();
    expect(within(card as HTMLElement).getByText("Última instalação")).toBeTruthy();
    expect(within(card as HTMLElement).getByText(installDate)).toBeTruthy();
    expect(within(card as HTMLElement).getByText("Licença válida até")).toBeTruthy();
    expect(within(card as HTMLElement).getByText(expirationDate)).toBeTruthy();
  });

  it("hides license activation when every catalog plugin is already active", () => {
    mocks.catalog.data = { plugins: [publicPlugin("active-plugin")] };
    mocks.plugins.data = [pluginStatus("active-plugin", { active: true, state: "active" })];
    renderPage();

    expect(screen.queryByLabelText("Chave de licença")).toBeNull();
    expect(screen.getByRole("heading", { name: "active-plugin" })).toBeTruthy();
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

  it("directly opens the Stripe session from the Buy action with the selected locale", async () => {
    const entry = publicPlugin("alpha");
    mocks.catalog.data = { plugins: [entry] };
    mocks.plugins.data = [pluginStatus("alpha")];
    mocks.createCheckout.mockResolvedValue("https://checkout.stripe.com/c/pay/cs_test_checkout_123");
    renderPage();

    const buy = screen.getByRole("button", { name: "Comprar" });
    expect(buy.className).toContain("plugin-buy-button");
    expect(buy.className).toContain("focus-visible:ring-2");
    fireEvent.click(buy);

    await waitFor(() => expect(mocks.createCheckout).toHaveBeenCalledWith(entry, "pt-BR"));
    expect(mocks.navigateToCheckout).toHaveBeenCalledWith("https://checkout.stripe.com/c/pay/cs_test_checkout_123");
  });

  it("shows a translated checkout error and keeps the plugin available after failure", async () => {
    mocks.catalog.data = { plugins: [publicPlugin("alpha")] };
    mocks.plugins.data = [pluginStatus("alpha")];
    mocks.createCheckout.mockRejectedValue(new Error("checkout unavailable"));
    renderPage();

    fireEvent.click(screen.getByRole("button", { name: "Comprar" }));

    expect(await screen.findByText("Não foi possível abrir o checkout seguro. Tente novamente.")).toBeTruthy();
    expect(screen.getByRole("heading", { name: "alpha" })).toBeTruthy();
    expect(mocks.navigateToCheckout).not.toHaveBeenCalled();
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
    const dialog = screen.getByRole("dialog", { name: "Desativar sigma?" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Desativar" }));

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/plugins/sigma/deactivate"));
    expect(mocks.post).not.toHaveBeenCalledWith(expect.stringContaining("pom-update"));
  });
});
