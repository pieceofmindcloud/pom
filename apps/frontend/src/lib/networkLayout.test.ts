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
  it("keeps the arrangement when a node merely restarted", () => {
    const before = [{ node_id: "node-a", start_layer: 0, end_layer: 15, changed_at: "2026-08-12T10:00:00Z" }];
    const after = [{ node_id: "node-a", start_layer: 0, end_layer: 15, changed_at: "2026-08-17T09:30:00Z" }];

    expect(deploymentLayoutKey("qwen", before)).toBe(deploymentLayoutKey("qwen", after));
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

  it("puts one column per layer range, in pipeline order", () => {
    const positions = autoLayout([ksia, macbook, salem]);
    expect(positions.macbook.left).toBeLessThan(positions.salem.left);
    expect(positions.salem.left).toBeLessThan(positions.ksia.left);
  });

  /** A ordem de entrada não pode mudar o desenho: a rede se reordena sozinha
   * a cada refresh, e um layout que dependesse disso tremeria na tela. */
  it("does not depend on the order the nodes arrived in", () => {
    const a = autoLayout([macbook, salem, salem2, ksia]);
    const b = autoLayout([ksia, salem2, macbook, salem]);
    expect(a).toEqual(b);
  });

  it("stacks replicas of the same range in the same column", () => {
    const positions = autoLayout([macbook, salem, salem2, ksia]);
    expect(positions.salem.left).toBe(positions.salem2.left);
    expect(positions.salem.top).not.toBe(positions.salem2.top);
  });

  /** Duas réplicas ficam simétricas em torno do eixo do estágio anterior, que
   * é o que faz a bifurcação parecer uma bifurcação. */
  it("centers the column vertically, whatever its height", () => {
    const positions = autoLayout([macbook, salem, salem2, ksia]);
    const axis = (id: string) => positions[id].top;
    const middle = (axis("salem") + axis("salem2")) / 2;
    expect(Math.abs(middle - axis("macbook"))).toBeLessThan(1);
    expect(Math.abs(middle - axis("ksia"))).toBeLessThan(1);
  });

  it("never puts a card on top of another in the same column", () => {
    const positions = autoLayout([salem, salem2]);
    expect(Math.abs(positions.salem.top - positions.salem2.top)).toBeGreaterThanOrEqual(202);
  });

  /** Duas rotas alternativas: 16-30 seguido de 31-40, ou 16-40 direto. As
   * faixas que começam em 16 dividem a coluna, e a rota de dois estágios
   * segue reta na linha da primeira. */
  it("puts alternative ranges that start at the same point in the same column", () => {
    const head = { id: "ksia-gpu2", start_layer: 0, end_layer: 15 };
    const shortRange = { id: "salem-gpu1", start_layer: 16, end_layer: 30 };
    const longRange = { id: "ksia-gpu1", start_layer: 16, end_layer: 40 };
    const tail = { id: "salem-gpu2", start_layer: 31, end_layer: 40 };
    const positions = autoLayout([longRange, tail, head, shortRange]);
    expect(positions["salem-gpu1"].left).toBe(positions["ksia-gpu1"].left);
    expect(positions["ksia-gpu2"].left).toBeLessThan(positions["salem-gpu1"].left);
    expect(positions["salem-gpu1"].left).toBeLessThan(positions["salem-gpu2"].left);
    expect(positions["salem-gpu1"].top).toBe(positions["ksia-gpu2"].top);
    expect(positions["salem-gpu2"].top).toBe(positions["ksia-gpu2"].top);
    expect(positions["ksia-gpu1"].top).toBeGreaterThan(positions["salem-gpu1"].top);
  });

  it("an empty list does not break", () => {
    expect(autoLayout([])).toEqual({});
  });

  /** Nó sem faixa (anunciando 0-0) ainda precisa de um lugar: some da tela é
   * pior que aparecer numa coluna própria. */
  it("makes room for whoever serves no range", () => {
    const positions = autoLayout([{ id: "empty", start_layer: 0, end_layer: 0 }]);
    expect(Number.isFinite(positions.empty.left)).toBe(true);
    expect(Number.isFinite(positions.empty.top)).toBe(true);
  });

  it("places the virtual gateway after the last column", () => {
    const peers = [
      { left: 48, top: 96 },
      { left: 404, top: 136 },
    ];
    const gateway = pipelineGatewayPosition(peers);
    expect(gateway).not.toBeNull();
    expect(gateway!.left).toBe(404 + CARD_WIDTH + 148);
    expect(gateway!.top + GATEWAY_HEIGHT / 2).toBe((96 + CARD_HEIGHT / 2 + 136 + CARD_HEIGHT / 2) / 2);
  });

  it("keeps the arrow of the final leg pointing right", () => {
    const from = { left: 404, top: 136 };
    const gateway = { left: 760, top: 136 };
    const geometry = gatewayRouteGeometry(from, gateway);
    expect(geometry.x1).toBe(from.left + CARD_WIDTH);
    expect(geometry.x2).toBe(gateway.left);
    expect(geometry.c1x).toBeGreaterThan(geometry.x1);
    expect(geometry.c2x).toBeLessThan(geometry.x2);
  });

  it("preserves existing positions when a new peer enters the canvas", () => {
    const current = { ksia: { left: 520, top: 140 } };
    const persisted = { ksia: { left: 100, top: 100 } };
    const arrangement = {
      ksia: { left: 100, top: 100 },
      salem: { left: 340, top: 100 },
    };

    expect(mergePositions(current, persisted, arrangement, ["ksia", "salem"])).toEqual({
      ksia: { left: 520, top: 140 },
      salem: { left: 340, top: 100 },
    });
  });
});
