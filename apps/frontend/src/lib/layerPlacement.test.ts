import { describe, expect, it } from "vitest";

import { layerPlacement } from "./layerPlacement";

describe("layerPlacement", () => {
  it("separa as camadas da GPU das que sobraram na CPU", () => {
    // O caso do captain: 64 camadas servidas, 55 couberam na GPU.
    expect(layerPlacement(0, 63, 55)).toEqual({ served: 64, gpu: 55, cpu: 9 });
  });

  it("trata negativo como 'todas na GPU', não como nenhuma", () => {
    // -1 é a convenção do llama.cpp para "descarregue tudo". Lido como zero,
    // o card diria que a máquina inteira roda na CPU.
    expect(layerPlacement(0, 63, -1)).toEqual({ served: 64, gpu: 64, cpu: 0 });
  });

  it("não deixa o pedido exceder o que a máquina serve", () => {
    // 99 é o sentinela "tudo"; a fatia tem 16 camadas, não 99.
    expect(layerPlacement(0, 15, 99)).toEqual({ served: 16, gpu: 16, cpu: 0 });
  });

  it("conta a faixa a partir da camada inicial, não do zero", () => {
    expect(layerPlacement(15, 31, 10)).toEqual({ served: 17, gpu: 10, cpu: 7 });
  });

  it("cala quando o motor não relatou nada", () => {
    // Sem relato não há palpite: dizer "tudo na GPU" sem saber é o defeito.
    expect(layerPlacement(0, 63, null)).toBeNull();
    expect(layerPlacement(null, null, 55)).toBeNull();
  });

  it("cala quando a faixa não faz sentido", () => {
    expect(layerPlacement(10, 4, 55)).toBeNull();
  });
});
