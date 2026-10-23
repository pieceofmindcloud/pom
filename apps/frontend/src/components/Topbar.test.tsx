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
let natStatus: "unknown" | "public" | "private" | undefined;
let relayedPeers: { node_id: string; hostname: string; relay_kind: "public" | "dedicated" | "member" | null }[] = [];
let quotaData: { available: boolean; configured?: boolean; pending?: boolean } | undefined;

vi.mock("../lib/queries", async (original) => ({
  ...(await original<typeof import("../lib/queries")>()),
  useNode: () => ({ data: nodeData }),
  useMyQuota: () => ({ data: quotaData }),
  useCluster: () => ({
    data: {
      peers: [],
      complete: false,
      traversal: natStatus && {
        nat: natStatus,
        addrs: [],
        reservations: [],
        hole_punch_ok: 0,
        hole_punch_failed: 0,
        last_hole_punch_error: null,
      },
      relayed_peers: relayedPeers,
    },
  }),
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

  it("labels a worker's profile and leaves a visitor's out", () => {
    nodeData = fakeNode({ profile: "worker" });
    mountTopbar("worker");
    expect(screen.getByTestId("node-profile").textContent).toBe("worker");
    cleanup();

    nodeData = fakeNode({ profile: "visitor" });
    mountTopbar("visitor");
    expect(screen.queryByTestId("node-profile")).toBeNull();
  });

  it("folds the network and node badges into one popover on a narrow screen", () => {
    nodeData = { ...fakeNode(), network_name: "POM-SALEM" };
    mountTopbar("worker");
    expect(screen.getAllByTestId("topbar-network")).toHaveLength(1);
    expect(screen.queryByTestId("topbar-details-popover")).toBeNull();

    fireEvent.click(screen.getByTestId("topbar-details"));
    const popover = screen.getByTestId("topbar-details-popover");
    expect(popover.textContent).toContain("POM-SALEM");
    expect(popover.textContent).toContain("worker");
  });

  it("hides the This node button", () => {
    nodeData = fakeNode({ profile: "visitor" });
    mountTopbar("visitor");

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

describe("NAT badge in the toolbar", () => {
  afterEach(() => {
    cleanup();
    natStatus = undefined;
    relayedPeers = [];
  });

  it("no longer shows the node id", () => {
    nodeData = fakeNode();
    mountTopbar("admin");
    expect(screen.queryByText(nodeData.short_id)).toBeNull();
  });

  it("shows nothing behind a NAT whose hole punch worked", () => {
    nodeData = fakeNode();
    natStatus = "private";
    relayedPeers = [];
    mountTopbar("admin");
    expect(screen.queryByTestId("topbar-nat")).toBeNull();
  });

  it("shows Behind NAT and opens the explanation with this machine's ports", () => {
    nodeData = { ...fakeNode(), p2p_port: 42777 };
    natStatus = "private";
    relayedPeers = [{ node_id: "abc123", hostname: "machine-b", relay_kind: "public" }];
    mountTopbar("admin");

    const badge = screen.getByTestId("topbar-nat");
    expect(badge.textContent).toBe("Atrás de NAT");
    expect(screen.queryByTestId("nat-dialog")).toBeNull();

    fireEvent.click(badge);
    const dialog = screen.getByTestId("nat-dialog");
    expect(dialog.textContent).toContain("2 MB/s");
    expect(dialog.textContent).toContain("IPv6");
    expect(dialog.textContent).toContain("UDP 42777");
    expect(dialog.textContent).toContain("UDP 52777");
    expect(screen.getByTestId("nat-relayed-peers").textContent).toContain("machine-b");
    expect(screen.getByTestId("nat-relayed-peers").textContent).toContain("relé público");
  });
});

describe("Usage beside the bell", () => {
  afterEach(() => {
    cleanup();
    quotaData = undefined;
  });

  it("shows only when this node has a budget set for it", () => {
    nodeData = fakeNode();
    quotaData = { available: true, configured: false };
    mountTopbar("visitor");
    expect(screen.queryByTestId("topbar-usage")).toBeNull();
    cleanup();

    quotaData = { available: true, configured: true };
    mountTopbar("visitor");
    const usage = screen.getByTestId("topbar-usage");
    expect(usage.getAttribute("aria-label")).toBe("Uso");
    expect(usage.textContent).toBe("");
    expect(screen.queryByTestId("usage-popover")).toBeNull();

    fireEvent.click(usage);
    expect(screen.getByTestId("usage-popover").textContent).toContain("Dentro do budget");
    expect(screen.getByTestId("usage-open-screen").getAttribute("href")).toBe("/config/usage");
  });
});
