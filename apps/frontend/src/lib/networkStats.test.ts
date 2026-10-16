import { describe, expect, it } from "vitest";
import { fakeCluster, fakeNode, fakeSlicePeer, fakeSystem } from "../test/fixtures";
import { networkStats } from "./networkStats";

describe("networkStats", () => {
  it("aggregates nodes, RTT, VRAM and models published in the cluster", () => {
    const first = fakeSlicePeer("a", 0, 15, {
      route: { addr: "10.0.0.1:42777", rtt_ms: 8 },
      node: fakeNode({
        node_id: "a",
        system: fakeSystem({
          gpus: [{ index: 0, name: "GPU A", vendor: "nvidia", vram_total_mb: 8192, vram_used_mb: 0, vram_used_pct: 0, utilization_pct: 0, power_watts: null, temperature_c: 50 }],
        }),
      }),
    });
    const second = fakeSlicePeer("b", 16, 31, {
      model: "other-model",
      route: { addr: "10.0.0.2:42777", rtt_ms: 12 },
      node: fakeNode({
        node_id: "b",
        system: fakeSystem({
          gpus: [{ index: 0, name: "GPU B", vendor: "nvidia", vram_total_mb: 4096, vram_used_mb: 0, vram_used_pct: 0, utilization_pct: 0, power_watts: null, temperature_c: 50 }],
        }),
      }),
    });

    expect(networkStats({ ...fakeCluster([first, second]), model_inventory: [{ model_id: "inventory-model", total_layers: 1, covered_layers: 1, coverage: [true], complete: true, unreadable_nodes: [] }] })).toEqual({
      nodeCount: 2,
      averageLatencyMs: 10,
      vramTotalMb: 12288,
      models: ["inventory-model", "other-model", "qwen35"],
    });
  });

  it("preserves unavailability when there are no computable readings", () => {
    const peer = fakeSlicePeer("a", 0, 15, { route: null, node: fakeNode({ system: fakeSystem({ gpus: [] }) }) });
    expect(networkStats(fakeCluster([peer], "qwen35"))).toMatchObject({
      nodeCount: 1,
      averageLatencyMs: null,
      vramTotalMb: null,
      models: ["qwen35"],
    });
  });
});
