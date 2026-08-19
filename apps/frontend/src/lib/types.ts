/**
 * Shapes the node actually serves.
 *
 * Authoritative sources, in the Rust crate: `src/webui.rs` (node, cluster,
 * catalog, jobs), `src/sysmon.rs` (system load), `src/runtime.rs` (runtime
 * status and node config), `src/transfer.rs` (transfers), `src/admin.rs`
 * (nodes and keys). Anything typed loosely here is loose in the API too.
 */

export interface MemStat {
  total_mb: number;
  used_mb: number;
  available_mb: number;
  used_pct: number;
}

export interface GpuStat {
  index: number;
  name: string;
  vram_total_mb: number | null;
  vram_used_mb: number | null;
  vram_used_pct: number | null;
  utilization_pct: number | null;
  temperature_c: number | null;
}

export interface GpuProcess {
  pid: number;
  name: string;
  used_mb: number;
}

export type GpuKind = "nvidia" | "unified" | "none";

export interface DiskStat {
  total_mb: number;
  free_mb: number;
  mount_point: string;
}

export interface SystemLoad {
  hostname: string;
  os: string;
  arch: string;
  cpu_count: number;
  load_avg: [number, number, number];
  ram: MemStat;
  gpu_kind: GpuKind;
  gpu_note: string | null;
  gpus: GpuStat[];
  gpu_processes: GpuProcess[];
  wired_mb: number | null;
  disk: DiskStat;
  sampled_at: string;
}

/**
 * The inference server's state on a node — `ServerState` in `src/runtime.rs`,
 * serialised lowercase. No `| string` escape hatch on purpose: the first
 * version of this type invented "stopped" and "running", and because anything
 * was assignable every screen compared against a value the node never sends.
 * A machine serving layers looked idle on every panel.
 */
export type ServerState = "disabled" | "starting" | "ready" | "failed";

export interface RuntimeStatus {
  state: ServerState;
  detail: string;
  pid: number | null;
  restarts: number;
  log_path: string | null;
  reported_total_layers: number | null;
  changed_at: string;
}

export interface NodeConfig {
  model_path: string;
  model_label: string;
  /** A implantação de que este nó faz parte; vazio se ele não veio de uma. */
  deploy_id: string;
  start_layer: number;
  end_layer: number;
  total_layers: number;
  peer_count: number;
  peer_index: number;
  primary: boolean;
  inference_port: number;
  n_gpu_layers: number;
  n_ctx: number;
  offload_kqv: boolean;
  fit_memory: boolean;
  n_batch: number;
  max_sessions: number;
  prefill_chunk_tokens: number;
  max_new_tokens: number;
  temperature: number;
  top_p: number;
  top_k: number;
  repeat_penalty: number;
  repeat_last_n: number;
  no_thinking: boolean;
  single_peer: boolean;
  no_embeddings: boolean;
  speculative_decoding_enabled: boolean;
  speculative_k: number;
  speculative_lookup_window: number;
  mtp_enabled: boolean;
  mtp_draft_tokens: number;
  mtp_min_probability: number;
}

export interface NodeInfo {
  node_id: string;
  short_id: string;
  hostname: string;
  role: string;
  connection_key: string;
  p2p_port: number;
  gateway_port: number;
  inference_port: number | null;
  config: NodeConfig | null;
  status: RuntimeStatus;
  system: SystemLoad;
  peer_count: number;
  load: number;
  active_model: string;
  /** True while the operator has taken this node off the network. */
  detached?: boolean;
  /** Commit de que o binário foi construído. */
  build_commit: string;
  /** Este nó aceita atualizar-se pela interface (POM_DEV_UPDATE=1). */
  dev_update: boolean;
  /** Endereço fixado pelo operador. `null` quando nada foi fixado. */
  advertise_addr: string | null;
  /** O que este nó anuncia agora de fato (fixado, observado, ou o sentinela). */
  effective_address: string;
  /** Se o cluster fala TLS entre peers agora. `webui.rs` - `NodeInfo`. */
  tls_required: boolean;
  /** Se **este** nó pode mudar isso: só o principal pode. */
  tls_authority: boolean;
}

