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

vi.mock("../lib/api", () => ({
  api: {
    get: vi.fn(async (path: string) => path.includes("trust-share") ? queryMocks.trustShare : queryMocks.networkInfo.data),
    post: vi.fn(async (path: string, body: unknown) => path.endsWith("/settings") ? { network: body, can_edit: true } : undefined),
  },
}));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), fromError: vi.fn() }),
}));
vi.mock("../lib/queries", () => ({
  useNode: () => queryMocks.node,
  useCluster: () => queryMocks.cluster,
  useNetworkInfo: () => queryMocks.networkInfo,
  useGeoReference: () => ({ data: { countries: [] } }),
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

  it("no principal, o interruptor é usável e propaga a virada", () => {
    const onToggle = vi.fn();
    mount({ tlsRequired: false, authority: true, onToggle });
    const botao = screen.getByRole("switch");
    expect((botao as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(botao);
    expect(onToggle).toHaveBeenCalledWith(true);
  });

  it("ligado, o principal pode desligar - a virada vale nos dois sentidos", () => {
    const onToggle = vi.fn();
    mount({ tlsRequired: true, authority: true, onToggle });
    fireEvent.click(screen.getByRole("switch"));
    expect(onToggle).toHaveBeenCalledWith(false);
  });

  it("fora do principal, mostra o estado mas não deixa mexer, e diz quem decide", () => {
    mount({ tlsRequired: true, authority: false });
    expect((screen.getByRole("switch") as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText("cifrado")).toBeTruthy();
    expect(screen.getByText(/só o nó principal decide/i)).toBeTruthy();
  });

  it("desligado, o estado é dito com todas as letras", () => {
    mount({ tlsRequired: false, authority: false });
    expect(screen.getByText("em texto claro")).toBeTruthy();
  });
});

describe("NetworkSettings", () => {
  afterEach(cleanup);

  it("não mostra as preferências que agora vivem na tela Preferências", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    expect(screen.queryByText("Proteção térmica da GPU")).toBeNull();
    expect(screen.queryByText("Hugging Face")).toBeNull();
    expect(screen.queryByText("Criptografia entre nós")).toBeNull();
  });

  it("mantém a rede acessível ao infer, sem listar peers, e envia um pedido", async () => {
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
    expect(screen.getByRole("button", { name: /pedir para virar node/i })).toBeTruthy();
    expect(screen.queryByText("Nós conhecidos")).toBeNull();
    expect(screen.queryByText("salem")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: /pedir para virar node/i }));
    await waitFor(() => expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/ui/network/request-node"));
  });

  it("mostra as informações da rede para um infer sem oferecer edição", () => {
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

  it("oferece edição Markdown ampla com preview para descrição, regras e canais", () => {
    queryMocks.node.data.profile = "admin";
    queryMocks.node.data.authority_level = "primary";
    queryMocks.networkInfo.data.can_edit = true;
    queryMocks.networkInfo.data.network.description = "# Descrição da rede\n\nTexto inicial";
    queryMocks.networkInfo.data.network.rules = "- Seja gentil";
    queryMocks.networkInfo.data.network.channels = "[Discord](https://discord.example)";

    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    for (const label of ["Descrição", "Regras da rede", "Discord ou Telegram"]) {
      const field = screen.getByRole("textbox", { name: label });
      expect(field.tagName).toBe("TEXTAREA");
      expect(field.className).toMatch(/min-h-\[12rem\]/);
    }
    expect(screen.getByTestId("markdown-preview-description").querySelector("h1")?.textContent).toBe("Descrição da rede");
    expect(screen.getByTestId("markdown-preview-rules").querySelector("li")?.textContent).toBe("Seja gentil");
    expect(screen.getByTestId("markdown-preview-channels").querySelector("a")?.textContent).toBe("Discord");
  });

  it("oferece edição ao administrador e mantém o compartilhamento na raiz de confiança", async () => {
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

  it("separa esquecer a configuração local (continua aqui) de sair do cluster (agora em Minha Máquina)", async () => {
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
    const esquecer = screen.getByRole("button", { name: "Esquecer rede" });
    fireEvent.click(esquecer);

    const dialog = screen.getByRole("dialog", { name: "Esquecer a configuração local?" });
    expect(dialog.textContent).toMatch(/apaga a configuração local.*reinicia o nó/i);
    expect(vi.mocked(api.post)).not.toHaveBeenCalled();

    const confirma = screen.getAllByRole("button", { name: "Esquecer rede" });
    fireEvent.click(confirma[confirma.length - 1]);

    await waitFor(() => expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/ui/network/leave"));
    expect(vi.mocked(api.post)).not.toHaveBeenCalledWith("/api/ui/leave");
  });

  it("mostra o estado do firewall no Windows e oferece retry", async () => {
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

  it("mostra a rota em uso e se ela é direta ou pelo relé, não o endereço anunciado", () => {
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
