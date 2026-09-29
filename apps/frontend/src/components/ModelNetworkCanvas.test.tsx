// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { edgeStyle, isControlTarget, ModelNetworkCanvas } from "./ModelNetworkCanvas";
import type { PipelineCycleCost, PipelineRouteCost } from "./PipelineGatewayNode";
import { LocaleProvider } from "../i18n/provider";
import type { ChatTelemetryPeerPerf, ChatTelemetryRtt, ClusterView } from "../lib/types";
import { fakeCluster, fakeNode, fakePeer, fakeSlicePeer, fakeSystem } from "../test/fixtures";
import { deploymentLayoutKey, writePositions } from "../lib/networkLayout";

describe("edgeStyle", () => {
  it("pinta a rota principal de verde, sólida e com seta", () => {
    const s = edgeStyle("principal");
    expect(s.className).toContain("stroke-aqua");
    expect(s.strokeDasharray).toBeUndefined();
    expect(s.markerEnd).toBeTruthy();
  });

  it("pinta rota paralela de azul, sólida e sem seta", () => {
    const s = edgeStyle("paralela");
    expect(s.className).toContain("stroke-iris");
    expect(s.strokeDasharray).toBeUndefined();
    expect(s.markerEnd).toBeUndefined();
  });

  it("pinta rota inativa de cinza tracejado", () => {
    const s = edgeStyle("inativa");
    expect(s.className).toContain("stroke-faint");
    expect(s.strokeDasharray).toBe("7 7");
  });
});

const cluster = fakeCluster([fakeSlicePeer("a", 0, 15), fakeSlicePeer("b", 16, 31)]);

function mount(
  view: ClusterView = cluster,
  rttMeasurements: ChatTelemetryRtt[] = [],
  pipelineCost?: PipelineCycleCost,
  properties?: { open: boolean; onToggle: () => void },
  pipelineRoutes?: PipelineRouteCost[],
  peerPerf?: Record<string, ChatTelemetryPeerPerf>,
) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
      <ModelNetworkCanvas
        cluster={view}
        rttMeasurements={rttMeasurements}
        pipelineCost={pipelineCost}
        pipelineRoutes={pipelineRoutes}
        peerPerf={peerPerf}
        propertiesOpen={properties?.open}
        onToggleProperties={properties?.onToggle}
      />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