export interface PeerView {
  node_id: string;
  short_id: string;
  addr: string;
  model: string;
  start_layer: number;
  end_layer: number;
  total_layers: number;
  capabilities: ModelCapability[];
  inference_url: string | null;
  /** Endereço escolhido para falar com este peer, e a latência medida. */
  route: { addr: string; rtt_ms: number } | null;
  gateway_url: string | null;
  load: number;
  is_self: boolean;
  node: NodeInfo | null;
  unreachable: string | null;
}

export interface ModelCapability {
  model_id: string;
  start_layer: number;
  end_layer: number;
  total_layers: number | null;
  supports_inference: boolean;
}

export interface ModelCoverage {
  model_id: string;
  total_layers: number;
  covered_layers: number;
  coverage: boolean[];
  complete: boolean;
}

/** A handoff selected by the backend routing planner for the active model. */
export interface RoutingEdge {
  from_node_id: string;
  to_node_id: string;
  model_id: string;
  from_end_layer: number;
  to_start_layer: number;
}

export interface ClusterView {
  self_id: string;
  active_model: string;
  total_layers: number;
  covered_layers: number;
  coverage: boolean[];
  complete: boolean;
  model_inventory: ModelCoverage[];
  peers: PeerView[];
  routing_edges: RoutingEdge[];
  candidate_edges: RoutingEdge[];
  node: NodeInfo | null;
}

export type ModelSource = "ollama" | "lmstudio" | "folder" | string;

export interface DiscoveredModel {
  id: string;
  name: string;
  source: ModelSource;
  source_label: string;
  path: string;
  size_bytes: number;
  architecture: string | null;
  total_layers: number | null;
  quant: string | null;
  shard_range: [number, number] | null;
  shard_group: string | null;
  /** sha256 da parte, quando o arquivo é endereçado por conteúdo. */
  sha256: string | null;
}

/** A whole `.gguf`, as opposed to a slice of one. Slices carry `shard_range`. */
export const isWholeModel = (m: DiscoveredModel): boolean => m.shard_range == null;

/** `src/models/deployment.rs` — de que forma um modelo está no ar. */
export type DeploymentKind = 'shards' | 'whole' | 'tensor_parallel';

/** `src/models/deployment.rs` — um nó da implantação, e a config com que subiu. */
export interface DeploymentNode {
  node_id: string;
  hostname: string;
  start_layer: number;
  end_layer: number;
  peer_index: number;
  peer_count: number;
  primary: boolean;
  part_sha256?: string | null;
  tp_rank?: number | null;
  config: RuntimeParams;
}

/**
 * `src/models/deployment.rs` — a ligação entre um conjunto de nós e um modelo.
 *
 * O mesmo modelo pode estar no ar em mais de uma implantação, com máquinas ou
 * configuração diferentes; o `deploy_id` é o que as distingue. Uma requisição
 * escolhe uma pelo sufixo `<modelo>@<deploy_id>`.
 */
export interface Deployment {
  deployment_version: number;
  deploy_id: string;
  kind: DeploymentKind;
  model: {
    label: string;
    total_layers: number;
    pom_id?: string | null;
  };
  created_at: string;
  created_by: string;
  activated_at: string;
  nodes: DeploymentNode[];
}

/** O que `GET /api/ui/deployments` responde. */
export interface DeploymentsResponse {
  node_id: string;
  deployments: Deployment[];
  /** Os `deploy_id` que atendem um pedido feito só com o nome do modelo. */
  active: string[];
}

export interface ShardSet {
  group: string;
  total_layers: number | null;
  total_layers_inferred: boolean;
  shards: DiscoveredModel[];
  complete: boolean;
}

export interface ScannedRoot {
  source: ModelSource;
  path: string;
  exists: boolean;
  found: number;
}

export interface CatalogResponse {
  hostname: string;
  node_id: string;
  models: DiscoveredModel[];
  shard_sets: ShardSet[];
  scanned: ScannedRoot[];
}

