// @vitest-environment jsdom
/**
 * Who can flip the cluster's TLS, and what someone who cannot sees.
 *
 * The real authority is the server's (`save_tls` answers 403 outside the
 * principal). What is asserted here is that the screen does not lie before the
 * click: on the principal the switch works, and on an ordinary node it shows
 * disabled for use but the **state** stays visible - hiding the panel would leave
 * the operator of an ordinary node nowhere to see whether their machine's traffic is encrypted.
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { NetworkParticipants, NetworkSettings } from "./NetworkSettings";
import { api } from "../lib/api";

const queryMocks = vi.hoisted(() => ({
  node: {
    data: {
      hostname: "infer-box",
      node_id: "a",
      p2p_port: 42777,
      gateway_port: 8080,
      inference_port: 8181,
      profile: "worker",
      authority_level: "primary" as "primary" | null,
      role: "principal",
      detached: false,
      advertise_addr: null,
      effective_address: "127.0.0.1:42777",
      thermal: { limit_c: 85, hysteresis_c: 8, cooldown_secs: 30 },
    },
  },
  cluster: { data: { peers: [] } },
  nodes: { data: [], isError: false, isLoading: false },
  networkInfo: {
    data: {
      network: {
        public_name: "Rede de teste",
        organization: "POM",
        description: "Uma rede para testar",
        rules: "Respeite os peers",
        channels: "#pom",
        country_code: "BR",
        country_name: "Brasil",
        city: "Curitiba",
        geo_label: "com.br.curitiba.node",
        visibility: "private",
        access_policy: "authorized",
        accepted_enrollments: ["worker", "visitor"],
        location: { latitude: -25.4, longitude: -49.2 },
      },
      can_edit: false,
    },
  },
  /** Whether the internet reaches this machine, as the relay report says. */
  relayReachable: true,
  trustShare: {
    filename: ".pom-admin-trust.json",
    document: '{"anchor":{}}',
    connection_key: "secret",
    visibility: "private",
  },
  firewall: {
    data: null as any,
    refetch: vi.fn(async () => undefined),
  },
}));

vi.mock("../lib/guide", () => ({
  useGuideActions: () => ({ complete: vi.fn(async () => {}), dismiss: vi.fn(async () => {}) }),
}));
vi.mock("../lib/api", () => ({
  api: {
    get: vi.fn(async (path: string) => path.includes("trust-share")
      ? queryMocks.trustShare
      : path.endsWith("/network/relay")
        ? { everyone: false, cluster: true, transfers: true, fixed: { everyone: false, no_cluster: false, no_transfers: false }, reachable: queryMocks.relayReachable, serving: queryMocks.relayReachable }
        : queryMocks.networkInfo.data),
    post: vi.fn(async (path: string, body: unknown) => path.endsWith("/settings") ? { network: body, can_edit: true } : undefined),
  },
}));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), fromError: vi.fn() }),
}));
const admissionMocks = vi.hoisted(() => ({
  admissions: { data: [] as unknown[], refetch: vi.fn() },
  decide: vi.fn(),
}));
vi.mock("../lib/admissions", () => ({
  useAdmissions: () => admissionMocks.admissions,
  admissionsApi: { decide: admissionMocks.decide },
}));
vi.mock("../lib/queries", () => ({
  useNode: () => queryMocks.node,
  useCluster: () => queryMocks.cluster,
  useNetworkInfo: () => queryMocks.networkInfo,
  useWindowsFirewall: () => queryMocks.firewall,
  useAdminNodes: (enabled = true) => (enabled ? queryMocks.nodes : { data: undefined }),
  useInvalidate: () => vi.fn(),
}));

