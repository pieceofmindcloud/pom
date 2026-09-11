// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
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
    address: "https://curitiba.example",
    bootstrap_address: "https://curitiba.example/bootstrap",
    trust_document: "{}",
    organization: "Equipe Sul",
    description: "",
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
    address: "https://sao-paulo.example",
    bootstrap_address: "https://sao-paulo.example/bootstrap",
    trust_document: "{}",
    organization: "Equipe Sudeste",
    description: "",
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
    address: "https://lisboa.example",
    bootstrap_address: "https://lisboa.example/bootstrap",
    trust_document: "{}",
    organization: "Equipe Europa",
    description: "",
    country_code: "PT",
    country_name: "Portugal",
    city: "Lisboa",
    geo_label: "pt.lisboa",
    access_policy: "authorized" as const,
    location: null,
    discovered_via: "pkarr",
  },
];

beforeEach(() => {
  mocks.get.mockImplementation((path: string) =>
    path === "/api/public/networks" ? Promise.resolve(networks) : Promise.reject(new Error("no local network")),
  );
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
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
});
