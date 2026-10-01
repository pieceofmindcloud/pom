// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { resetPublicLocatorCache } from "../lib/publicLocator";
import { PublicNetworks } from "./PublicNetworks";

const mocks = vi.hoisted(() => ({
  get: vi.fn(),
}));

vi.mock("../lib/api", () => ({
  api: { get: mocks.get },
}));
vi.mock("../lib/queries", () => ({
  useCluster: () => ({ data: undefined }),
}));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ fromError: vi.fn() }),
}));

const networks = [
  {
    cluster_id: "curitiba",
    name: "Rede Curitiba",
    fingerprint: "fingerprint-curitiba",
    publisher_key: "pk:curitiba",
    trust_document: "{}",
    organization: "Equipe Sul",
    description: "# Rede para colaborar\n\n**Bem-vindos**",
    rules: "- Respeite os peers",
    channels: "[Discord](https://discord.example/pom)",
    country_code: "BR",
    country_name: "Brasil",
    city: "Curitiba",
    geo_label: "br.curitiba",
    access_policy: "authorized" as const,
    location: null,
    discovered_via: "pkarr",
  },
  {
    cluster_id: "sao-paulo",
    name: "Rede São Paulo",
    fingerprint: "fingerprint-sao-paulo",
    publisher_key: "pk:sao-paulo",
    trust_document: "{}",
    organization: "Equipe Sudeste",
    description: "",
    rules: "",
    channels: "",
    country_code: "BR",
    country_name: "Brasil",
    city: "São Paulo",
    geo_label: "br.sao-paulo",
    access_policy: "authorized" as const,
    location: null,
    discovered_via: "pkarr",
  },
  {
    cluster_id: "lisboa",
    name: "Rede Lisboa",
    fingerprint: "fingerprint-lisboa",
    publisher_key: "pk:lisboa",
    trust_document: "{}",
    organization: "Equipe Europa",
    description: "",
    rules: "",
    channels: "",
    country_code: "PT",
    country_name: "Portugal",
    city: "Lisboa",
    geo_label: "pt.lisboa",
    access_policy: "authorized" as const,
    location: null,
    discovered_via: "pkarr",
  },
];

const publicHealth = vi.fn();

beforeEach(() => {
  vi.stubGlobal("fetch", publicHealth);
  mocks.get.mockImplementation((path: string) => {
    if (path === "/api/public/networks") return Promise.resolve(networks);
    if (path.startsWith("/api/public/networks/locate?")) {
      const key = new URLSearchParams(path.split("?")[1]).get("key");
      const reachability = key === "pk:curitiba" ? "direct" : key === "pk:sao-paulo" ? "relayed" : "unresolved";
      return Promise.resolve({ reachability, relays: reachability === "relayed" ? 1 : 0 });
    }
    return Promise.reject(new Error("no local network"));
  });
});

afterEach(() => {
  cleanup();
  resetPublicLocatorCache();
  vi.clearAllMocks();
  vi.unstubAllGlobals();
});

describe("PublicNetworks", () => {
  it("filtra redes por país e cidade", async () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <PublicNetworks />
      </LocaleProvider>,
    );

    expect(await screen.findByText("Rede Curitiba")).toBeTruthy();
    expect(screen.getByText("Rede São Paulo")).toBeTruthy();
    expect(screen.getByText("Rede Lisboa")).toBeTruthy();

    const country = screen.getByRole("combobox", { name: "País" });
    fireEvent.change(country, { target: { value: "BR" } });
    expect(screen.queryByText("Rede Lisboa")).toBeNull();
    expect(screen.getByText("Rede Curitiba")).toBeTruthy();

    const city = screen.getByRole("combobox", { name: "Cidade" });
    expect(within(city).getByRole("option", { name: "Curitiba" })).toBeTruthy();
    fireEvent.change(city, { target: { value: "São Paulo" } });
    await waitFor(() => {
      expect(screen.queryByText("Rede Curitiba")).toBeNull();
      expect(screen.getByText("Rede São Paulo")).toBeTruthy();
    });
  });

  it("shows reachability by the publisher key, resolved by the node, without an IP", async () => {
    const { container } = render(
      <LocaleProvider initialLocale="pt-BR">
        <PublicNetworks />
      </LocaleProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("public-network-reachability-fingerprint-curitiba").getAttribute("data-reachability")).toBe("direct");
      expect(screen.getByTestId("public-network-reachability-fingerprint-sao-paulo").getAttribute("data-reachability")).toBe("relayed");
      expect(screen.getByTestId("public-network-reachability-fingerprint-lisboa").getAttribute("data-reachability")).toBe("unresolved");
    });

    expect(mocks.get).toHaveBeenCalledWith("/api/public/networks/locate?key=pk%3Acuritiba", { auth: false });
    // The browser probes no address: the node does the resolving.
    expect(publicHealth).not.toHaveBeenCalled();
    expect(container.textContent).not.toMatch(/\d+\.\d+\.\d+\.\d+/);
  });

  it("abre os detalhes completos da rede pública ao clicar na linha", async () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <PublicNetworks />
      </LocaleProvider>,
    );

    const networkName = await screen.findByText("Rede Curitiba");
    fireEvent.click(networkName);

    const dialog = await screen.findByRole("dialog", { name: "Detalhes de Rede Curitiba" });
    expect(within(dialog).getByRole("heading", { name: "Rede para colaborar" })).toBeTruthy();
    expect(within(dialog).getByText("Respeite os peers")).toBeTruthy();
    expect(within(dialog).getByRole("link", { name: "Discord" })).toBeTruthy();
    expect(within(dialog).getByText("Equipe Sul")).toBeTruthy();
    expect(within(dialog).getByText("fingerprint-curitiba")).toBeTruthy();
  });
});