describe("NetworkSettings", () => {
  afterEach(cleanup);

  it("does not show the preferences that now live on the Preferences screen", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    expect(screen.queryByText("Proteção térmica da GPU")).toBeNull();
    expect(screen.queryByText("Hugging Face")).toBeNull();
    expect(screen.queryByText("Criptografia entre nós")).toBeNull();
  });

  it("keeps the network accessible to infer, without listing peers, and sends a request", async () => {
    queryMocks.node.data.profile = "visitor";
    queryMocks.nodes.data = [{
      node_id: "outro",
      short_id: "outro",
      hostname: "salem",
      addr: "127.0.0.1:8081",
      role: "worker",
      is_self: false,
      connected: true,
      blocked: false,
      verified: true,
    }];

    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    expect(screen.getAllByText("visitor").length).toBeGreaterThan(0);
    // The role is decided at admission: there is no upgrade in place.
    expect(screen.queryByRole("button", { name: /pedir para virar node/i })).toBeNull();
    expect(screen.getByText(/pedir admissão de novo como nó da rede/)).toBeTruthy();
    expect(screen.queryByText("Nós conhecidos")).toBeNull();
    expect(screen.queryByText("salem")).toBeNull();
    expect(vi.mocked(api.post)).not.toHaveBeenCalledWith("/api/ui/network/request-node");
  });

  it("shows the network information to an infer without offering editing", () => {
    queryMocks.node.data.profile = "visitor";
    queryMocks.node.data.authority_level = null;
    queryMocks.networkInfo.data.network.description = "# Uma descrição formatada";
    queryMocks.networkInfo.data.network.rules = "- Regra em Markdown";
    queryMocks.networkInfo.data.network.channels = "[Discord](https://discord.example)";
    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    expect(screen.getByText("Sobre esta rede")).toBeTruthy();
    expect(screen.getByText("Rede de teste")).toBeTruthy();
    expect(screen.getByText("Somente leitura")).toBeTruthy();
    expect(screen.getByRole("heading", { name: "Uma descrição formatada" })).toBeTruthy();
    expect(screen.getByText("Regra em Markdown")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Discord" })).toBeTruthy();
    expect(screen.queryByText("# Uma descrição formatada")).toBeNull();
    expect(screen.queryByRole("button", { name: "Salvar detalhes da rede" })).toBeNull();
  });

  it("edits description, rules and channels as plain text, with no Markdown preview", () => {
    queryMocks.node.data.profile = "admin";
    queryMocks.node.data.authority_level = "primary";
    queryMocks.networkInfo.data.can_edit = true;
    queryMocks.networkInfo.data.network.description = "# Descrição da rede\n\nTexto inicial";
    queryMocks.networkInfo.data.network.rules = "- Seja gentil";

    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    expect((screen.getByLabelText(/^Descrição/) as HTMLTextAreaElement).value).toBe("# Descrição da rede\n\nTexto inicial");
    expect(screen.getByLabelText(/^Descrição/).tagName).toBe("TEXTAREA");
    expect(screen.getByLabelText(/^Regras da rede/).tagName).toBe("TEXTAREA");
    expect(screen.queryByTestId("markdown-preview-description")).toBeNull();
    expect(screen.queryByTestId("markdown-preview-rules")).toBeNull();
  });

  it("offers editing to the administrator and keeps sharing in the trust root", async () => {
    queryMocks.node.data.profile = "admin";
    queryMocks.node.data.authority_level = "primary";
    queryMocks.networkInfo.data.can_edit = true;
    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    expect(screen.getByRole("button", { name: "Salvar detalhes da rede" })).toBeTruthy();
    expect(screen.queryByText("Documento de confiança")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Salvar detalhes da rede" }));
    await waitFor(() => expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/ui/network/settings", expect.anything()));
  });

  it("asks the quick setup's questions in its order, and only those that apply to the discovery", async () => {
    queryMocks.node.data.profile = "admin";
    queryMocks.node.data.authority_level = "primary";
    queryMocks.networkInfo.data.can_edit = true;
    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    const sections = () => screen.getAllByTestId(/^network-section-/).map((section) => section.getAttribute("data-testid"));
    // Hidden: no place, since only the public directory places a network.
    // This machine's relay closes the form once the node answered.
    await screen.findByTestId("network-section-relay");
    expect(sections()).toEqual(["network-section-discovery", "network-section-info", "network-section-access", "network-section-relay"]);
    expect(screen.getByRole("radio", { name: /Oculta/ }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByRole("radio", { name: /Aprovados por um administrador/ }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByText("Aceitando novos membros")).toBeTruthy();
    expect(screen.queryByText("Lobby")).toBeNull();
    // Joining without approval carries its warning in the choice itself.
    fireEvent.click(screen.getByRole("radio", { name: /Qualquer um sem aprovação/ }));
    expect(screen.getByRole("radio", { name: /Qualquer um sem aprovação/ }).textContent).toContain("sem um administrador olhar");

    fireEvent.click(screen.getByRole("radio", { name: /Pública/ }));
    expect(sections()).toEqual(["network-section-discovery", "network-section-info", "network-section-access", "network-section-location", "network-section-relay"]);
    expect(screen.getByText("Rótulo de descoberta")).toBeTruthy();
    fireEvent.click(screen.getByRole("radio", { name: /Desligada/ }));
    expect(sections()).toEqual(["network-section-discovery", "network-section-info", "network-section-relay"]);
    expect(screen.queryByText("Aceitando novos membros")).toBeNull();
  });

  it("asks nothing about relaying behind a NAT", async () => {
    queryMocks.node.data.profile = "admin";
    queryMocks.node.data.authority_level = "primary";
    queryMocks.networkInfo.data.can_edit = true;
    queryMocks.relayReachable = false;
    try {
      render(
        <LocaleProvider initialLocale="pt-BR">
          <NetworkSettings />
        </LocaleProvider>,
      );
      await waitFor(() => expect(vi.mocked(api.get)).toHaveBeenCalledWith("/api/ui/network/relay"));
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(screen.queryByTestId("network-section-relay")).toBeNull();
      expect(screen.queryByTestId("relay-everyone")).toBeNull();
    } finally {
      queryMocks.relayReachable = true;
    }
  });

  it("lets the admin revoke an admitted machine, behind a confirmation", async () => {
    admissionMocks.admissions.data = [
      { node_id: "aaaaaaaaaaaa1111", role: "worker", status: "approved", requested_at: "", card: { hostname: "forasteiro", os: "", arch: "", cpu_count: 0, ram_total_mb: 0, gpus: [] } },
      // A request still waiting is decided in the notifications, not here.
      { node_id: "bbbbbbbbbbbb2222", role: "visitor", status: "pending", requested_at: "", card: { hostname: "esperando", os: "", arch: "", cpu_count: 0, ram_total_mb: 0, gpus: [] } },
    ];
    admissionMocks.decide.mockResolvedValue([]);
    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    const list = screen.getByTestId("admitted-machines");
    expect(list.textContent).toContain("forasteiro");
    expect(list.textContent).not.toContain("esperando");
    fireEvent.click(screen.getByRole("button", { name: "Revogar" }));
    expect(admissionMocks.decide).not.toHaveBeenCalled();
    const dialog = screen.getByRole("dialog", { name: "Revogar forasteiro?" });
    expect(dialog.textContent).toMatch(/nova chave de conexão.*pedir admissão de novo/);
    const confirm = screen.getAllByRole("button", { name: "Revogar" });
    fireEvent.click(confirm[confirm.length - 1]);
    await waitFor(() => expect(admissionMocks.decide).toHaveBeenCalledWith("aaaaaaaaaaaa1111", "forget"));
    admissionMocks.admissions.data = [];
  });

  it("offers no role change in place: a node cannot step down to inference here", () => {
    const before = queryMocks.node.data;
    queryMocks.node.data = { ...before, profile: "worker", authority_level: null } as typeof before;
    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );
    expect(screen.queryByRole("button", { name: /virar um nó infer/i })).toBeNull();
    expect(screen.queryByRole("button", { name: /pedir para virar node/i })).toBeNull();
    queryMocks.node.data = before;
  });

  it("offers no way out of the machine's own network", () => {
    const before = queryMocks.node.data;
    queryMocks.node.data = { ...before, own_network: true, standby_network: null } as typeof before;
    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );
    expect(screen.queryByRole("button", { name: "Voltar para a minha rede" })).toBeNull();
    queryMocks.node.data = before;
  });

  it("separates forgetting the local configuration (stays here) from leaving the cluster (now in My Machine)", async () => {
    queryMocks.node.data.profile = "admin";
    queryMocks.node.data.authority_level = "primary";
    queryMocks.networkInfo.data.can_edit = true;
    vi.clearAllMocks();

    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    expect(screen.queryByRole("button", { name: "Sair da rede" })).toBeNull();
    const forget = screen.getByRole("button", { name: "Voltar para a minha rede" });
    fireEvent.click(forget);

    const dialog = screen.getByRole("dialog", { name: "Sair desta rede?" });
    expect(dialog.textContent).toMatch(/volta para a rede própria.*nova chave de conexão.*novo pedido de admissão/i);
    expect(vi.mocked(api.post)).not.toHaveBeenCalled();

    const confirm = screen.getAllByRole("button", { name: "Voltar para a minha rede" });
    fireEvent.click(confirm[confirm.length - 1]);

    await waitFor(() => expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/ui/network/leave"));
    expect(vi.mocked(api.post)).not.toHaveBeenCalledWith("/api/ui/leave");
  });

  it("shows the firewall state on Windows and offers retry", async () => {
    queryMocks.firewall.data = {
      supported: true,
      p2p_port: 42777,
      rules: [
        { kind: "Discovery", protocol: "Udp", port: 42777, allowed: false },
      ],
      all_allowed: false,
      automatic_attempted: true,
      can_retry: true,
      last_error: null,
      in_progress: false,
    };
    vi.clearAllMocks();

    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    expect(screen.getByText("Firewall do Windows")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Tentar novamente" }));
    await waitFor(() =>
      expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/ui/network/firewall/retry"),
    );
    queryMocks.firewall.data = { supported: false };
  });
});

describe("NetworkParticipants", () => {
  afterEach(cleanup);

  it("shows the route in use and whether it is direct or relayed, not the announced address", () => {
    queryMocks.node.data.profile = "admin";
    queryMocks.node.data.authority_level = "primary";
    const base = { role: "worker", is_self: false, connected: true, blocked: false, verified: true };
    queryMocks.nodes.data = [
      {
        ...base,
        node_id: "furado",
        short_id: "furado",
        hostname: "ksia",
        addr: "[2804:d55::1]:42777",
        route: { kind: "p2p_direct", address: "189.75.167.63:28515 (QUIC)", hole_punched: true, links: [] },
      },
      {
        ...base,
        node_id: "relayed",
        short_id: "relayed",
        hostname: "salem",
        addr: "[2804:d55::2]:42777",
        route: { kind: "relayed", address: "82.25.69.187:52777 (TCP)", relay_kind: "dedicated", hole_punched: false, links: [] },
      },
      {
        ...base,
        node_id: "member-relayed",
        short_id: "member-relayed",
        hostname: "mac",
        addr: "[2804:d55::3]:42777",
        route: { kind: "relayed", address: "198.51.100.4:52777 (QUIC)", relay_kind: "member", hole_punched: false, links: [] },
      },
      {
        ...base,
        node_id: "lan",
        short_id: "lan",
        hostname: "gpu",
        addr: "192.168.0.37:42777",
        route: { kind: "http", address: "192.168.0.37:8080", hole_punched: false, links: [] },
      },
    ] as any;

    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkParticipants />
      </LocaleProvider>,
    );

    expect(screen.getByText("Direto (hole punch)")).toBeTruthy();
    expect(screen.getAllByText("Via relé")).toHaveLength(2);
    expect(screen.getByText("Direto")).toBeTruthy();
    expect(screen.getByText(/relé 82\.25\.69\.187:52777/)).toBeTruthy();
    expect(screen.getByText(/relé membro 198\.51\.100\.4:52777/)).toBeTruthy();
    expect(screen.getByText(/189\.75\.167\.63:28515/)).toBeTruthy();
    // The announced one stays only in the `title`, for comparison.
    expect(screen.queryByText(/2804:d55::1/)).toBeNull();
    expect(screen.getByTitle("anunciado: [2804:d55::1]:42777")).toBeTruthy();
  });

  it("shows each machine's POM version, amber where it is not this node's, and the build only in development mode", () => {
    queryMocks.node.data.profile = "admin";
    const base = { role: "worker", connected: true, blocked: false, verified: true, addr: "", route: null };
    queryMocks.nodes.data = [
      { ...base, node_id: "me", short_id: "me", hostname: "me", is_self: true, pom_version: "v0.5.6", pom_build: "abc1234" },
      { ...base, node_id: "same", short_id: "same", hostname: "same", is_self: false, pom_version: "v0.5.6", pom_build: "abc1234" },
      { ...base, node_id: "old", short_id: "old", hostname: "old", is_self: false, pom_version: "v0.5.4", pom_build: "def5678" },
      { ...base, node_id: "silent", short_id: "silent", hostname: "silent", is_self: false },
    ] as any;
    const view = () => render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkParticipants />
      </LocaleProvider>,
    );

    queryMocks.node.data.pom_dev_mode = false;
    view();
    const versions = screen.getAllByTestId("participant-version");
    expect(versions.map((badge) => badge.textContent)).toEqual(["v0.5.6", "v0.5.6", "v0.5.4", "versão desconhecida"]);
    expect(versions[2].className).toContain("amber");
    expect(versions[1].className).not.toContain("amber");
    expect(screen.getByTitle("Roda uma versão do POM diferente da deste nó (v0.5.6)")).toBeTruthy();
    cleanup();

    queryMocks.node.data.pom_dev_mode = true;
    view();
    expect(screen.getAllByTestId("participant-version")[2].textContent).toBe("v0.5.4 · def5678");
    queryMocks.node.data.pom_dev_mode = false;
  });
});