export interface PeerCatalog {
  node_id: string;
  hostname: string | null;
  is_self: boolean;
  models: DiscoveredModel[];
  shard_sets: ShardSet[];
  scanned?: ScannedRoot[];
  error?: string;
}

export interface PathEntry {
  path: string;
  exists: boolean;
  kind: "ollama" | "lmstudio" | "folder";
  used_bytes: number;
  max_bytes: number | null;
  usage_percent: number | null;
}

export interface PathsResponse {
  model_dirs: PathEntry[];
  shard_dir: PathEntry;
  using_defaults: boolean;
  detected: PathEntry[];
  default_shard_dir: string;
  shard_max_gb: number;
}

/** A slicing or apply run. `src/webui.rs` — `Job`. */
export interface Job {
  id: string;
  state: "running" | "done" | "failed";
  step: string;
  message: string;
  progress: number | null;
  outputs: string[];
  started_at: string;
  finished_at: string | null;
  /** "slice" | "apply" — added by `GET /api/ui/jobs`. */
  kind?: string;
  /** Human label for the notification list. */
  title?: string;
}

export interface TransferJob {
  id: string;
  name: string;
  /** De que lado desta máquina a transferência está. O id é o mesmo nas duas. */
  direction: "sending" | "receiving";
  /** A outra máquina. */
  peer_host: string;
  /** Arquivo local envolvido. */
  path: string;
  state: "running" | "verifying" | "done" | "failed" | "cancelled";
  bytes_done: number;
  bytes_total: number;
  bytes_per_sec: number;
  eta_secs: number | null;
  detail: string;
}

export interface TransferOffer {
  id: string;
  from_node_id: string;
  from_host: string;
  from_gateway: string;
  remote_path: string;
  grant: string | null;
  name: string;
  size_bytes: number;
  /**
   * Sempre vazio. Quem confere o arquivo é este nó, depois de aceitar, com o
   * digest que ele pede em `/api/transfer/manifest`; calculá-lo para oferecer
   * segurava a notificação pelo tempo de ler o arquivo inteiro.
   */
  md5: string;
  start_layer: number | null;
  end_layer: number | null;
}

export interface AdminNode {
  node_id: string;
  short_id: string;
  hostname: string;
  addr: string;
  role?: string;
  model?: string;
  start_layer?: number;
  end_layer?: number;
  total_layers?: number;
  is_self: boolean;
  connected: boolean;
  blocked: boolean;
  verified: boolean;
}

export interface ApiKey {
  key: string;
  label: string;
  node_id: string;
  active: boolean;
  created_at: string;
}

/** One inference request, as the node recorded it. `src/reqlog.rs`. */
export interface RequestLogEntry {
  id: string;
  at: string;
  route: string;
  model: string;
  /** Label of the API token that paid for it, or "chave de conexão". */
  token_label: string;
  /** Last 6 characters of the token, all that is ever shown. */
  token_hint: string;
  /** Node the token is assigned to, when it is assigned to one. */
  token_node_id: string | null;
  /** Node that received the request. */
  entry_node_id: string;
  /** Ordered node ids the generation actually went through. */
  hops: string[];
  source: string;
  prompt_tokens: number | null;
  completion_tokens: number | null;
  duration_ms: number | null;
  status: "ok" | "error" | string;
  error: string | null;
}

/** One slice of a size-driven plan. `src/plan.rs` — `SizedRange`. */
export interface SizedRange {
  index: number;
  start_layer: number;
  end_layer: number;
  layers: number;
  bytes: number;
  target_mb: number | null;
  is_rest: boolean;
  is_last: boolean;
  too_small: boolean;
  empty: boolean;
}

/** `src/plan.rs` — `SizePlan`, what `POST /api/ui/slice/plan` answers. */
export interface SizePlan {
  total_layers: number;
  model_bytes: number;
  ranges: SizedRange[];
  covered_layers: number;
  complete: boolean;
  missing_range: [number, number] | null;
  missing_bytes: number;
  blocked: boolean;
  warnings: string[];
}

