// @vitest-environment jsdom
/**
 * O estado da criptografia do cluster tem que ser legível de qualquer tela.
 *
 * O que se afirma aqui é que o selo diz as duas coisas - cifrado e não cifrado -
 * em vez de aparecer só quando está ligado. Um selo que some quando o TLS cai é
 * pior que nenhum: o operador lê a ausência como "a tela não mostra isso", não
 * como "o cluster está em texto claro".
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { ProfileProvider } from "../lib/profile";
import { fakeNode } from "../test/fixtures";
import type { NodeInfo } from "../lib/types";

let nodeData: NodeInfo | undefined;

vi.mock("../lib/queries", async (original) => ({
  ...(await original<typeof import("../lib/queries")>()),
  useNode: () => ({ data: nodeData }),
  useCluster: () => ({ data: { peers: [], complete: false } }),
}));

const { TlsChip } = await import("./TlsIndicator");
const { Topbar } = await import("./Topbar");

function mount() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
          <TlsChip />
        </MemoryRouter>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

function mountTopbar(profile: "admin" | "node" | "infer") {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
          <ProfileProvider initialProfile={profile}>
            <Topbar />
          </ProfileProvider>
        </MemoryRouter>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

function mountTopbarWithRoutes() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <MemoryRouter initialEntries={["/chat"]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
          <ProfileProvider initialProfile="admin">
            <Topbar />
            <Routes>
              <Route path="/chat" element={<p>chat</p>} />
              <Route path="/modelos/remotos" element={<p>remotos</p>} />
            </Routes>
          </ProfileProvider>
        </MemoryRouter>
      </LocaleProvider>
    </QueryClientProvider>
  );
}

describe("TlsChip", () => {
  afterEach(cleanup);

  it("não renderiza o indicador TLS", () => {
    nodeData = fakeNode({ tls_required: true });
    const { container } = mount();
    expect(container.querySelector('[data-testid="tls-indicator"]')).toBeNull();
  });
});

describe("perfil na toolbar", () => {
  afterEach(cleanup);

  it("shows the network this machine is in, linking to its settings", () => {
    nodeData = { ...fakeNode(), network_name: "POM-SALEM" };
    mountTopbar("node");

    const network = screen.getByTestId("topbar-network");
    expect(network.textContent).toBe("POM-SALEM");
    expect(network.getAttribute("aria-label")).toBe("Rede: POM-SALEM");
    expect(network.getAttribute("href")).toBe("/config/rede");
  });

  it("shows no network chip while the node reports no network name", () => {
    nodeData = { ...fakeNode(), network_name: "" };
    mountTopbar("admin");
    expect(screen.queryByTestId("topbar-network")).toBeNull();
  });

  it("oculta o botão Este nó e mantém o perfil oficial na toolbar", () => {
    nodeData = fakeNode({ profile: "infer" });
    mountTopbar("infer");

    expect(screen.getByTestId("node-profile").textContent).toBe("infer");
    expect(screen.queryByText(/perfil em visualização/i)).toBeNull();
    expect(screen.queryByRole("button", { name: "Este nó" })).toBeNull();
    expect(screen.queryByRole("button", { name: /administrador|node|infer/i })).toBeNull();
  });

  it("mostra Larger Window na toolbar nativa quando o WebKit oferece a ponte", () => {
    nodeData = fakeNode({ profile: "admin" });
    Object.defineProperty(window, "webkit", {
      configurable: true,
      value: { messageHandlers: { pomOpenWindow: { postMessage: vi.fn() } } },
    });
    mountTopbar("admin");

    expect(screen.getByRole("button", { name: /janela maior/i })).toBeTruthy();
    delete (window as Window & { webkit?: unknown }).webkit;
  });

  it("oculta o indicador TLS e o hash do build fora do admin", () => {
    nodeData = fakeNode({ profile: "infer", build_commit: "abc123" });
    mountTopbar("infer");

    expect(screen.queryByTestId("tls-indicator")).toBeNull();
    expect(screen.queryByText("abc123")).toBeNull();
  });
});

describe("menu mobile", () => {
  afterEach(cleanup);

  it("fecha o drawer ao navegar e permite reabri-lo na tela seguinte", () => {
    mountTopbarWithRoutes();

    fireEvent.click(screen.getByRole("button", { name: "Abrir menu" }));
    fireEvent.click(screen.getByRole("button", { name: "Modelos" }));
    fireEvent.click(screen.getByRole("link", { name: "Encontrar Modelos HF" }));

    expect(screen.getByText("remotos")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Fechar menu" })).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Abrir menu" }));
    expect(screen.getByRole("button", { name: "Fechar menu" })).toBeTruthy();
  });
});