describe("ModelNetworkCanvas: RTT das arestas", () => {
  afterEach(cleanup);

  it("mostra o RTT direcional no mesmo selo do mapa de implantação", () => {
    const view = fakeCluster([
      fakeSlicePeer("a", 0, 15),
      fakeSlicePeer("b", 16, 31),
    ]);
    view.routing_edges = [{
      from_node_id: "a",
      to_node_id: "b",
      model_id: "qwen35",
      from_end_layer: 15,
      to_start_layer: 16,
    }];

    mount(view, [{
      from_node_id: "a",
      to_node_id: "b",
      rtt_ms: 7.4,
      transport: "libp2p",
    }]);

    expect(screen.getByText("RTT 7.4 ms")).toBeTruthy();
  });

  it("desenha o retorno do último peer para o gateway sem arestas reversas", () => {
    const view = fakeCluster([
      fakeSlicePeer("a", 0, 15),
      fakeSlicePeer("b", 16, 31),
      fakeSlicePeer("c", 32, 47),
    ]);
    view.routing_edges = [
      { from_node_id: "a", to_node_id: "b", model_id: "qwen35", from_end_layer: 15, to_start_layer: 16 },
      { from_node_id: "b", to_node_id: "c", model_id: "qwen35", from_end_layer: 31, to_start_layer: 32 },
      { from_node_id: "c", to_node_id: "b", model_id: "qwen35", from_end_layer: 47, to_start_layer: 16 },
    ];

    const { container } = mount(view, [
      { from_node_id: "a", to_node_id: "b", rtt_ms: 30, transport: "https" },
      { from_node_id: "b", to_node_id: "c", rtt_ms: 31, transport: "https" },
      { from_node_id: "c", to_node_id: "gateway", rtt_ms: 29, transport: "https" },
    ]);

    expect(screen.getByRole("heading", { name: "Gateway" })).toBeTruthy();
    expect(container.querySelectorAll('svg[aria-hidden="true"] path[marker-end]').length).toBe(3);
    expect(screen.getByText("RTT 29.0 ms")).toBeTruthy();
    expect(screen.queryByText("RTT 31.0 ms")).toBeTruthy();
    const gateway = screen.getByRole("heading", { name: "Gateway" }).closest("article") as HTMLElement;
    const lastPeer = Array.from(container.querySelectorAll("h3")).find((heading) => heading.textContent === "c")?.closest("article") as HTMLElement;
    expect(parseFloat(gateway.style.left)).toBeGreaterThan(parseFloat(lastPeer.style.left));
  });

  it("mostra no gateway o custo completo das pernas e do compute", () => {
    const view = fakeCluster([
      fakeSlicePeer("a", 0, 15),
      fakeSlicePeer("b", 16, 31),
      fakeSlicePeer("c", 32, 47),
    ]);
    const cost: PipelineCycleCost = {
      legsMs: [20, 30, 31, 29],
      computeMs: 40,
    };

    mount(view, [], cost);

    expect(screen.getByText("Total do pipeline: 150.0 ms")).toBeTruthy();
    expect(screen.getByText("rede 110.0 ms · compute 40.0 ms")).toBeTruthy();
  });

  it("mostra todas as rotas de cabeca, ordenadas pelo tempo por token do backend", () => {
    const view = fakeCluster([
      fakeSlicePeer("head-fast", 0, 15),
      fakeSlicePeer("head-slow", 0, 15),
      fakeSlicePeer("tail", 16, 31),
    ]);
    view.routing_edges = [
      { from_node_id: "head-fast", to_node_id: "tail", model_id: "qwen35", from_end_layer: 15, to_start_layer: 16 },
      { from_node_id: "head-slow", to_node_id: "tail", model_id: "qwen35", from_end_layer: 15, to_start_layer: 16 },
    ];

    mount(view, [], undefined, undefined, [
      { nodeIds: ["head-slow", "tail", "gateway"], legsMs: [18, 12, 3], computeMs: null, networkMs: 16.5, returnMode: "direct" },
      { nodeIds: ["head-fast", "tail", "gateway"], legsMs: [4, 12, 3], computeMs: null, networkMs: 9.5, returnMode: "direct" },
    ]);

    const gateway = screen.getByRole("heading", { name: "Gateway" }).closest("article") as HTMLElement;
    expect(gateway.textContent).not.toContain("não medido");
    // A soma das pernas (19 e 33 ms) contaria ida e volta inteiras.
    expect(gateway.textContent).toMatch(/head-fast.*9\.5 ms/);
    expect(gateway.textContent).toMatch(/head-slow.*16\.5 ms/);
    expect(gateway.textContent?.indexOf("head-fast")).toBeLessThan(gateway.textContent?.indexOf("head-slow") ?? -1);
  });
});

describe("ModelNetworkCanvas: velocidade das máquinas", () => {
  afterEach(cleanup);

  it("mostra o TPS de decode e prefill de cada máquina e o tok/s da melhor rota", () => {
    const view = fakeCluster([fakeSlicePeer("head", 0, 15), fakeSlicePeer("tail", 16, 31)]);
    mount(view, [], undefined, undefined, [
      { nodeIds: ["head", "tail", "gateway"], legsMs: [4, 12, 4], computeMs: 30, networkMs: 20, totalMs: 50, returnMode: "ring" },
    ], {
      head: { decode_tokens_per_sec: 66.7, prefill_tokens_per_sec: 1250, active_generations: 0 },
    });

    const cards = screen.getAllByTestId("machine-tps").map((element) => element.textContent);
    expect(cards).toContain("decode 66.7 · prefill 1250 tok/s");
    expect(cards).toContain("velocidade não medida");
    expect(screen.getByText("Total do pipeline: 50.0 ms")).toBeTruthy();
    expect(screen.getByTestId("pipeline-throughput").textContent).toBe("≈ 20.0 tok/s por ciclo de token");
    expect(screen.getByTestId("pipeline-route-breakdown").textContent).toBe("rede 20.0 ms (anel) · compute 30.0 ms");
  });
});

