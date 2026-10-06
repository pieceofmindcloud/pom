// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import {
  autoLayout,
  clearPositions,
  deploymentLayoutKey,
  fallbackPosition,
  GATEWAY_HEIGHT,
  gatewayRouteGeometry,
  mergePositions,
  pipelineGatewayPosition,
  CARD_HEIGHT,
  CARD_WIDTH,
  writePositions,
} from "./networkLayout";

describe("network layout", () => {
  it("invalidates stored positions when a deployment model changes", () => {
    const peers = [
      { node_id: "node-a", start_layer: 0, end_layer: 15, changed_at: "2026-08-12T10:00:00Z" },
      { node_id: "node-b", start_layer: 16, end_layer: 31, changed_at: "2026-08-12T10:00:00Z" },
    ];

    expect(deploymentLayoutKey("qwen", peers)).not.toBe(deploymentLayoutKey("llama", peers));
  });

  it("invalidates stored positions when a peer receives a new range", () => {
    const peers = [{ node_id: "node-a", start_layer: 0, end_layer: 15, changed_at: "2026-08-12T10:00:00Z" }];
    const moved = [{ node_id: "node-a", start_layer: 0, end_layer: 31, changed_at: "2026-08-12T10:00:00Z" }];

    expect(deploymentLayoutKey("qwen", peers)).not.toBe(deploymentLayoutKey("qwen", moved));
  });

  /**
   * Reiniciar um nó mudava o `changed_at` dele, o que mudava a chave, o que
   * fazia `readPositions` voltar vazio: o arranjo que o operador tinha montado
   * à mão não era reorganizado, era **perdido**. A fatia e o modelo continuam
   * invalidando, porque aí a rede desenhada é outra.
   */
  it("mantém o arranjo quando um nó só reiniciou", () => {
    const antes = [{ node_id: "node-a", start_layer: 0, end_layer: 15, changed_at: "2026-08-12T10:00:00Z" }];
    const depois = [{ node_id: "node-a", start_layer: 0, end_layer: 15, changed_at: "2026-08-17T09:30:00Z" }];

    expect(deploymentLayoutKey("qwen", antes)).toBe(deploymentLayoutKey("qwen", depois));
  });

  it("places the automatic layout inside the canvas bounds", () => {
    expect(fallbackPosition(0)).toEqual({ left: 48, top: 96 });
    expect(fallbackPosition(5)).toEqual({ left: 668, top: 336 });
  });
});

describe("writePositions and clearPositions", () => {
  it("writePositions does not throw when localStorage is blocked", () => {
    vi.spyOn(localStorage, "setItem").mockImplementation(() => {
      throw new Error("quota exceeded");
    });
    expect(() => writePositions("any-key", { a: { left: 10, top: 20 } })).not.toThrow();
  });

  it("clearPositions does not throw when localStorage is blocked", () => {
    vi.spyOn(localStorage, "removeItem").mockImplementation(() => {
      throw new Error("access denied");
    });
    expect(() => clearPositions("any-key")).not.toThrow();
  });
});

/**
 * O arranjo automático existe porque a grade não conta a história do pipeline.
 *
 * O que a tela precisa mostrar é a ordem das camadas da esquerda para a
 * direita, e as réplicas de uma mesma faixa empilhadas na mesma coluna - é
 * assim que a aresta vira uma linha curta e legível em vez de atravessar o
 * canvas inteiro.
 */
