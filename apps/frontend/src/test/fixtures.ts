/**
 * Peers de mentira com a forma inteira que o nó serve.
 *
 * Um objeto parcial passa pelo `?.` de um componente e explode no seguinte —
 * `node` sem `status`, por exemplo, derruba o canvas na chave de layout. Aqui
 * o padrão é completo e o teste sobrescreve só o campo de que fala.
 *
 * Só testes importam este arquivo; nada do app depende dele.
 */
import type { ClusterView, NodeInfo, PeerView, SystemLoad } from "../lib/types";

export function fakeSystem(over: Partial<SystemLoad> = {}): SystemLoad {
  return {
    hostname: "salem",
    os: "macos",
    arch: "aarch64",
    cpu_count: 10,
    load_avg: [1.2, 1.1, 1.0],
    ram: { total_mb: 16384, used_mb: 8192, available_mb: 8192, used_pct: 50 },
    gpu_kind: "unified",
    gpu_note: null,
    gpus: [],
    gpu_processes: [],
    wired_mb: 2048,
    disk: { total_mb: 500000, free_mb: 200000, mount_point: "/" },
    sampled_at: "2026-08-15T00:00:00Z",
    ...over,
  };
}

export function fakeNode(over: Partial<NodeInfo> = {}): NodeInfo {
  return {
    node_id: "a",
    short_id: "a",
    hostname: "salem",
    role: "peer",
    connection_key: "",
    p2p_port: 42777,
    gateway_port: 8080,
    inference_port: 8181,
    config: null,
    status: {
      state: "ready",
      detail: "servindo",
      pid: 42,
      restarts: 0,
      log_path: null,
      reported_total_layers: 32,
      changed_at: "2026-08-15T00:00:00Z",
    },
    system: fakeSystem({ hostname: over.hostname ?? "salem" }),
    peer_count: 1,
    load: 0,
    active_model: "qwen35",
    build_commit: "abc1234",
    dev_update: false,
    advertise_addr: null,
    effective_address: "10.0.0.1:42777",
    ...over,
  };
}

/** Um peer sem fatia: `model` vazio e zero camadas, como o nó anuncia. */
export function fakePeer(over: Partial<PeerView> & Pick<PeerView, "node_id">): PeerView {
  return {
    short_id: over.node_id,
    addr: "10.0.0.1:42777",
    model: "",
    start_layer: 0,
    end_layer: 0,
    total_layers: 0,
    capabilities: [],
    inference_url: null,
    route: null,
    gateway_url: null,
    load: 0,
    is_self: false,
    node: null,
    unreachable: null,
    ...over,
  };
}

/** Um peer servindo uma faixa do modelo, com as capabilities coerentes. */
export function fakeSlicePeer(
  node_id: string,
  start_layer: number,
  end_layer: number,
  over: Partial<PeerView> = {},
): PeerView {
  return fakePeer({
    node_id,
    model: "qwen35",
    start_layer,
    end_layer,
    total_layers: 32,
    inference_url: "http://10.0.0.1:8181",
    capabilities: [
      { model_id: "qwen35", start_layer, end_layer, total_layers: 32, supports_inference: true },
    ],
    node: fakeNode({ node_id, short_id: node_id, hostname: node_id }),
    ...over,
  });
}

export function fakeCluster(peers: PeerView[], active_model = "qwen35"): ClusterView {
  return {
    self_id: peers[0]?.node_id ?? "",
    active_model,
    total_layers: 32,
    covered_layers: 32,
    coverage: [],
    complete: true,
    model_inventory: [],
    routing_edges: [],
    candidate_edges: [],
    node: null,
    peers,
  };
}