describe("ModelNetworkCanvas: rota por relé", () => {
  afterEach(cleanup);

  it("pinta de laranja a aresta por relé e explica a lentidão no gateway", () => {
    const view = fakeCluster([fakeSlicePeer("head", 0, 15), fakeSlicePeer("tail", 16, 31)]);
    view.routing_edges = [
      { from_node_id: "head", to_node_id: "tail", model_id: "qwen35", from_end_layer: 15, to_start_layer: 16 },
    ];
    const { container } = mount(view, [
      { from_node_id: "head", to_node_id: "tail", rtt_ms: 67, transport: "libp2p-tunnel", relayed: true },
      { from_node_id: "tail", to_node_id: "gateway", rtt_ms: 3, transport: "quic" },
    ], undefined, undefined, [
      { nodeIds: ["head", "tail", "gateway"], legsMs: [4, 67, 3], computeMs: 30, networkMs: 37, totalMs: 67, returnMode: "direct", relayed: true, relayedLegs: [["head", "tail"]] },
    ]);

    const label = screen.getByTestId("route-relay-label");
    expect(label.textContent).toContain("RELAY");
    expect(label.textContent).toContain("RTT 67.0 ms");
    expect(label.getAttribute("title")).toContain("peer de relé");
    expect(container.querySelectorAll('path[data-relayed="true"]').length).toBe(1);
    expect(screen.getByTestId("pipeline-route-relay").textContent).toBe("RELAY");
    expect(screen.getByTestId("pipeline-relay-warning").textContent).toContain("Rota por relé");
    // O texto longo só aparece no card, ao clicar no ícone.
    expect(screen.queryByTestId("pipeline-relay-popup")).toBeNull();
    fireEvent.click(screen.getByTestId("pipeline-relay-info"));
    expect(screen.getByTestId("pipeline-relay-legs").textContent).toBe("Pernas por relé: head → tail");
    expect(screen.getByTestId("pipeline-relay-popup").textContent).toContain("não consegue contornar");
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByTestId("pipeline-relay-popup")).toBeNull();
  });

  it("diz quando o anel contorna o relé da volta direta", () => {
    const view = fakeCluster([fakeSlicePeer("gw", 0, 15), fakeSlicePeer("tail", 16, 31)]);
    mount(view, [], undefined, undefined, [
      { nodeIds: ["gw", "tail", "gw"], legsMs: [0, 30, 52], computeMs: 90, networkMs: 30, totalMs: 120, returnMode: "ring", relayBypassedByRing: true },
    ]);
    expect(screen.getByTestId("pipeline-relay-warning").textContent).toContain("Relé contornado pelo anel");
    fireEvent.click(screen.getByTestId("pipeline-relay-info"));
    expect(screen.getByTestId("pipeline-relay-bypass").textContent).toContain("tail → gw");
    expect(screen.getByTestId("pipeline-relay-popup").textContent).toContain("volta pelo anel");
  });

  it("não alerta quando nenhuma perna passa por relé", () => {
    const view = fakeCluster([fakeSlicePeer("head", 0, 15), fakeSlicePeer("tail", 16, 31)]);
    mount(view, [], undefined, undefined, [
      { nodeIds: ["head", "tail", "gateway"], legsMs: [4, 12, 3], computeMs: 30, networkMs: 9.5, totalMs: 39.5, returnMode: "direct" },
    ]);
    expect(screen.queryByTestId("pipeline-relay-warning")).toBeNull();
    expect(screen.queryByTestId("route-relay-label")).toBeNull();
  });
});

describe("ModelNetworkCanvas: propriedades da implantação", () => {
  afterEach(cleanup);

  it("mantém o toggle compacto dentro do mapa", () => {
    const onToggle = vi.fn();
    const { container } = mount(cluster, [], undefined, { open: true, onToggle });
    const canvas = container.querySelector("section");
    const button = screen.getByRole("button", { name: "Ocultar propriedades da implantação" });

    expect(button.textContent).toBe("");
    expect(button.className).toContain("w-8");
    expect(button.closest("section")).toBe(canvas);
    fireEvent.click(button);
    expect(onToggle).toHaveBeenCalledTimes(1);
  });
});

