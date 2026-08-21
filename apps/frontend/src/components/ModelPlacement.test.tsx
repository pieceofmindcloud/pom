// @vitest-environment jsdom
/**
 * Uma camada é "na GPU" pelos bytes dela, não pelo dispositivo do bloco.
 *
 * Esta é a única regra que a seção tem, e é a que separa o que a tela mostra do
 * que `n_gpu_layers` diria: com um override de buffer type, o llama.cpp deixa o
 * bloco atribuído à GPU e manda parte dos pesos - os experts de um MoE - para a
 * CPU. Contar essa camada como descarregada faria a conta de memória de quem lê
 * a tela não fechar.
 */
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { ModelPlacementSection, isCpuBuffer, kvCacheBuffers, placementOf } from "./ModelPlacement";
import { LocaleProvider } from "../i18n/provider";
import type { ContextMemoryBuffer, ModelPlacement } from "../lib/types";

describe("placementOf", () => {
  it("chama de GPU a camada sem nenhum byte em buffer de host", () => {
    expect(placementOf([{ buffer: "CUDA0", bytes: 100 }])).toBe("gpu");
  });

  it("chama de CPU a camada que o mmap deixou no host", () => {
    expect(placementOf([{ buffer: "CPU_Mapped", bytes: 100 }])).toBe("cpu");
  });

  it("chama de dividida a camada com experts na CPU e o resto na GPU", () => {
    expect(
      placementOf([
        { buffer: "CUDA0", bytes: 90 },
        { buffer: "CPU", bytes: 10 },
      ]),
    ).toBe("mixed");
  });

  it("não inventa colocação para um grupo sem bytes", () => {
    expect(placementOf([])).toBe("unknown");
  });
});

describe("isCpuBuffer", () => {
  it("reconhece o buffer do host com e sem sufixo", () => {
    expect(isCpuBuffer("CPU")).toBe(true);
    expect(isCpuBuffer("CPU_Mapped")).toBe(true);
  });

  it("não confunde um backend cujo nome apenas começa parecido", () => {
    expect(isCpuBuffer("CUDA0")).toBe(false);
    expect(isCpuBuffer("Metal")).toBe(false);
  });
});

describe("kvCacheBuffers", () => {
  it("deixa de fora o buffer que não guarda contexto nenhum", () => {
    const buffers: ContextMemoryBuffer[] = [
      { buffer: "CUDA0", device: "CUDA0", model: 100, context: 40, compute: 5 },
      { buffer: "CPU", device: "CPU", model: 10, context: 0, compute: 2 },
    ];
    expect(kvCacheBuffers(buffers).map((b) => b.buffer)).toEqual(["CUDA0"]);
  });
});

function mount(placement: ModelPlacement | null, contextMemory: ContextMemoryBuffer[] | null) {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <ModelPlacementSection
        placement={placement}
        contextMemory={contextMemory}
        offloadKqv
        fitApplied={false}
      />
    </LocaleProvider>,
  );
}

const parcial: ModelPlacement = {
  n_layer: 3,
  n_gpu_layers: 2,
  has_tensor_overrides: true,
  devices: [{ name: "CUDA0", description: "NVIDIA GeForce RTX 4090" }],
  layers: [
    { layer: 0, device: "CUDA0", buffers: [{ buffer: "CUDA0", bytes: 100 }] },
    {
      layer: 1,
      device: "CUDA0",
      buffers: [
        { buffer: "CUDA0", bytes: 90 },
        { buffer: "CPU", bytes: 10 },
      ],
    },
    { layer: 2, device: "CPU", buffers: [{ buffer: "CPU_Mapped", bytes: 100 }] },
  ],
  output: { device: "CUDA0", buffers: [{ buffer: "CUDA0", bytes: 50 }] },
  totals: [
    { buffer: "CUDA0", bytes: 240 },
    { buffer: "CPU", bytes: 60 },
  ],
};

describe("ModelPlacementSection", () => {
  afterEach(cleanup);

  it("conta a camada dividida junto com as da GPU no resumo, e à parte na legenda", () => {
    mount(parcial, null);
    expect(screen.getByText("2 de 3 camadas na GPU")).toBeTruthy();
    expect(screen.getByText("1 inteiras na GPU")).toBeTruthy();
    expect(screen.getByText("1 divididas entre as duas")).toBeTruthy();
    expect(screen.getByText("1 na CPU")).toBeTruthy();
  });

  it("avisa que um override mandou parte das camadas para a CPU", () => {
    mount(parcial, null);
    expect(screen.getByText(/override de buffer type/)).toBeTruthy();
  });

  /**
   * O ponto da medida: sem ela a tela só pode repetir o que foi pedido, e tem
   * que dizer que é pedido. Com ela, diz onde o cache está.
   */
  it("mostra o cache KV medido em vez da flag quando a repartição chega", () => {
    mount(parcial, [{ buffer: "CUDA0", device: "CUDA0", model: 240, context: 1024, compute: 16 }]);
    expect(screen.getByText(/CUDA0 1,0 KB/)).toBeTruthy();
    expect(screen.queryByText("pedido na GPU, sem medida")).toBeNull();
  });

  it("diz que o cache KV foi apenas pedido quando não há repartição", () => {
    mount(parcial, null);
    expect(screen.getByText("pedido na GPU, sem medida")).toBeTruthy();
  });

  it("diz que não sabe, em vez de derivar, quando o nó não informa a colocação", () => {
    mount(null, null);
    expect(screen.getByText(/não informou onde as camadas ficaram/)).toBeTruthy();
  });
});
