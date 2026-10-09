import { describe, expect, it } from "vitest";
import { sumVram, vramUsedPct } from "./gpu";
import type { GpuStat } from "./types";

function gpu(over: Partial<GpuStat>): GpuStat {
  return {
    index: 0,
    name: "GPU",
    vendor: "nvidia",
    vram_total_mb: 24576,
    vram_used_mb: 4096,
    vram_used_pct: 16.7,
    utilization_pct: 10,
    power_watts: null,
    temperature_c: 50,
    ...over,
  };
}

describe("sumVram", () => {
  it("sums both cards, used and total", () => {
    expect(sumVram([gpu({ index: 0 }), gpu({ index: 1, vram_used_mb: 8192 })])).toEqual({
      used: 12288,
      total: 49152,
    });
  });

  /** Memória unificada não tem VRAM: um total inventado viraria barra cheia. */
  it("returns null when no card reports VRAM", () => {
    expect(sumVram([gpu({ vendor: "apple", vram_total_mb: null, vram_used_mb: null })])).toBeNull();
    expect(sumVram([])).toBeNull();
  });

  /** Só as que sabem entram na conta; a que não sabe não vira zero usado. */
  it("ignores the card without a reading instead of counting it as empty", () => {
    expect(
      sumVram([gpu({ index: 0 }), gpu({ index: 1, vram_total_mb: null, vram_used_mb: null })]),
    ).toEqual({ used: 4096, total: 24576 });
  });

  it("converts the sum to a percentage, and does not measure what does not exist", () => {
    expect(vramUsedPct([gpu({ vram_total_mb: 1000, vram_used_mb: 250 })])).toBe(25);
    expect(vramUsedPct([gpu({ vram_total_mb: null, vram_used_mb: null })])).toBeNull();
  });
});