/**
 * Quantas camadas foram parar na CPU tem que aparecer no próprio card.
 *
 * Antes, isso só existia atrás do duplo clique: a inferência ficava lenta
 * porque parte do modelo não coube na VRAM, e o card - que mostra VRAM, RAM e
 * carga - não dizia nada. O resumo aqui não substitui o detalhe; ele existe
 * para o operador não precisar abrir máquina por máquina para descobrir qual
 * delas está rodando meio na CPU.
 */
describe("ModelNetworkCanvas: camadas na GPU e na CPU", () => {
  afterEach(cleanup);

  function withGpuLayers(reported: number | null) {
    const peer = fakeSlicePeer("a", 0, 63);
    return fakeCluster([
      {
        ...peer,
        node: { ...peer.node!, status: { ...peer.node!.status, reported_gpu_layers: reported } },
      },
    ]);
  }

  it("mostra a divisão e destaca o que sobrou na CPU", () => {
    // 64 camadas servidas, 55 na GPU: o caso que o captain relatou.
    mount(withGpuLayers(55));
    expect(screen.getByText("55 GPU · 9 CPU")).toBeTruthy();
    // Âmbar é o destaque: é o sinal que interessa de relance.
    expect(screen.getByText("55 GPU · 9 CPU").className).toContain("text-amber");
    expect(screen.getByLabelText(/9 camadas não couberam na GPU/)).toBeTruthy();
  });

  it("não alarma quando tudo coube na GPU", () => {
    mount(withGpuLayers(-1));
    const value = screen.getByText("64 na GPU");
    expect(value.className).not.toContain("text-amber");
    expect(screen.queryByLabelText(/não couberam na GPU/)).toBeNull();
  });

  it("cala quando o motor não relatou nada, em vez de chutar", () => {
    mount(withGpuLayers(null));
    expect(screen.queryByText(/GPU ·/)).toBeNull();
    expect(screen.queryByText(/na GPU$/)).toBeNull();
  });

  it("cala numa máquina sem GPU, mesmo com camadas relatadas na GPU", () => {
    // O caso do captain: Windows sem placa nenhuma mostrando "25 na GPU" - o
    // motor relatava o sentinela do pedido, não uma leitura real do hardware.
    const peer = fakeSlicePeer("windows-no-gpu", 0, 63);
    const cluster = fakeCluster([
      {
        ...peer,
        node: {
          ...peer.node!,
          status: { ...peer.node!.status, reported_gpu_layers: 25 },
          system: { ...peer.node!.system, gpu_kind: "none", gpus: [] },
        },
      },
    ]);
    mount(cluster);
    expect(screen.queryByText(/GPU ·/)).toBeNull();
    expect(screen.queryByText(/na GPU$/)).toBeNull();
  });
});

describe("ModelNetworkCanvas: temperatura", () => {
  afterEach(cleanup);

  it("não mostra temperatura quando gpu_kind informa que a máquina não tem GPU", () => {
    const peer = fakeSlicePeer("cpu-only", 0, 31, {
      node: fakeNode({
        node_id: "cpu-only",
        short_id: "cpu-only",
        system: {
          ...fakeNode().system,
          hostname: "cpu-only",
          gpu_kind: "none",
          gpus: [{
            index: 0,
            name: "stale device",
            vendor: "nvidia",
            vram_total_mb: null,
            vram_used_mb: null,
            vram_used_pct: null,
            utilization_pct: null,
            power_watts: null,
            temperature_c: null,
          }],
        },
      }),
    });

    mount(fakeCluster([peer]));

    expect(screen.queryByText(/temperatura da gpu/i)).toBeNull();
  });
});

/**
 * O card do Fluxo e a aba Detalhes mostram a mesma coisa, e o caminho até ela é
 * o duplo clique. Um clique só continua sendo seleção e arrasto.
 */
