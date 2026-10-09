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

  /** Fatia não é modelo inteiro: as duas listas saem da mesma varredura, e
   *  `shard_range` é o que as separa - o mesmo critério do nó. */
  it("ignores slices", () => {
    const out = wholeModelsOf([
      peer("ksia", [
        model({ id: "s", name: "qwen_0_15.gguf", shard_range: [0, 15], shard_group: "qwen" }),
      ]),
    ]);
    expect(out).toEqual([]);
  });

  /**
   * Um GGUF cujo cabeçalho não abriu continua na lista.
   *
   * Ele aparece na tela de Modelos; sumir daqui se leria como o cluster ter
   * perdido o arquivo. Escolhê-lo rende uma recusa do nó que diz o que está
   * errado com o arquivo, que é a informação útil.
   */
  it("keeps a model without a layer count, for the node to explain", () => {
    const out = wholeModelsOf([peer("ksia", [model({ total_layers: null })])]);
    expect(out).toHaveLength(1);
    expect(out[0].total_layers).toBeNull();
  });

  /** Uma máquina lendo o cabeçalho e outra não é fato sobre um arquivo, não
   *  sobre o modelo: a resposta que existe vence a ausente. */
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
