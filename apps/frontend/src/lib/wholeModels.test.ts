import { describe, expect, it } from "vitest";
import { tensorParallelModelsOf, wholeModelsOf } from "./wholeModels";
import type { DiscoveredModel, PeerCatalog } from "./types";

function model(over: Partial<DiscoveredModel>): DiscoveredModel {
  return {
    id: "1",
    name: "qwen3.5-9b",
    source: "lmstudio",
    source_label: "LM Studio",
    path: "/lm/qwen3.5-9b.gguf",
    size_bytes: 5_000_000_000,
    architecture: "qwen3",
    total_layers: 32,
    quant: "Q4_K_M",
    shard_range: null,
    shard_group: null,
    ...over,
  };
}

function peer(node_id: string, models: DiscoveredModel[]): PeerCatalog {
  return { node_id, hostname: node_id, is_self: false, models, shard_sets: [] };
}

function tpPart(rank: number, size = 2): DiscoveredModel {
  return model({
    id: `tp-${rank}`,
    name: `qwen · L0-31 · rank ${rank}`,
    path: `/shards/tp-${rank}.gguf`,
    shard_range: [0, 31],
    shard_group: "qwen-hash",
    tp_rank: rank,
    tp_size: size,
  });
}

describe("wholeModelsOf", () => {
  it("names each model once and gathers the machines that have it", () => {
    const out = wholeModelsOf([
      peer("ksia", [model({})]),
      peer("mac", [model({ id: "2", path: "/outro/qwen3.5-9b.gguf" })]),
    ]);
    expect(out).toHaveLength(1);
    expect(out[0].name).toBe("qwen3.5-9b");
    expect(out[0].holders).toEqual(["ksia", "mac"]);
  });

  it("takes the repository a holder downloaded it from, with the other parts", () => {
    const parts = [
      { index: 1, path: "/m/q-00001-of-00002.gguf", bytes: 1, tensor_count: 1 },
      { index: 2, path: "/m/q-00002-of-00002.gguf", bytes: 1, tensor_count: 1 },
    ];
    const handPlaced = peer("mac", [model({ path: "/lm/qwen3.5-9b.gguf" })]);
    const downloaded = {
      ...peer("ksia", [model({ id: "2", path: "/m/q-00001-of-00002.gguf", multipart_parts: parts })]),
      hf_sources: [{ path: "/m/q-00001-of-00002.gguf", repo_id: "org/q", revision: null, filename: "q-00001-of-00002.gguf" }],
    };
    const [entry] = wholeModelsOf([handPlaced, downloaded]);
    expect(entry.source).toEqual({
      repo_id: "org/q",
      revision: null,
      filename: "q-00001-of-00002.gguf",
      extra_files: ["q-00002-of-00002.gguf"],
    });
    expect(wholeModelsOf([handPlaced])[0].source).toBeNull();
  });

  /** A slice is not a whole model: both lists come out of the same scan, and
   *  `shard_range` is what separates them - the same criterion as the node. */
  it("ignores slices", () => {
    const out = wholeModelsOf([
      peer("ksia", [
        model({ id: "s", name: "qwen_0_15.gguf", shard_range: [0, 15], shard_group: "qwen" }),
      ]),
    ]);
    expect(out).toEqual([]);
  });

  /**
   * A GGUF whose header did not open stays in the list.
   *
   * It shows on the Models screen; vanishing from here would read as the cluster
   * having lost the file. Choosing it yields a refusal from the node that says
   * what is wrong with the file, which is the useful information.
   */
  it("keeps a model without a layer count, for the node to explain", () => {
    const out = wholeModelsOf([peer("ksia", [model({ total_layers: null })])]);
    expect(out).toHaveLength(1);
    expect(out[0].total_layers).toBeNull();
  });

  /** One machine reading the header and another not is a fact about a file, not
   *  about the model: the answer that exists beats the absent one. */
  it("fills in the data one of the copies did not bring", () => {
    const out = wholeModelsOf([
      peer("ksia", [model({ total_layers: null, quant: null, architecture: null })]),
      peer("mac", [model({ id: "2" })]),
    ]);
    expect(out[0].total_layers).toBe(32);
    expect(out[0].quant).toBe("Q4_K_M");
    expect(out[0].architecture).toBe("qwen3");
  });

  it("sorts by name and returns empty without peers", () => {
    const out = wholeModelsOf([
      peer("ksia", [model({ name: "zephyr" }), model({ id: "2", name: "alpaca" })]),
    ]);
    expect(out.map((w) => w.name)).toEqual(["alpaca", "zephyr"]);
    expect(wholeModelsOf(undefined)).toEqual([]);
  });

  it("keeps TP ranks separate from the whole model and groups by width", () => {
    const out = tensorParallelModelsOf([
      {
        ...peer("ksia", []),
        shard_sets: [{
          group: "qwen-hash",
          model_label: "qwen",
          pom_id: "hash",
          total_layers: 32,
          total_layers_inferred: false,
          shards: [],
          tp_parts: [tpPart(1), tpPart(0)],
          complete: true,
        }],
      },
    ]);
    expect(out).toHaveLength(1);
    expect(out[0].key).toBe("qwen-hash:2");
    expect(out[0].ranks.map((rank) => rank.rank)).toEqual([0, 1]);
    expect(out[0].ranks.every((rank) => rank.holders.includes("ksia"))).toBe(true);
  });
});
