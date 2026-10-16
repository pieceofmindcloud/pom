import { describe, expect, it } from "vitest";

import { layerPlacement } from "./layerPlacement";

describe("layerPlacement", () => {
  it("separates the GPU layers from the ones left on the CPU", () => {
    // The captain's case: 64 layers served, 55 fit on the GPU.
    expect(layerPlacement(0, 63, 55, true)).toEqual({ served: 64, gpu: 55, cpu: 9 });
  });

  it("treats negative as 'all on the GPU', not as none", () => {
    // -1 is llama.cpp's convention for "offload everything". Read as zero,
    // the card would say the whole machine runs on the CPU.
    expect(layerPlacement(0, 63, -1, true)).toEqual({ served: 64, gpu: 64, cpu: 0 });
  });

  it("does not let the request exceed what the machine serves", () => {
    // 99 is the "everything" sentinel; the slice has 16 layers, not 99.
    expect(layerPlacement(0, 15, 99, true)).toEqual({ served: 16, gpu: 16, cpu: 0 });
  });

  it("counts the range from the starting layer, not from zero", () => {
    expect(layerPlacement(15, 31, 10, true)).toEqual({ served: 17, gpu: 10, cpu: 7 });
  });

  it("stays quiet when the engine reported nothing", () => {
    // Without a report there is no guess: saying "everything on the GPU" without
    // knowing is the defect.
    expect(layerPlacement(0, 63, null, true)).toBeNull();
    expect(layerPlacement(null, null, 55, true)).toBeNull();
  });

  it("stays quiet when the range makes no sense", () => {
    expect(layerPlacement(10, 4, 55, true)).toBeNull();
  });

  it("stays quiet on a machine without GPU, even with a positive layer report", () => {
    // The captain's case: Windows with no board at all showing "25 on the GPU" -
    // the reported number was the request's sentinel, not a real reading.
    expect(layerPlacement(0, 63, 25, false)).toBeNull();
  });
});
