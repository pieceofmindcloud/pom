import { describe, expect, it } from "vitest";

import { layerPlacement } from "./layerPlacement";

describe("layerPlacement", () => {
  it("separa as camadas da GPU das que sobraram na CPU", () => {
    // O caso do captain: 64 camadas servidas, 55 couberam na GPU.
    expect(layerPlacement(0, 63, 55, true)).toEqual({ served: 64, gpu: 55, cpu: 9 });
  });

  it("trata negativo como 'todas na GPU', não como nenhuma", () => {
    // -1 é a convenção do llama.cpp para "descarregue tudo". Lido como zero,
    // o card diria que a máquina inteira roda na CPU.
    expect(layerPlacement(0, 63, -1, true)).toEqual({ served: 64, gpu: 64, cpu: 0 });
  });

  it("não deixa o pedido exceder o que a máquina serve", () => {
    // 99 é o sentinela "tudo"; a fatia tem 16 camadas, não 99.
    expect(layerPlacement(0, 15, 99, true)).toEqual({ served: 16, gpu: 16, cpu: 0 });
  });

  it("conta a faixa a partir da camada inicial, não do zero", () => {
    expect(layerPlacement(15, 31, 10, true)).toEqual({ served: 17, gpu: 10, cpu: 7 });
  });

  it("cala quando o motor não relatou nada", () => {
    // Sem relato não há palpite: dizer "tudo na GPU" sem saber é o defeito.
    expect(layerPlacement(0, 63, null, true)).toBeNull();
    expect(layerPlacement(null, null, 55, true)).toBeNull();
  });

  it("cala quando a faixa não faz sentido", () => {
    expect(layerPlacement(10, 4, 55, true)).toBeNull();
  });

  it("cala numa máquina sem GPU, mesmo com um relato positivo de camadas", () => {
    // O caso do captain: Windows sem placa nenhuma mostrando "25 na GPU" -
    // o número relatado era o sentinela do pedido, não uma leitura real.
    expect(layerPlacement(0, 63, 25, false)).toBeNull();
  });
});
