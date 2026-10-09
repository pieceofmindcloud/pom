import { describe, expect, it } from "vitest";

import { layerPlacement } from "./layerPlacement";

describe("layerPlacement", () => {
  it("separates the GPU layers from the ones left on the CPU", () => {
    // O caso do captain: 64 camadas servidas, 55 couberam na GPU.
    expect(layerPlacement(0, 63, 55, true)).toEqual({ served: 64, gpu: 55, cpu: 9 });
  });

  it("treats negative as 'all on the GPU', not as none", () => {
    // -1 é a convenção do llama.cpp para "descarregue tudo". Lido como zero,
    // o card diria que a máquina inteira roda na CPU.
    expect(layerPlacement(0, 63, -1, true)).toEqual({ served: 64, gpu: 64, cpu: 0 });
  });

  it("does not let the request exceed what the machine serves", () => {
    // 99 é o sentinela "tudo"; a fatia tem 16 camadas, não 99.
    expect(layerPlacement(0, 15, 99, true)).toEqual({ served: 16, gpu: 16, cpu: 0 });
  });

  it("counts the range from the starting layer, not from zero", () => {
    expect(layerPlacement(15, 31, 10, true)).toEqual({ served: 17, gpu: 10, cpu: 7 });
  });

  it("stays quiet when the engine reported nothing", () => {
    // Sem relato não há palpite: dizer "tudo na GPU" sem saber é o defeito.
    expect(layerPlacement(0, 63, null, true)).toBeNull();
    expect(layerPlacement(null, null, 55, true)).toBeNull();
  });

  it("stays quiet when the range makes no sense", () => {
    expect(layerPlacement(10, 4, 55, true)).toBeNull();
  });

  it("stays quiet on a machine without GPU, even with a positive layer report", () => {
    // O caso do captain: Windows sem placa nenhuma mostrando "25 na GPU" -
    // o número relatado era o sentinela do pedido, não uma leitura real.
    expect(layerPlacement(0, 63, 25, false)).toBeNull();
  });
});
