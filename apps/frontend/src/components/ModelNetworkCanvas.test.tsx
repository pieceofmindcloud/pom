// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { edgeStyle, isControlTarget, ModelNetworkCanvas } from "./ModelNetworkCanvas";
import { LocaleProvider } from "../i18n/provider";
import type { ClusterView } from "../lib/types";
import { fakeCluster, fakeNode, fakePeer, fakeSlicePeer } from "../test/fixtures";
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

function mount(view: ClusterView = cluster) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <ModelNetworkCanvas cluster={view} />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

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
