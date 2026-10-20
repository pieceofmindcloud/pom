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
  code: "logs",
  display_name: "Statistics and Logs",
  description: "Advanced observability for POM.",
  description_pt_br: null,
  requires_license: true,
  icon_url: "https://license.pieceofmind.cloud/v1/plugins/logs/images/icon",
  screenshot_urls: [
    "https://license.pieceofmind.cloud/v1/plugins/logs/images/screenshot_1",
    "https://license.pieceofmind.cloud/v1/plugins/logs/images/screenshot_2",
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

  it("opens the site's purchase in a new tab with the plugin, the period and the language", () => {
    mocks.catalog.data = { plugins: [enterprise] };
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    renderPage("statistics");

    fireEvent.click(screen.getAllByRole("button", { name: "Comprar agora" })[0]);

    expect(open).toHaveBeenCalledWith(
      "https://pieceofmind.cloud/comprar/?plugin=logs&interval=month&lang=pt",
      "_blank",
      "noopener,noreferrer",
    );
    expect(mocks.post).not.toHaveBeenCalled();
    open.mockRestore();
  });

  it("keeps working without the catalog and sells Logs, Statistics and Quota as one plugin", () => {
    renderPage();

    expect(screen.getByText("Logs, Estatísticas e Quota")).toBeTruthy();
    expect(screen.getByText("Preço mostrado no checkout")).toBeTruthy();
    expect(screen.getByRole("heading", { name: "Quota" })).toBeTruthy();
    expect(screen.getByText(/Feito para os admins da rede/)).toBeTruthy();
    expect(screen.queryByText(/Stripe/)).toBeNull();
    const site = screen.getByRole("link", { name: /Mais na loja de plugins/ });
    expect(site.getAttribute("href")).toBe("https://pieceofmind.cloud/comprar/?plugin=logs&interval=month&lang=pt");
    expect(site.getAttribute("target")).toBe("_blank");
  });
});
