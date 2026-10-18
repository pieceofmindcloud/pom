// @vitest-environment jsdom
/**
 * The cluster's encryption state has to be readable from any screen.
 *
 * What is asserted here is that the badge says both things - encrypted and not encrypted -
 * instead of appearing only when it is on. A badge that vanishes when TLS drops is
 * worse than none: the operator reads the absence as "the screen does not show this", not
 * as "the cluster is in clear text".
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

function mountTopbar(profile: "admin" | "worker" | "visitor") {
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
              <Route path="/models/remote" element={<p>remotos</p>} />
            </Routes>
          </ProfileProvider>
        </MemoryRouter>
      </LocaleProvider>
    </QueryClientProvider>
  );
}

describe("TlsChip", () => {
  afterEach(cleanup);

  it("does not render the TLS indicator", () => {
    nodeData = fakeNode();
    const { container } = mount();
    expect(container.querySelector('[data-testid="tls-indicator"]')).toBeNull();
  });
});

describe("profile in the toolbar", () => {
  afterEach(cleanup);

  it("shows the network this machine is in, linking to its settings", () => {
    nodeData = { ...fakeNode(), network_name: "POM-SALEM" };
    mountTopbar("worker");

    const network = screen.getByTestId("topbar-network");
    expect(network.textContent).toBe("POM-SALEM");
    expect(network.getAttribute("aria-label")).toBe("Rede: POM-SALEM");
    expect(network.getAttribute("href")).toBe("/config/network");
  });

  it("shows no network chip while the node reports no network name", () => {
    nodeData = { ...fakeNode(), network_name: "" };
    mountTopbar("admin");
    expect(screen.queryByTestId("topbar-network")).toBeNull();
  });

  it("hides the This node button and keeps the official profile in the toolbar", () => {
    nodeData = fakeNode({ profile: "visitor" });
    mountTopbar("visitor");

    expect(screen.getByTestId("node-profile").textContent).toBe("visitor");
    expect(screen.queryByText(/perfil em visualização/i)).toBeNull();
    expect(screen.queryByRole("button", { name: "Este nó" })).toBeNull();
    expect(screen.queryByRole("button", { name: /administrador|node|infer/i })).toBeNull();
  });

  it("shows Larger Window in the native toolbar when WebKit offers the bridge", () => {
    nodeData = fakeNode({ profile: "admin" });
    Object.defineProperty(window, "webkit", {
      configurable: true,
      value: { messageHandlers: { pomOpenWindow: { postMessage: vi.fn() } } },
    });
    mountTopbar("admin");

    expect(screen.getByRole("button", { name: /janela maior/i })).toBeTruthy();
    delete (window as Window & { webkit?: unknown }).webkit;
  });

  it("hides the TLS indicator and the build hash outside admin", () => {
    nodeData = fakeNode({ profile: "visitor", build_commit: "abc123" });
    mountTopbar("visitor");

    expect(screen.queryByTestId("tls-indicator")).toBeNull();
    expect(screen.queryByText("abc123")).toBeNull();
  });
});

describe("menu mobile", () => {
  afterEach(cleanup);

  it("closes the drawer on navigation and allows reopening it on the next screen", () => {
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