describe("autoLayout", () => {
  const macbook = { id: "macbook", start_layer: 0, end_layer: 18 };
  const salem = { id: "salem", start_layer: 19, end_layer: 34 };
  const salem2 = { id: "salem2", start_layer: 19, end_layer: 34 };
  const ksia = { id: "ksia", start_layer: 35, end_layer: 64 };

  it("põe uma coluna por faixa de camadas, na ordem do pipeline", () => {
    const posicoes = autoLayout([ksia, macbook, salem]);
    expect(posicoes.macbook.left).toBeLessThan(posicoes.salem.left);
    expect(posicoes.salem.left).toBeLessThan(posicoes.ksia.left);
  });

  /** A ordem de entrada não pode mudar o desenho: a rede se reordena sozinha
   * a cada refresh, e um layout que dependesse disso tremeria na tela. */
  it("não depende da ordem em que os nós chegaram", () => {
    const a = autoLayout([macbook, salem, salem2, ksia]);
    const b = autoLayout([ksia, salem2, macbook, salem]);
    expect(a).toEqual(b);
  });

  it("empilha réplicas da mesma faixa na mesma coluna", () => {
    const posicoes = autoLayout([macbook, salem, salem2, ksia]);
    expect(posicoes.salem.left).toBe(posicoes.salem2.left);
    expect(posicoes.salem.top).not.toBe(posicoes.salem2.top);
  });

  /** Duas réplicas ficam simétricas em torno do eixo do estágio anterior, que
   * é o que faz a bifurcação parecer uma bifurcação. */
  it("centra a coluna verticalmente, qualquer que seja a altura dela", () => {
    const posicoes = autoLayout([macbook, salem, salem2, ksia]);
    const eixo = (id: string) => posicoes[id].top;
    const meio = (eixo("salem") + eixo("salem2")) / 2;
    expect(Math.abs(meio - eixo("macbook"))).toBeLessThan(1);
    expect(Math.abs(meio - eixo("ksia"))).toBeLessThan(1);
  });

  it("nunca põe um card em cima do outro na mesma coluna", () => {
    const posicoes = autoLayout([salem, salem2]);
    expect(Math.abs(posicoes.salem.top - posicoes.salem2.top)).toBeGreaterThanOrEqual(202);
  });

  it("uma lista vazia não quebra", () => {
    expect(autoLayout([])).toEqual({});
  });

  /** Nó sem faixa (anunciando 0-0) ainda precisa de um lugar: some da tela é
   * pior que aparecer numa coluna própria. */
  it("dá lugar a quem não serve faixa nenhuma", () => {
    const posicoes = autoLayout([{ id: "vazio", start_layer: 0, end_layer: 0 }]);
    expect(Number.isFinite(posicoes.vazio.left)).toBe(true);
    expect(Number.isFinite(posicoes.vazio.top)).toBe(true);
  });

  it("coloca o gateway virtual depois da última coluna", () => {
    const peers = [
      { left: 48, top: 96 },
      { left: 404, top: 136 },
    ];
    const gateway = pipelineGatewayPosition(peers);
    expect(gateway).not.toBeNull();
    expect(gateway!.left).toBe(404 + CARD_WIDTH + 148);
    expect(gateway!.top + GATEWAY_HEIGHT / 2).toBe((96 + CARD_HEIGHT / 2 + 136 + CARD_HEIGHT / 2) / 2);
  });

  it("mantém a seta da perna final apontando para a direita", () => {
    const from = { left: 404, top: 136 };
    const gateway = { left: 760, top: 136 };
    const geometry = gatewayRouteGeometry(from, gateway);
    expect(geometry.x1).toBe(from.left + CARD_WIDTH);
    expect(geometry.x2).toBe(gateway.left);
    expect(geometry.c1x).toBeGreaterThan(geometry.x1);
    expect(geometry.c2x).toBeLessThan(geometry.x2);
  });

  it("preserva posições existentes quando um peer novo entra no canvas", () => {
    const atual = { ksia: { left: 520, top: 140 } };
    const persistido = { ksia: { left: 100, top: 100 } };
    const arranjo = {
      ksia: { left: 100, top: 100 },
      salem: { left: 340, top: 100 },
    };

    expect(mergePositions(atual, persistido, arranjo, ["ksia", "salem"])).toEqual({
      ksia: { left: 520, top: 140 },
      salem: { left: 340, top: 100 },
    });
  });
});
