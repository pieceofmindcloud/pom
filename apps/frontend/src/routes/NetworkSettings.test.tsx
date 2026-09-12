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
import { NetworkSettings } from "./NetworkSettings";
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
    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    expect(screen.getByText("Sobre esta rede")).toBeTruthy();
    expect(screen.getByText("Rede de teste")).toBeTruthy();
    expect(screen.getByText("Somente leitura")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Salvar detalhes da rede" })).toBeNull();
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

  it("separa esquecer a configuração local de sair do cluster", async () => {
    queryMocks.node.data.profile = "admin";
    queryMocks.node.data.authority_level = "primary";
    queryMocks.networkInfo.data.can_edit = true;
    vi.clearAllMocks();

    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    expect(screen.getByRole("button", { name: "Sair da rede" })).toBeTruthy();
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

  it("mantém Sair da rede como saída da máquina do cluster", async () => {
    queryMocks.node.data.profile = "admin";
    queryMocks.node.data.authority_level = "primary";
    queryMocks.networkInfo.data.can_edit = true;
    vi.clearAllMocks();

    render(
      <LocaleProvider initialLocale="pt-BR">
        <NetworkSettings />
      </LocaleProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Sair da rede" }));

    await waitFor(() => expect(vi.mocked(api.post)).toHaveBeenCalledWith("/api/ui/leave"));
    expect(vi.mocked(api.post)).not.toHaveBeenCalledWith("/api/ui/network/leave");
  });
});
