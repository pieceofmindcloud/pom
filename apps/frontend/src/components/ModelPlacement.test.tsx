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
  it("calls GPU the layer with no byte in a host buffer", () => {
    expect(placementOf([{ buffer: "CUDA0", bytes: 100 }])).toBe("gpu");
  });

  it("calls CPU the layer that mmap left on the host", () => {
    expect(placementOf([{ buffer: "CPU_Mapped", bytes: 100 }])).toBe("cpu");
  });

  it("calls split the layer with experts on the CPU and the rest on the GPU", () => {
    expect(
      placementOf([
        { buffer: "CUDA0", bytes: 90 },
        { buffer: "CPU", bytes: 10 },
      ]),
    ).toBe("mixed");
  });

  it("invents no placement for a group without bytes", () => {
    expect(placementOf([])).toBe("unknown");
  });
});

describe("isCpuBuffer", () => {
  it("recognizes the host buffer with and without a suffix", () => {
    expect(isCpuBuffer("CPU")).toBe(true);
    expect(isCpuBuffer("CPU_Mapped")).toBe(true);
  });

  it("does not mistake a backend whose name merely starts alike", () => {
    expect(isCpuBuffer("CUDA0")).toBe(false);
    expect(isCpuBuffer("Metal")).toBe(false);
  });
});

describe("kvCacheBuffers", () => {
  it("leaves out the buffer that holds no context at all", () => {
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

const partial: ModelPlacement = {
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

  it("counts the split layer with the GPU ones in the summary, and apart in the legend", () => {
    mount(partial, null);
    expect(screen.getByText("2 de 3 camadas na GPU")).toBeTruthy();
    expect(screen.getByText("1 inteiras na GPU")).toBeTruthy();
    expect(screen.getByText("1 divididas entre as duas")).toBeTruthy();
    expect(screen.getByText("1 na CPU")).toBeTruthy();
  });

  it("warns that an override sent part of the layers to the CPU", () => {
    mount(partial, null);
    expect(screen.getByText(/override de buffer type/)).toBeTruthy();
  });

  /**
   * O ponto da medida: sem ela a tela só pode repetir o que foi pedido, e tem
   * que dizer que é pedido. Com ela, diz onde o cache está.
   */
  it("shows the measured KV cache instead of the flag when the breakdown arrives", () => {
    mount(partial, [{ buffer: "CUDA0", device: "CUDA0", model: 240, context: 1024, compute: 16 }]);
    expect(screen.getByText(/CUDA0 1,0 KB/)).toBeTruthy();
    expect(screen.queryByText("pedido na GPU, sem medida")).toBeNull();
  });

  it("says the KV cache was only requested when there is no breakdown", () => {
    mount(partial, null);
    expect(screen.getByText("pedido na GPU, sem medida")).toBeTruthy();
  });

  it("says it does not know, instead of deriving, when the node does not report the placement", () => {
    mount(null, null);
    expect(screen.getByText(/não informou onde as camadas ficaram/)).toBeTruthy();
  });
});
