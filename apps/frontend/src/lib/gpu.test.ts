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

  /** Unified memory has no VRAM: an invented total would become a full bar. */
  it("returns null when no card reports VRAM", () => {
    expect(sumVram([gpu({ vendor: "apple", vram_total_mb: null, vram_used_mb: null })])).toBeNull();
    expect(sumVram([])).toBeNull();
  });

  /** Only the ones that know enter the sum; one that does not know does not become zero used. */
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

import { describe as describeServed, expect as expectServed, it as itServed } from "vitest";
import { servedSystem } from "./gpu";
import type { NodeConfig, SystemLoad } from "./types";

describeServed("servedSystem", () => {
  const board = (index: number) => ({ index, name: `GPU ${index}`, vendor: "nvidia", vram_total_mb: 16000, vram_used_mb: 1000, vram_used_pct: 6, utilization_pct: 0, power_watts: null, temperature_c: 40 });
  const system = { gpus: [board(0), board(1)] } as unknown as SystemLoad;
  itServed("keeps only the deployment's GPUs", () => {
    const config = { gpus: [1] } as unknown as NodeConfig;
    expectServed(servedSystem(system, config)?.gpus.map((g) => g.index)).toEqual([1]);
  });
  itServed("shows the whole machine without a choice", () => {
    expectServed(servedSystem(system, null)?.gpus).toHaveLength(2);
    expectServed(servedSystem(system, { gpus: null } as unknown as NodeConfig)?.gpus).toHaveLength(2);
  });
});
