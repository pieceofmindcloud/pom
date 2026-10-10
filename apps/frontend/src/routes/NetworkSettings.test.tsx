// @vitest-environment jsdom
/**
 * Quem pode virar o TLS do cluster, e o que vê quem não pode.
 *
 * A autoridade de verdade é do servidor (`save_tls` responde 403 fora do
 * principal). O que se afirma aqui é que a tela não mente antes do clique: no
 * principal o interruptor funciona, e num nó comum ele aparece desligado para
 * uso mas o **estado** continua visível - esconder o painel faria o operador de
 * um nó comum não ter onde ver se o tráfego da máquina dele está cifrado.
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { TlsPanel } from "./Preferences";
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
      profile: "node",
      authority_level: "primary" as "primary" | null,
      role: "principal",
      detached: false,
      advertise_addr: null,
      effective_address: "127.0.0.1:42777",
      tls_required: false,
      tls_authority: true,
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
        accepted_enrollments: ["node", "infer"],
        location: { latitude: -25.4, longitude: -49.2 },
      },
      can_edit: false,
    },
  },
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
    get: vi.fn(async (path: string) => path.includes("trust-share") ? queryMocks.trustShare : queryMocks.networkInfo.data),
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

function mount(props: Parameters<typeof TlsPanel>[0]) {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <TlsPanel {...props} />
    </LocaleProvider>,
  );
}

describe("TlsPanel", () => {
  afterEach(cleanup);

  it("on the principal, the switch is usable and propagates the flip", () => {
    const onToggle = vi.fn();
    mount({ tlsRequired: false, authority: true, onToggle });
    const button = screen.getByRole("switch");
    expect((button as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(button);
    expect(onToggle).toHaveBeenCalledWith(true);
  });

  it("when on, the principal can turn it off - the flip works both ways", () => {
    const onToggle = vi.fn();
    mount({ tlsRequired: true, authority: true, onToggle });
    fireEvent.click(screen.getByRole("switch"));
    expect(onToggle).toHaveBeenCalledWith(false);
  });

  it("outside the principal, shows the state but does not allow changes, and says who decides", () => {
    mount({ tlsRequired: true, authority: false });
    expect((screen.getByRole("switch") as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText("cifrado")).toBeTruthy();
    expect(screen.getByText(/só o nó principal decide/i)).toBeTruthy();
  });

  it("when off, the state is spelled out", () => {
    mount({ tlsRequired: false, authority: false });
    expect(screen.getByText("em texto claro")).toBeTruthy();
  });
});

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
    queryMocks.node.data.profile = "infer";
    queryMocks.nodes.data = [{
      node_id: "outro",
      short_id: "outro",
      hostname: "salem",
      addr: "127.0.0.1:8081",
      role: "node",
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

    expect(screen.getAllByText("infer").length).toBeGreaterThan(0);
    // The role is decided at admission: there is no upgrade in place.
    expect(screen.queryByRole("button", { name: /pedir para virar node/i })).toBeNull();
    expect(screen.getByText(/pedir admissão de novo como nó da rede/)).toBeTruthy();
    expect(screen.queryByText("Nós conhecidos")).toBeNull();
    expect(screen.queryByText("salem")).toBeNull();
    expect(vi.mocked(api.post)).not.toHaveBeenCalledWith("/api/ui/network/request-node");
  });

  it("shows the network information to an infer without offering editing", () => {
    queryMocks.node.data.profile = "infer";
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

  it("asks the quick setup's questions in its order, and only those that apply to the discovery", () => {
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
    expect(sections()).toEqual(["network-section-discovery", "network-section-info", "network-section-access"]);
    expect(screen.getByRole("radio", { name: /Oculta/ }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByRole("radio", { name: /Aprovados por um administrador/ }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByText("Aceitando novos membros")).toBeTruthy();
    expect(screen.queryByText("Lobby")).toBeNull();
    // Joining without approval carries its warning in the choice itself.
    fireEvent.click(screen.getByRole("radio", { name: /Qualquer um sem aprovação/ }));
    expect(screen.getByRole("radio", { name: /Qualquer um sem aprovação/ }).textContent).toContain("sem um administrador olhar");

    fireEvent.click(screen.getByRole("radio", { name: /Pública/ }));
    expect(sections()).toEqual(["network-section-discovery", "network-section-info", "network-section-access", "network-section-location"]);
    expect(screen.getByText("Rótulo de descoberta")).toBeTruthy();
    fireEvent.click(screen.getByRole("radio", { name: /Desligada/ }));
    expect(sections()).toEqual(["network-section-discovery", "network-section-info"]);
    expect(screen.queryByText("Aceitando novos membros")).toBeNull();
  });

  it("lets the admin revoke an admitted machine, behind a confirmation", async () => {
    admissionMocks.admissions.data = [
      { node_id: "aaaaaaaaaaaa1111", role: "node", status: "approved", requested_at: "", card: { hostname: "forasteiro", os: "", arch: "", cpu_count: 0, ram_total_mb: 0, gpus: [] } },
      // A request still waiting is decided in the notifications, not here.
      { node_id: "bbbbbbbbbbbb2222", role: "infer", status: "pending", requested_at: "", card: { hostname: "esperando", os: "", arch: "", cpu_count: 0, ram_total_mb: 0, gpus: [] } },
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
    queryMocks.node.data = { ...before, profile: "node", authority_level: null } as typeof before;
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
    const base = { role: "node", is_self: false, connected: true, blocked: false, verified: true };
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
    // O anunciado fica só no `title`, para comparação.
    expect(screen.queryByText(/2804:d55::1/)).toBeNull();
    expect(screen.getByTitle("anunciado: [2804:d55::1]:42777")).toBeTruthy();
  });
});