/** One peer in a memory-budget plan. `src/plan.rs` — `PlannedShard`. */
export interface PlannedShard {
  node_id: string;
  label: string;
  budget_mb: number;
  usable_mb: number;
  start_layer: number;
  end_layer: number;
  layers: number;
  weight_mb: number;
  is_first: boolean;
  is_last: boolean;
  has_model: boolean;
}

/** `src/plan.rs` — `SplitPlan`, what `POST /api/ui/plan` answers. */
export interface SplitPlan {
  model_path: string;
  base_name: string;
  total_layers: number;
  model_weight_mb: number;
  reserve_pct: number;
  shards: PlannedShard[];
  covered_layers: number;
  complete: boolean;
  missing_range: [number, number] | null;
  missing_mb: number;
  warnings: string[];
}

/** One machine offered as the target of one slice. `src/deploy.rs`. */
export interface DeployAlternative {
  node_id: string;
  label: string;
  /** Already has the file — nothing to copy. */
  has: boolean;
  /** Nothing about this machine stops the assignment. */
  ok: boolean;
  note: string | null;
}

/** One slice feeding a merged step: the range, its file, where it comes from. */
export interface DeployStepPart {
  start_layer: number;
  end_layer: number;
  name: string;
  size_bytes: number;
  /** Already on the target — nothing to copy for this part. */
  ready: boolean;
  source_node_id: string | null;
  source_host: string | null;
  source_path: string | null;
}

/** One slice of a deploy plan: who serves it, and what has to move first. */
export interface DeployStep {
  /** 1-based place in the pipeline. Position 1 receives the prompt. */
  position: number;
  start_layer: number;
  end_layer: number;
  /** The plan's answer for the model size, `null` when nobody could say. */
  total_layers: number | null;
  name: string;
  size_bytes: number;
  target_node_id: string;
  target_host: string;
  /** Already on the target — nothing to copy. */
  ready: boolean;
  source_node_id: string | null;
  source_host: string | null;
  source_path: string | null;
  target_path: string | null;
  /** Entry point of the pipeline — whoever holds layer 0. */
  primary: boolean;
  /** Per-machine GPU offload, when set. `null` means "the cluster's number". */
  n_gpu_layers: number | null;
  /** Per-machine KV cache placement. `null` means "the cluster's answer". */
  offload_kqv: boolean | null;
  options: DeployAlternative[];
  /** Set when this assignment cannot be carried out, and why. */
  problem: string | null;
  /** Set when it can, but the operator should read it first. */
  note: string | null;
  /** True when this step serves the union of `parts`, joined into one file on
   *  the target, instead of a single slice already in place. */
  merged: boolean;
  /** The slices this step is assembled from, in layer order. Empty for a
   *  plain step. */
  parts: DeployStepPart[];
}

/** A range that exists somewhere in the cluster, selected or not. */
export interface DeployCandidate {
  start_layer: number;
  end_layer: number;
  /** What this slice itself declares. `null` for slices cut before
   *  `split_model.py` started writing `pom.original_block_count`. */
  total_layers: number | null;
  name: string;
  size_bytes: number;
  /** Node ids that already have this exact range. */
  holders: string[];
  selected: boolean;
}

export interface DeployMachine {
  node_id: string;
  label: string;
  mem_free_mb: number;
  disk_free_mb: number;
  holds: Array<[number, number]>;
  taking: [number, number] | null;
}

/**
 * Execution settings a launch carries. `src/webui.rs` — `RuntimeParams`.
 *
 * Cluster-wide on purpose: the context window and the batch decide the prompt
 * budget the gateway enforces, and the sampling only runs on the last peer.
 * `n_gpu_layers` is the default a peer takes when it has no override of its own.
 */
