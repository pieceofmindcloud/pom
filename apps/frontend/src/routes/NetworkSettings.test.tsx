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
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { NetworkSettings, TlsPanel } from "./NetworkSettings";

const queryMocks = vi.hoisted(() => ({
  node: {
    data: {
      hostname: "salem",
      node_id: "a",
      p2p_port: 42777,
      gateway_port: 8080,
      inference_port: 8181,
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
}));

vi.mock("../lib/api", () => ({ api: { post: vi.fn() } }));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), fromError: vi.fn() }),
}));
vi.mock("../lib/queries", () => ({
  useNode: () => queryMocks.node,
  useCluster: () => queryMocks.cluster,
  useAdminNodes: () => queryMocks.nodes,
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
    expect(screen.getByText("Criptografia entre nós")).toBeTruthy();
  });
});