describe("ModelNetworkCanvas: detalhes por duplo clique", () => {
  afterEach(cleanup);

  it("nao inclui peer sem fatia no otherRanges da janela de detalhes", () => {
    const b = fakeSlicePeer("b", 0, 0);
    b.total_layers = 0;
    const view = fakeCluster([fakeSlicePeer("a", 5, 10), b], "qwen35");
    mount(view);

    const cards = document.querySelectorAll("article");
    expect(cards.length).toBeGreaterThanOrEqual(1);

    act(() => {
      fireEvent.doubleClick(cards[1]);
    });

    // The dialog shows the serving peer's real range, not the [0,0] that
    // a non-serving peer would contribute to otherRanges without the filter.
    const dialog = screen.getByRole("dialog");
    expect(dialog.textContent).toMatch(/L5.+10/);
  });

  it("nao abre nada antes do duplo clique", () => {
    mount();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("abre a janela da máquina no duplo clique do card", () => {
    mount();
    const card = document.querySelector("article")!;
    act(() => {
      fireEvent.doubleClick(card);
    });
    const dialog = screen.getByRole("dialog");
    expect(dialog.getAttribute("aria-label")).toContain("Detalhes de");
  });

  it("um clique simples não abre a janela", () => {
    mount();
    const card = document.querySelector("article")!;
    act(() => {
      fireEvent.click(card);
    });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  // A janela sai com animação, então "fechou" se afirma esperando a saída — e
  // não no tique seguinte ao clique.
  it("fecha pelo botão e pelo Esc", async () => {
    mount();
    const card = document.querySelector("article")!;
    act(() => {
      fireEvent.doubleClick(card);
    });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Fechar" }));
    });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    act(() => {
      fireEvent.doubleClick(card);
    });
    expect(screen.getByRole("dialog")).toBeTruthy();
    act(() => {
      fireEvent.keyDown(document, { key: "Escape" });
    });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("mostra a máquina em que se clicou, e não a primeira da lista", () => {
    mount({
      ...cluster,
      peers: [
        fakeSlicePeer("a", 0, 15, { node: fakeNode({ node_id: "a", short_id: "a", hostname: "salem" }) }),
        fakeSlicePeer("b", 16, 31, { node: fakeNode({ node_id: "b", short_id: "b", hostname: "ksia" }) }),
      ],
    });
    const cards = document.querySelectorAll("article");
    act(() => {
      fireEvent.doubleClick(cards[1]);
    });
    expect(screen.getByRole("dialog").getAttribute("aria-label")).toContain("ksia");
  });
});

/**
 * O duplo clique é do card, não dos controles dentro dele: o card do Fluxo não
 * tem botão hoje, e o gesto não pode virar armadilha para o primeiro que vier.
 */
describe("isControlTarget", () => {
  afterEach(cleanup);

  it("reconhece um clique que veio de dentro de um botão", () => {
    const host = document.createElement("div");
    host.innerHTML = "<article><button><span>Sair da rede</span></button></article>";
    document.body.append(host);
    expect(isControlTarget(host.querySelector("span"))).toBe(true);
    expect(isControlTarget(host.querySelector("article"))).toBe(false);
    host.remove();
  });

  it("não trata um alvo sem elemento como controle", () => {
    expect(isControlTarget(null)).toBe(false);
  });
});

/**
 * Uma implantação nova não tem nada salvo, e é justamente o momento em que a
 * tela precisa se explicar sozinha: ordem das camadas da esquerda para a
 * direita, réplicas empilhadas. A grade de três colunas que existia antes
 * punha três máquinas em fila e a quarta na linha de baixo, com a aresta entre
 * as pontas cruzando o canvas inteiro.
 */
describe("ModelNetworkCanvas: o desenho de uma implantação nova", () => {
  afterEach(() => {
    cleanup();
    localStorage.clear();
  });

  const quatroMaquinas = fakeCluster([
    fakeSlicePeer("macbook", 0, 18),
    fakeSlicePeer("salem", 19, 34),
    fakeSlicePeer("salem2", 19, 34),
    fakeSlicePeer("ksia", 35, 64),
  ]);

  function cardDe(container: HTMLElement, hostname: string) {
    const titulo = Array.from(container.querySelectorAll("h3")).find((h) => h.textContent === hostname);
    return titulo?.closest("article") as HTMLElement;
  }

  it("põe as faixas em colunas na ordem do pipeline, sem ninguém arrastar nada", () => {
    const { container } = mount(quatroMaquinas);
    const left = (nome: string) => parseFloat(cardDe(container, nome).style.left);

    expect(left("macbook")).toBeLessThan(left("salem"));
    expect(left("salem")).toBeLessThan(left("ksia"));
  });

  it("empilha as duas réplicas de L19-L34 na mesma coluna", () => {
    const { container } = mount(quatroMaquinas);
    const salem = cardDe(container, "salem");
    const salem2 = cardDe(container, "salem2");

    expect(salem.style.left).toBe(salem2.style.left);
    expect(salem.style.top).not.toBe(salem2.style.top);
  });

  /** A entrada e a saída ficam no eixo do meio das réplicas: é o que faz a
   * bifurcação parecer uma bifurcação em vez de uma escada. */
  it("centra quem não se ramifica no eixo de quem se ramifica", () => {
    const { container } = mount(quatroMaquinas);
    const top = (nome: string) => parseFloat(cardDe(container, nome).style.top);
    const eixoDasReplicas = (top("salem") + top("salem2")) / 2;

    expect(Math.abs(top("macbook") - eixoDasReplicas)).toBeLessThan(1);
    expect(Math.abs(top("ksia") - eixoDasReplicas)).toBeLessThan(1);
  });
});

/**
 * O arranjo feito à mão é uma decisão, e o botão que apaga decisões é o botão
 * que ninguém aperta.
 *
 * "Organizar" arruma o resto e respeita quem foi movido; quem quer descartar
 * tem o "Redefinir" ao lado. Antes disto não havia escolha: um reinício de nó
 * mudava a chave do layout e o arranjo sumia sozinho, sem ninguém pedir.
 */
describe("ModelNetworkCanvas: organizar e redefinir", () => {
  afterEach(() => {
    cleanup();
    localStorage.clear();
  });

  const rede = fakeCluster([
    fakeSlicePeer("macbook", 0, 18),
    fakeSlicePeer("salem", 19, 34),
    fakeSlicePeer("ksia", 35, 64),
  ]);

  function cardDe(container: HTMLElement, hostname: string) {
    const titulo = Array.from(container.querySelectorAll("h3")).find((h) => h.textContent === hostname);
    return titulo?.closest("article") as HTMLElement;
  }

  /**
   * Simula o que o arrasto grava. A chave do card não é o `node_id` cru: é o
   * `keyFor` do canvas, que inclui modelo e faixa.
   */
  const cardKey = (node_id: string, start: number, end: number) =>
    `${node_id}:qwen35:${start}:${end}`;

  function comArrasto(posicoes: Record<string, { left: number; top: number }>) {
    const peers = [
      { node_id: cardKey("macbook", 0, 18), start_layer: 0, end_layer: 18 },
      { node_id: cardKey("salem", 19, 34), start_layer: 19, end_layer: 34 },
      { node_id: cardKey("ksia", 35, 64), start_layer: 35, end_layer: 64 },
    ];
    writePositions(deploymentLayoutKey("qwen35", peers), posicoes);
  }

  it("organizar mantém onde o operador pôs, e arruma o resto", () => {
    comArrasto({ [cardKey("ksia", 35, 64)]: { left: 900, top: 640 } });
    const { container } = mount(rede);
    expect(cardDe(container, "ksia").style.top).toBe("640px");

    fireEvent.click(screen.getByLabelText(/Organizar pela ordem das camadas/i));

    expect(cardDe(container, "ksia").style.left).toBe("900px");
    expect(cardDe(container, "ksia").style.top).toBe("640px");
    // E o resto continua no arranjo por camadas.
    const left = (nome: string) => parseFloat(cardDe(container, nome).style.left);
    expect(left("macbook")).toBeLessThan(left("salem"));
  });

  it("redefinir descarta o arrasto e devolve o arranjo por camadas", () => {
    comArrasto({ [cardKey("ksia", 35, 64)]: { left: 900, top: 640 } });
    const { container } = mount(rede);
    expect(cardDe(container, "ksia").style.top).toBe("640px");

    fireEvent.click(screen.getByLabelText(/Redefinir o arranjo/i));

    expect(cardDe(container, "ksia").style.left).not.toBe("900px");
    const top = (nome: string) => parseFloat(cardDe(container, nome).style.top);
    expect(Math.abs(top("ksia") - top("macbook"))).toBeLessThan(1);
  });
});

/**
 * Duas placas na mesma caixa não são um detalhe do card grande: o card em
 * miniatura era onde a segunda GPU sumia. A VRAM aqui é a soma - é o que a
 * máquina tem a oferecer - e a temperatura ganha uma linha por placa, porque
 * uma pode estar fervendo enquanto a outra dorme.
 */
describe("ModelNetworkCanvas: máquina com mais de uma GPU", () => {
  afterEach(cleanup);

  const duasGpus = fakeCluster([
    fakeSlicePeer("a", 0, 31, {
      node: fakeNode({
        node_id: "a",
        short_id: "a",
        hostname: "ksia",
        system: fakeSystem({
          gpu_kind: "nvidia",
          gpus: [
            { index: 0, name: "RTX 5090", vendor: "nvidia", vram_total_mb: 32768, vram_used_mb: 8192, vram_used_pct: 25, utilization_pct: 30, power_watts: null, temperature_c: 61 },
            { index: 1, name: "RTX 3090", vendor: "nvidia", vram_total_mb: 24576, vram_used_mb: 4096, vram_used_pct: 16, utilization_pct: 10, power_watts: null, temperature_c: 78 },
          ],
        }),
      }),
    }),
  ]);

  it("soma a VRAM das duas placas em vez de mostrar só a primeira", () => {
    mount(duasGpus);
    // (8192 + 4096) / 1024 = 12,0 GiB de (32768 + 24576) / 1024 = 56,0 GiB.
    expect(screen.getByText("12.0 / 56.0 GiB")).toBeTruthy();
    expect(screen.getByText("VRAM · 2 GPUs")).toBeTruthy();
  });

  it("mostra a temperatura de cada placa, nomeada", () => {
    mount(duasGpus);
    expect(screen.getByText(/61,0 °C/)).toBeTruthy();
    expect(screen.getByText(/78,0 °C/)).toBeTruthy();
    expect(screen.getByText("RTX 5090")).toBeTruthy();
    expect(screen.getByText("RTX 3090")).toBeTruthy();
  });
});

describe("ModelNetworkCanvas: arrumação do card menor", () => {
  afterEach(cleanup);

  it("ordena VRAM, camadas, memória e carga antes das linhas de GPU", () => {
    const view = fakeCluster([
      fakeSlicePeer("compact", 0, 15, {
        load: 2,
        node: fakeNode({
          node_id: "compact",
          short_id: "compact",
          hostname: "compact-node",
          status: { state: "ready", detail: "servindo", reported_gpu_layers: 16 } as never,
          system: fakeSystem({
            gpu_kind: "nvidia",
            gpus: [{
              index: 0,
              name: "RTX A",
              vendor: "nvidia",
              vram_total_mb: 8192,
              vram_used_mb: 2048,
              vram_used_pct: 25,
              utilization_pct: 30,
              temperature_c: 61,
              power_watts: 250,
            } as never],
          }),
        }),
      }),
    ]);

    const { container } = mount(view);
    const card = container.querySelector("h3")?.closest("article") as HTMLElement;
    const meters = screen.getByTestId("machine-meter-group");
    const vram = screen.getByText("VRAM");
    const layerBar = screen.getByText("camadas");
    const memory = screen.getByText("Memória");
    const load = screen.getByText("Carga");
    const gpu = screen.getByText(/61,0 °C/);
    const status = screen.getByTestId("machine-status");
    const layers = screen.getByTestId("machine-layers");

    expect(meters.querySelectorAll('[aria-label]')).toHaveLength(4);
    expect(vram.compareDocumentPosition(layerBar) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(layerBar.compareDocumentPosition(memory) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(memory.compareDocumentPosition(load) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(load.compareDocumentPosition(gpu) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(status.parentElement).toBe(layers.parentElement);
    expect(status.compareDocumentPosition(layers) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(layers.className).toContain("text-[9px]");
    expect(card.className).toContain("p-3");
  });
});