export interface RuntimeParams {
  n_gpu_layers: number;
  n_ctx: number;
  offload_kqv: boolean;
  fit_memory: boolean;
  n_batch: number;
  max_new_tokens: number;
  max_sessions: number;
  prefill_chunk_tokens: number;
  temperature: number;
  top_p: number;
  top_k: number;
  repeat_penalty: number;
  repeat_last_n: number;
  no_thinking: boolean;
  no_embeddings: boolean;
  /** Kill-switch do operador: mesmo com todos os peers capazes, a
   *  especulação só liga aqui. */
  speculative_decoding_enabled: boolean;
  /** Largura do rascunho: o token confirmado mais até `k - 1` palpites. */
  speculative_k: number;
  /** Maior n-grama que o rascunho por prompt-lookup procura repetir. */
  speculative_lookup_window: number;
  /** Predição multi-token pelo bloco `nextn` do próprio modelo.
   *  Interruptor **separado** do de cima: são mecanismos diferentes, e só
   *  este usa peso treinado. Só entra em ação quando uma máquina serve o
   *  modelo inteiro — ver `crates/node/src/inference/mtp.rs`. */
  mtp_enabled: boolean;
  /** Quantos tokens a cabeça `nextn` rascunha por rodada. */
  mtp_draft_tokens: number;
  /** Confiança mínima para o rascunho continuar; 0 aceita qualquer palpite. */
  mtp_min_probability: number;
}

/** What the operator picked, as `POST /api/ui/deploy{,/plan}` accepts it. */
export interface DeploySelection {
  start_layer: number;
  end_layer: number;
  target_node_id: string;
  /** Overrides `params.n_gpu_layers` for this machine alone. */
  n_gpu_layers?: number | null;
  /**
   * Where this machine's KV cache lives, overriding `params.offload_kqv`.
   *
   * Per machine because the cache is sized by the context window and the
   * machines are not alike: 16 GB of VRAM and a CPU-only box in one pipeline
   * cannot both be right about where it fits.
   */
  offload_kqv?: boolean | null;
}

/**
 * What is being put into service. `src/backend/deploy.rs` — `DeployKind`.
 *
 * Not two flows: a whole model is read by the node as a single range covering
 * every layer, so the plan, the copies and the launch are the same ones the
 * slice path uses. What the screen does differently is stop asking questions
 * that only have one answer — the order of a pipeline of one, and which layers
 * of all of them to include.
 */
export type DeployKind = "shards" | "whole" | "tensor_parallel";

/**
 * Tensor parallelism group (caminho C), from src/backend/deploy.rs TpLaunch.
 * Present only with kind "tensor_parallel": the primary serves the whole
 * model and each worker extracts its own tensor shard.
 */
export interface TpLaunch {
  /** Ranks in the group (1 primary + size-1 workers). */
  size: number;
  /** node_id of the primary (rank 0); the node derives the reduce URL. */
  primary_node_id: string;
  /** node_ids of the workers, in rank order (rank 1..size-1). */
  worker_node_ids: string[];
}

export interface DeployPlan {
  group: string;
  /**
   * Echoed by the node, so the screen never guesses which question it asked.
   *
   * Optional because a peer running an older binary does not send it, and that
   * peer only ever answers about shards — read it as `"shards"` when absent,
   * the same default the node applies to a request without a `kind`.
   */
  kind?: DeployKind;
  /** `null` means no slice declares it and none starts at layer 0, which is a
   *  `blocked` plan rather than a model of zero layers. */
  total_layers: number | null;
  /** True when the total was read off the slices' coverage instead of their
   *  metadata. The deploy still runs; the screen has to say so. */
  total_layers_inferred: boolean;
  steps: DeployStep[];
  complete: boolean;
  transfer_bytes: number;
  /** Set when the plan cannot be carried out at all, and why. */
  blocked: string | null;
  /** True while this is the node's suggestion rather than the operator's edit. */
  auto: boolean;
  /** True when the operator asked each machine's adjacent slices to be joined
   *  into one file — the steps reflect it. */
  merge: boolean;
  available: DeployCandidate[];
  machines: DeployMachine[];
  missing: Array<[number, number]>;
  overlap: Array<[number, number]>;
  warnings: string[];
  /** The settings this deploy would launch with, defaults filled in. */
  params: RuntimeParams;
  /** Prompt tokens one request gets under `params`. `gateway::prompt_budget`. */
  prompt_tokens: number;
  /** Prompt tokens per round trip through the pipeline, under `params`. */
  chunk_tokens: number;
}
