// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { EnterpriseFeatureShowcase } from "./EnterpriseFeatureShowcase";

const mocks = vi.hoisted(() => ({
  post: vi.fn(),
  catalog: { data: undefined as unknown, isLoading: false, isError: false },
}));

vi.mock("../lib/api", async () => {
  const actual = await vi.importActual<typeof import("../lib/api")>("../lib/api");
  return { ...actual, api: { post: mocks.post } };
});
vi.mock("../lib/queries", () => ({ usePublicPluginCatalog: () => mocks.catalog }));

const enterprise = {
  code: "enterprise",
  display_name: "Statistics and Logs",
  description: "Advanced observability for POM.",
  description_pt_br: null,
  requires_license: true,
  icon_url: "https://license.pieceofmind.cloud/v1/plugins/enterprise/images/icon",
  screenshot_urls: [
    "https://license.pieceofmind.cloud/v1/plugins/enterprise/images/screenshot_1",
    "https://license.pieceofmind.cloud/v1/plugins/enterprise/images/screenshot_2",
  ],
  pricing: { monthly: { amount: 1499, currency: "brl", interval: "month" }, annual: null },
  releases: [],
};

function renderPage(kind: "logs" | "statistics" = "logs") {
  return render(
    <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
      <LocaleProvider initialLocale="pt-BR">
        <EnterpriseFeatureShowcase kind={kind} />
      </LocaleProvider>
    </MemoryRouter>,
  );
}

describe("EnterpriseFeatureShowcase", () => {
  afterEach(() => {
    cleanup();
    mocks.post.mockReset();
    mocks.catalog.data = undefined;
  });

  it("shows the official screenshots and price published by the license server", () => {
    mocks.catalog.data = { plugins: [enterprise] };
    renderPage();

    expect(screen.getByText("Statistics and Logs")).toBeTruthy();
    expect(screen.getAllByText(/R\$\s?14,99/).length).toBeGreaterThan(0);
    const shots = screen.getAllByRole("img", { name: /Captura \d de Statistics and Logs/ });
    expect(shots.map((image) => image.getAttribute("src"))).toEqual(enterprise.screenshot_urls);
    expect(screen.getByText("Veja cada requisição.")).toBeTruthy();
  });

  it("opens Stripe Checkout through the node with the plugin and interval", async () => {
    mocks.catalog.data = { plugins: [enterprise] };
    mocks.post.mockResolvedValue({ url: "https://checkout.stripe.com/c/pay/cs_test_1" });
    const assign = vi.fn();
    Object.defineProperty(window, "location", { configurable: true, value: { ...window.location, assign } });
    renderPage("statistics");

    fireEvent.click(screen.getAllByRole("button", { name: "Comprar agora" })[0]);

    await waitFor(() => expect(mocks.post).toHaveBeenCalledWith("/api/ui/plugins/checkout", {
      plugin_code: "enterprise",
      interval: "month",
      locale: "pt-BR",
    }));
    await waitFor(() => expect(assign).toHaveBeenCalledWith("https://checkout.stripe.com/c/pay/cs_test_1"));
  });

  it("keeps working without the catalog and explains a refused checkout", async () => {
    const { ApiError } = await import("../lib/api");
    mocks.post.mockRejectedValue(new ApiError(403, "forbidden"));
    renderPage();

    expect(screen.getByText("Estatísticas e Logs")).toBeTruthy();
    expect(screen.getByText("Preço mostrado no checkout")).toBeTruthy();
    fireEvent.click(screen.getAllByRole("button", { name: "Comprar agora" })[0]);
    expect((await screen.findByRole("alert")).textContent).toContain("Só um administrador deste nó pode comprar plugins.");
  });
});
