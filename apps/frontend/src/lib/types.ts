/**
 * Shapes the node actually serves.
 *
 * Authoritative sources, in the Rust crate: `src/backend/webui/` (node, cluster,
 * catalog, jobs), `src/utils/sysmon.rs` (system load), `src/inference/runtime.rs` (runtime
 * status and node config), `src/backend/transfer.rs` (transfers), `src/backend/admin.rs`
 * (nodes and keys). Anything typed loosely here is loose in the API too.
 */
import type { Profile } from "./profile";

export interface MemStat {
  total_mb: number;
  used_mb: number;
  available_mb: number;
  used_pct: number;
}

/** Whose card it is. `GpuKind` describes the machine; this, one part. */
export type GpuVendor = "nvidia" | "amd" | "apple";

export interface GpuStat {
  index: number;
  name: string;
  vendor: GpuVendor;
  vram_total_mb: number | null;
  vram_used_mb: number | null;
  vram_used_pct: number | null;
  utilization_pct: number | null;
  power_watts: number | null;
  temperature_c: number | null;
}

export interface ThermalPolicy {
  limit_c: number;
  hysteresis_c: number;
  cooldown_secs: number;
}

export interface GpuProcess {
  pid: number;
  name: string;
  used_mb: number;
}

/** Summary of the machine. `mixed` = cards of different brands together. */
export type GpuKind = "nvidia" | "amd" | "mixed" | "unified" | "none";

export interface DiskStat {
  total_mb: number;
  free_mb: number;
  mount_point: string;
  /** Device behind the mount (`/dev/nvme0n1p2`); empty when it is not known. */
  device: string;
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
  /** The volume where the slices live - the one that answers "does another fit?". */
  disk: DiskStat;
  /**
   * Every volume mounted on the machine, one per partition.
   *
   * An empty list is "I did not ask", not "there is no disk": an old node does
   * not publish this field, and the screen falls back to `disk`.
   */
  disks: DiskStat[];
  sampled_at: string;
}

/**
 * The inference server's state on a node — `ServerState` in `src/inference/runtime.rs`,
 * serialised lowercase. No `| string` escape hatch on purpose: the first
 * version of this type invented "stopped" and "running", and because anything
 * was assignable every screen compared against a value the node never sends.
 * A machine serving layers looked idle on every panel.
 */
export type ServerState = "disabled" | "starting" | "ready" | "idle" | "failed" | "thermal";

/** Engine the backend states it ran, with no inference by the interface. */
export type ExecutionEngine = "llama.cpp" | "moe" | "plugin" | "unknown";

export interface RuntimeStatus {
  state: ServerState;
  detail: string;
  pid: number | null;
  restarts: number;
  log_path: string | null;
  reported_total_layers: number | null;
  /** Layers llama.cpp actually put on the GPU, reported by the engine after
   *  loading. Not to be confused with `NodeConfig.n_gpu_layers`, which is the request.
   *  Negative is "all", in llama.cpp's convention. See `layerPlacement`. */
  reported_gpu_layers: number | null;
  changed_at: string;
}

export interface NodeConfig {
  model_path: string;
  /** Multimodal projector loaded on this node, when vision is on. */
  mmproj_path?: string | null;
  model_label: string;
  /** The deployment this node is part of; empty if it did not come from one. */
  deploy_id: string;
  /** Operational name of the deployment, when given by the operator. */
  deployment_name: string;
  start_layer: number;
  end_layer: number;
  total_layers: number;
  peer_count: number;
  peer_index: number;
  primary: boolean;
  /** Persisted engine preference carried by the active deployment. */
  inference_mode: InferenceMode;
  inference_port: number;
  n_gpu_layers: number;
  n_ctx: number;
  offload_kqv: boolean;
  fit_memory: boolean;
  fit_memory_margin_mb: number;
  n_batch: number;
  max_sessions: number;
  prefill_chunk_tokens: number;
  /** Maximum number of prefills overlapping per request. */
  prefill_max_inflight_chunks: number;
  max_new_tokens: number;
  temperature: number;
  top_p: number;
  top_k: number;
  repeat_penalty: number;
  repeat_last_n: number;
  /** Subtracted once from the logit of each token seen in the window; 0 is off. */
  presence_penalty?: number;
  /** Subtracted per occurrence of a token in the window; 0 is off. */
  frequency_penalty?: number;
  /** Drops candidates less likely than this fraction of the most likely one; 0 is off. */
  min_p?: number;
  /** Seed of the sampler's generator, as text: a 64-bit integer does not fit a JS number. */
  seed?: string;
  /** DRY sampler: penalty for extending a sequence already seen; 0 is off. */
  dry_multiplier?: number;
  dry_base?: number;
  dry_allowed_length?: number;
  dry_penalty_last_n?: number;
  /** Physical micro-batch under the memory fit; never more than `n_batch`. */
  n_ubatch?: number;
  /** Token limit of a thinking block; -1 (or absent) is no limit. */
  reasoning_budget?: number;
  no_thinking: boolean;
  single_peer: boolean;
  no_embeddings: boolean;
  speculative_decoding_enabled: boolean;
  speculative_k: number;
  speculative_lookup_window: number;
  mtp_enabled: boolean;
  mtp_draft_tokens: number;
  mtp_min_probability: number;
  /** Local peer unload policy; absent means the pre-feature default. */
  unload_if_idle?: boolean;
  idle_minutes?: number;
  /** Every routed MoE expert stays in RAM and the CPU computes it (llama.cpp `--cpu-moe`). */
  moe_experts_in_ram?: boolean;
  /** GPU cache, in MiB, for the experts kept in RAM; 0 turns it off. */
  moe_cache_mib?: number;
  /** The gateway picks each prefill chunk; off, it uses `prefill_chunk_tokens` as is. */
  adaptive_prefill_chunks?: boolean;
}

/** Boundary of the administrative surface, distinct from the pipeline routes. */
export type AdminExposure = "localhost" | "network";

/** Public metadata used to decide whether the turnstile should appear. */
export interface AdminAccessStatus {
  admin_exposure: AdminExposure;
  requires_password: boolean;
  /** The password in effect is the first access default (`admin`). */
  default_password?: boolean;
  /** Identifies this run of the node; changes on every restart. */
  boot_id?: string;
  gateway_port: number;
}

/** Administrative state queried after the page has already been authenticated. */
export interface AdminAccessInfo {
  admin_exposure: AdminExposure;
  gateway_port: number;
  /** Only presence is exposed; the Argon2id hash never crosses the API. */
  password_configured: boolean;
}

/**
 * Local state of an independent plugin (see `crates/node/src/plugins.rs`),
 * bought/activated/deactivated separately from the others. The node creates
 * runtimes dynamically for the `plugin_code`s granted by the server; the
 * Plugins screen crosses these local states with the public storefront, which
 * also holds plugins with no local status on this installation.
 */
export type PluginLicenseState =
  | "needs_activation"
  | "active"
  | "degraded"
  | "expired"
  | "revoked"
  | "unavailable";

/** An available version update - of the plugin itself, never of `pom_core`. */
export interface PluginUpdateInfo {
  release_id: string;
  version: string;
  changelog: string | null;
}

export interface PluginActivationCandidate {
  plugin_code: string;
  display_name: string;
  release_id: string;
  version: string;
}

export interface PluginActivationUsage {
  plugin_code: string;
  activation_count: number;
  max_activations: number;
}

export interface PluginBooleanPreference {
  key: string;
  label: string;
  default: boolean;
  description?: string | null;
}

export interface PluginPreferencesSchema {
  features: PluginBooleanPreference[];
  storage_directory?: { label: string; default?: string | null } | null;
}

export interface PluginPreferences {
  features: Record<string, boolean>;
  storage_directory?: string | null;
}

export interface GithubRepositoryManifest {
  schema: number;
  plugin_code: string;
  display_name: string;
  description: string | null;
  plugin_abi: number;
  feature_set: string[];
  artifacts: Record<string, string>;
  preferences: PluginPreferencesSchema | null;
}

export interface GithubRepositoryRelease {
  release_id: string;
  tag_name: string;
  name: string | null;
  changelog: string | null;
  published_at: string | null;
  prerelease: boolean;
  asset_name: string | null;
  asset_size: number | null;
  available_for_node: boolean;
}

export interface GithubRepository {
  id: string;
  repository_url: string;
  manifest_path?: string;
  manifest: GithubRepositoryManifest;
  ui_manifest?: Record<string, unknown> | null;
  releases: GithubRepositoryRelease[];
  /** Per-language description declared in the repository's manifest. */
  description?: Record<string, string>;
  /** Screenshots served straight from GitHub (`raw.githubusercontent.com`). */
  screenshot_urls?: string[];
  checked_at: string;
  check_error: string | null;
}

export interface DevelopmentPluginSource {
  repository: string;
  plugin_code: string;
  display_name: string;
  active: boolean;
  install_kind: "development" | "github" | "licensed" | null;
}

export interface DevelopmentPluginSources {
  mount_available: boolean;
  sources: DevelopmentPluginSource[];
}

export interface DevelopmentPluginRebuildResult {
  repository: string;
  plugin_code: string;
  build_ms: number;
  installed_at: string;
}

export interface DevelopmentPluginError {
  code: string;
  stage?: "inspect" | "metadata" | "prepare" | "compile" | "install";
  exit_code?: number | null;
}

export interface PluginStatus {
  code: string;
  /** True only when the plugin host is installed and ready. */
  active: boolean;
  /** The running artifact was built from a locally mounted development source. */
  development_install: boolean;
  state: PluginLicenseState;
  features: string[];
  manage_url: string;
  plugin_version: string | null;
  lease_expires_at: string | null;
  /** License validity (not the technical lease above), from GET /v1/licenses/status. */
  license_expires_at: string | null;
  /** Per-license activation usage; null means the server supplied no value. */
  activation_count: number | null;
  max_activations: number | null;
  /** Plugin codes this license grants. */
  licensed_plugins: string[];
  /** Per-plugin activation usage returned by the authenticated license server. */
  plugin_usage: PluginActivationUsage[];
  /** Other catalog products with a release eligible for this node. */
  activation_candidates: PluginActivationCandidate[];
  /** Last time the installed host reported ready for a release. */
  installed_at: string | null;
  /** Last successful manifest lookup, regardless of whether it found an update. */
  update_checked_at: string | null;
  available_update: PluginUpdateInfo | null;
  /** Actionable installation failure, independent of license validity. */
  installation_error?: string | null;
  /** Installed and running from a public GitHub source without a license lease. */
  public_install?: boolean;
  github_repository?: string | null;
  preferences_schema?: PluginPreferencesSchema | null;
  preferences?: PluginPreferences | null;
}

/**
 * Update of the `pom` binary itself. A product boundary deliberately
 * separate from the plugins (see `crates/node/src/pom_core_update.rs`): never
 * mix this type with `PluginStatus`/`PluginUpdateInfo` in the UI.
 */
export interface PomCoreUpdateStatus {
  core_update: PluginUpdateInfo | null;
}

export interface NodeInfo {
  node_id: string;
  short_id: string;
  hostname: string;
  profile: Profile;
  network_name: string;
  /** This node is the principal of the network it is in: its own network. */
  own_network?: boolean;
  /** This node's own network, kept aside while it is a member elsewhere. */
  standby_network?: string | null;
  /** This node's copy of the trust chain and whether a peer has a newer one. */
  cluster_version?: import("./clusterVersion").ClusterVersionStatus;
  /**
   * The cluster's connection key, which this interface shows under "Connection
   * Key" and keeps to authenticate the calls that require it.
   */
  connection_key: string;
  /**
   * This node's **effective** administrative authority, and not the profile: a
   * `profile: admin` without the signed grant comes as `null`. It is what decides
   * whether the screen offers an administrative act or only shows it.
   */
  authority_level: "primary" | "secondary" | null;
  /** Fingerprint of the anchored principal, when this node is `admin`. */
  authority_fingerprint: string | null;
  p2p_port: number;
  gateway_port: number;
  admin_exposure: AdminExposure;
  admin_password_configured: boolean;
  inference_port: number | null;
  config: NodeConfig | null;
  status: RuntimeStatus;
  system: SystemLoad;
  /** What the plugins running on this machine serve to the network. Absent on an old node. */
  plugin_services?: PluginService[];
  /** Disk used by this machine's KV snapshot directory (`kv-slots`). Absent on an old node. */
  kv_cache?: KvCacheDisk;
  peer_count: number;
  load: number;
  active_model: string;
  active_deployment?: string;
  /** True while the operator has taken this node off the network. */
  detached?: boolean;
  /** Commit the binary was built from. */
  build_commit: string;
  /** This node accepts updating itself through the interface (POM_DEV_UPDATE=1). */
  dev_update: boolean;
  /** Whether the development-only POM controls are exposed. */
  pom_dev_mode: boolean;
  /** The name the operator gave this machine; `null` means the system hostname. */
  node_name: string | null;
  /** Address pinned by the operator. `null` when nothing was pinned. */
  advertise_addr: string | null;
  /** What this node actually announces now (pinned, observed, or the sentinel). */
  effective_address: string;
  /** Whether the cluster prefers HTTP/3 over QUIC on new hops. */
  quic_enabled: boolean;
  /** Only the principal can change the QUIC preference. */
  quic_authority: boolean;
  /** Whether this node collects and sends the cluster's process logs. */
  cluster_logs_enabled: boolean;
  /** Only the principal can change the log policy. */
  cluster_logs_authority: boolean;
  /** Whether the global conversation channel is on at this node. */
  network_chat_enabled: boolean;
  /** Only the principal can change the network Chat policy. */
  network_chat_authority: boolean;
  thermal: ThermalPolicy;
}

export type NetworkVisibility = "local" | "private" | "public";
export type NetworkAccessPolicy = "authorized" | "auto_approve" | "anyone_with_anchor" | "local_only";

/** A role a new machine can enroll with; `admin` is never an enrollment. */
export type EnrollmentRole = "worker" | "visitor";
export const ENROLLMENT_ROLES: EnrollmentRole[] = ["worker", "visitor"];

export interface NetworkLocation {
  latitude: number;
  longitude: number;
  accuracy_m?: number | null;
}

export interface NetworkSettings {
  public_name: string;
  organization: string;
  description: string;
  rules: string;
  channels: string;
  /** The network's city: a row of the node's offline city database. */
  city_id?: number | null;
  country_code: string;
  country_name: string;
  state?: string;
  city: string;
  geo_label: string;
  /** How much of the placed point the network publishes. */
  location_precision?: "exact" | "approximate" | "city";
  visibility: NetworkVisibility;
  access_policy: NetworkAccessPolicy;
  /** Roles the network advertises as open to new members; empty means closed. */
  accepted_enrollments: EnrollmentRole[];
  /** Opens the lobby room of the network Chat to machines that are not members. */
  /** Publishes served models, node count and total VRAM to non-members. */
  public_stats: boolean;
  location: NetworkLocation | null;
}

export interface NetworkInfoView {
  network: NetworkSettings;
  can_edit: boolean;
}

export type FirewallProtocol = "Udp" | "Tcp";

export interface FirewallRuleStatus {
  kind: string;
  protocol: FirewallProtocol;
  port: number;
  allowed: boolean;
}

export interface WindowsFirewallStatus {
  supported: boolean;
  p2p_port: number;
  rules: FirewallRuleStatus[];
  all_allowed: boolean;
  automatic_attempted: boolean;
  can_retry: boolean;
  last_error: string | null;
  in_progress: boolean;
}

export interface TrustShareView {
  filename: string;
  document: string;
  connection_key: string | null;
  visibility: NetworkVisibility;
}

export type NetworkChatKind = "text" | "file" | "typing" | "typing_stopped" | "plugin_notice";

export interface NetworkChatRequest {
  action: {
    kind: "text" | "file" | "typing" | "plugin_notice";
    message_id: string;
    room_id: string;
    sender_node_id: string;
    recipient_node_id: string | null;
    text?: string;
    file_name?: string;
    mime_type?: string;
    data?: number[];
    is_typing?: boolean;
    /** Only in `plugin_notice` (a plugin's notice to the network). */
    plugin_code?: string;
    title?: string;
    body?: string;
    level?: "info" | "success" | "warning" | "error";
    sent_at: string;
  };
  api_key?: string;
  relayed?: boolean;
}

export interface NetworkChatEvent {
  messageId: string;
  roomId: string;
  senderNodeId: string;
  recipientNodeId: string | null;
  kind: NetworkChatKind;
  text: string | null;
  fileName: string | null;
  mimeType: string | null;
  fileData: number[];
  sentAt: string;
  /** Only in `plugin_notice`. */
  pluginCode?: string;
  title?: string;
  level?: "info" | "success" | "warning" | "error";
}

export interface NetworkChatParticipant {
  nodeId: string;
  hostname: string;
  online: boolean;
  profile: string;
}

/** Hardware a lobby candidate declares about itself. Not verified by anyone. */
export interface LobbyCard {
  hostname: string;
  os: string;
  arch: string;
  cpu_count: number;
  ram_total_mb: number;
  gpus: { name: string; vram_total_mb: number | null }[];
  /** Round trip to the network's principal, as the candidate measured it. */
  rtt_ms?: number | null;
  /** The principal sees this candidate through a relay circuit. */
  relayed?: boolean | null;
}

/** A machine in the lobby that is not a member of the network. */
export interface LobbyGuest {
  nodeId: string;
  card: LobbyCard;
}

/** One lobby message as the network's principal hands it to a candidate. */
export interface LobbyMessage {
  message_id: string;
  sender_node_id: string;
  sender_name: string;
  /** The sender is a member of the network, not another candidate. */
  member: boolean;
  text: string;
  sent_at: string;
}

/** What this node got back from another network's lobby. */
export interface LobbyExchange {
  accepted: boolean;
  error: string | null;
  /** This machine's `node_id`, as the principal derived it. */
  node_id: string;
  card: LobbyCard;
  messages: LobbyMessage[];
  /** The network takes admission requests but has its conversation off. */
  messages_closed?: boolean;
  /** This machine's admission request on that network, when it made one. */
  admission?: {
    key: string;
    name: string;
    role: EnrollmentRole;
    status: "pending" | "approved" | "rejected";
    /** Approved without having asked now: this machine was admitted before. */
    standing?: boolean;
    /** When declined: the Unix second from which this machine may ask again. */
    retry_at?: number | null;
  } | null;
}

export type NetworkChatRoom = "network" | "lobby";

export interface NetworkChatSnapshot {
  enabled: boolean;
  /** The room `messages` belongs to. */
  room?: NetworkChatRoom;
  /** The network accepts lobby chat from non-members right now. */
  lobbyEnabled?: boolean;
  /** Candidates present in the lobby, with their declared hardware. */
  lobbyGuests?: LobbyGuest[];
  messages: NetworkChatEvent[];
  participants: NetworkChatParticipant[];
  typing: NetworkChatEvent[];
  /** Plugin notices received in the last 24 h; in memory only on the node. */
  notices?: NetworkChatEvent[];
  dropped: number;
}

export interface PeerView {
  /** The plugin engine that runs this machine's model; null for llama.cpp. A pipeline is built on one engine. */
  engine?: string | null;
  /** That engine's part in a prefill/decode split; absent when the machine does both. */
  role?: Exclude<InferenceRole, "both"> | null;
  node_id: string;
  short_id: string;
  addr: string;
  model: string;
  start_layer: number;
  end_layer: number;
  total_layers: number;
  capabilities: ModelCapability[];
  inference_url: string | null;
  /** Address chosen to talk to this peer, and the measured latency. */
  route: { addr: string; rtt_ms: number } | null;
  gateway_url: string | null;
  /** In-flight requests as reported by the peer itself; `null` means it was not readable. */
  load: number | null;
  is_self: boolean;
  node: NodeInfo | null;
  unreachable: string | null;
  /**
   * Every known address of this peer is discredited and the node has already
   * asked the network for its current address.
   *
   * Not the same as `unreachable`: here there is work in progress. Temporary
   * IPv6 addresses rotate on their own, and the screen has to say "finding again"
   * instead of announcing a dead end that resolves on the next gossip round.
   */
  rediscovering: boolean;
}

/** How many bytes of a tensor group ended up in a buffer. */
export interface BufferBytes {
  buffer: string;
  bytes: number;
}

/**
 * Where llama.cpp put each layer, measured after the load.
 *
 * `device` is the device llama.cpp assigned to the block; `buffers` is where
 * its weights actually are. The two diverge when a buffer type override
 * comes in - the classic case is `fit` sending the experts of a MoE to the
 * CPU on a layer whose device is still the GPU. That is why the screen reads
 * the buffers, not the device.
 */
export interface LayerPlacement {
  layer: number;
  device: string;
  buffers: BufferBytes[];
}

export interface ModelPlacement {
  n_layer: number;
  /** What llama.cpp ended up using, not what was requested. */
  n_gpu_layers: number;
  has_tensor_overrides: boolean;
  devices: Array<{ name: string; description: string }>;
  layers: LayerPlacement[];
  /** Embeddings, final norm and head: everything that does not belong to a block. */
  output: { device: string; buffers: BufferBytes[] };
  totals: BufferBytes[];
}

/**
 * The context's memory per buffer. `context` is the KV cache - the real answer
 * to "did the KV stay on the GPU?", which the `offload_kqv` flag only requests.
 */
export interface ContextMemoryBuffer {
  buffer: string;
  device: string;
  model: number;
  context: number;
  compute: number;
}

/**
 * The llama.cpp parameters a node's context came up with.
 *
 * It comes from the live context, not from the node's `EngineConfig`: the KV in
 * RAM fallback rebuilds the context with another configuration, and the
 * operator needs to see what is running, not what was requested.
 */
export interface PeerLoadParams {
  n_ctx: number;
  n_ctx_seq: number;
  n_batch: number;
  n_ubatch: number;
  n_seq_max: number;
  n_threads: number;
  n_threads_batch: number;
  /** `null` when the diagnostics bridge did not answer. */
  flash_attn: boolean | null;
  /** Did Flash Attention come from the `AUTO` probe or from an explicit request? */
  flash_attn_auto: boolean | null;
  kv_unified: boolean;
  embeddings: boolean;
  n_rs_seq: number;
  offload_kqv: boolean;
  cache_type_k: KvCacheType;
  cache_type_v: KvCacheType;
  n_gpu_layers: number;
  fit_memory_applied: boolean;
}

/**
 * A service that a plugin on one machine offers the cluster (`host.services`),
 * such as Qdrant serving as the RAG database. The node validates and publishes;
 * the screen only draws.
 */
export interface PluginService {
  plugin_code: string;
  id: string;
  /** What it is for: `rag`. Open vocabulary; an unknown kind shows up as it came. */
  kind: string;
  name: string;
  status: "ready" | "starting" | "installing" | "waiting" | "error";
  version?: string;
  detail?: string;
  /** Kind `inference`: the GGUF architectures its engine runs. */
  architectures?: string[];
  /** Kind `inference`: the settings its engine takes, drawn by the deploy inspector. */
  settings?: EngineSetting[];
  /** Kind `inference`: its engine also runs layer slices, one stage per machine. */
  stages?: boolean;
  /** Kind `inference`: the POM deployment settings that apply to its engine. */
  pom_settings?: PomSetting[];
  /** Kind `inference`: its engine reads images through the deployment's vision projector (`mmproj`). */
  vision?: boolean;
}

/** A POM deployment setting an inference service can declare, by its `RuntimeParams` name. */
export type PomSetting = "prefill_max_inflight_chunks" | "hidden_state_dtype" | "inference_role";

/** A whole-model machine's part in a prefill/decode split of a plugin inference service. */
export type InferenceRole = "both" | "prefill" | "decode";

/** One setting an inference service's engine publishes (`host.services`). */
export interface EngineSetting {
  key: string;
  type: "number" | "integer" | "boolean" | "enum" | "list";
  /** Per language; `en` is always present. */
  label: Record<string, string>;
  description?: Record<string, string>;
  min?: number;
  max?: number;
  options?: string[];
  /** `pom`: drawn in the deployment's POM settings card, whatever the slices; absent: the engine's card. */
  placement?: "pom";
}

/** Values by setting key; an absent key keeps the engine's default. */
export type EngineSettingValues = Record<string, number | boolean | string | string[]>;

/** A machine's KV snapshot directory, measured by the node itself. */
export interface KvCacheDisk {
  /** `false` when `slot_save_path` is off on that machine. */
  enabled: boolean;
  path: string | null;
  snapshots: number;
  bytes: number;
}

/** The inference server's KV cache in RAM (`--cache-ram`). */
export interface KvCacheRam {
  enabled: boolean;
  /** `null` with the cache on: a machine with less than 12 GiB of RAM, or not measured yet. */
  limit_bytes: number | null;
  user_reserve_bytes: number | null;
  bytes: number;
  entries: number;
  tokens: number;
  parked: number;
  restored: number;
  evicted: number;
}

/** What `POST /api/ui/kv-cache/clear` returns. */
export interface KvCacheCleared {
  cleared: boolean;
  node_id: string;
  disk_files: number;
  disk_bytes: number;
  ram_entries: number;
  ram_bytes: number;
}

/** The `/health` of a machine's inference server, as the screen reads it. */
export interface PeerRuntimeHealth {
  start_layer: number;
  end_layer: number;
  n_ctx_total: number;
  n_ctx_per_session: number;
  max_sessions: number;
  offload_kqv: boolean;
  fit_memory_applied: boolean;
  n_gpu_layers_effective: number;
  n_ubatch_effective: number;
  /** Real backend setting on this peer, not inferred from a zero timing. */
  profile_sync_enabled?: boolean;
  /** Path selected by the GGUF; the per-request engine comes from the trace. */
  selected_path?: string;
  /** Effective ownership of the experts on this node, separate from the selected path. */
  moe_requested?: boolean;
  moe_executor?: "cpu-reference" | "cuda" | "not-installed" | "not-requested" | "not-run" | "mixed";
  moe_executor_dtypes?: number[];
  /** Concrete reason when the MoE executor was not installed or uses the CPU. */
  moe_executor_reason?: string | null;
  /** This node's KV cache in RAM. Absent on an old node. */
  cache_ram?: KvCacheRam;
  mtp_ready: boolean;
  /** Reason returned by the runtime when the requested MTP was not attached. */
  mtp_unavailable_reason?: string | null;
  /** Dominant location of the buffers of the ready MTP context. */
  mtp_allocation?: "vram" | "ram" | null;
  /** Which llama.cpp parameters this context came up with. */
  load_params?: PeerLoadParams;
  /** `null` on a node whose diagnostics bridge did not answer. */
  placement: ModelPlacement | null;
  context_memory: { buffers: ContextMemoryBuffer[] } | null;
}

/** Response of `/api/ui/peer/:node_id/runtime`. */
export interface PeerRuntime {
  loaded: boolean;
  /** Why there is no data, when `loaded` is false. */
  reason?: string;
  health?: PeerRuntimeHealth;
}

/** Directional RTT measured between two peers of the run shown in the chat. */
export interface ChatTelemetryRtt {
  from_node_id: string;
  to_node_id: string;
  rtt_ms: number;
  transport: string;
  /** The edge goes through a relay: a route to review. */
  relayed?: boolean;
}

export interface ChatTelemetryNetwork {
  measurements: ChatTelemetryRtt[];
  /** Observed cost of the full cycle, when the trace closed every leg. */
  pipeline_cost?: {
    legs_ms: Array<number | null>;
    compute_ms: number | null;
  } | null;
  /** Every possible complete route for the active model, from head to tail. */
  pipeline_routes?: Array<{
    node_ids: string[];
    legs_ms: Array<number | null>;
    rtt_ms: number | null;
    /** Compute of one decode step summed over the route's peers. */
    compute_ms?: number | null;
    /** Network paid per token in the chosen return mode (not the sum of the legs). */
    network_ms?: number | null;
    return_mode?: "direct" | "ring" | null;
    /** One token through the route: network in the chosen mode plus compute. */
    total_ms?: number | null;
    tokens_per_sec?: number | null;
    /** Some leg of the route goes through a relay. */
    relayed?: boolean;
    /** Which legs go through a relay, as `[from, to]`. */
    relayed_legs?: Array<[string, string]>;
    /** The direct return would go through a relay and the ring goes around it. */
    relay_bypassed_by_ring?: boolean;
  }>;
  /** Measured speed of each peer of the active model. */
  peer_perf?: Record<string, ChatTelemetryPeerPerf>;
}

/** Measured speed of a peer: test when serving and real traffic. */
export interface ChatTelemetryPeerPerf {
  prefill_tokens_per_sec: number | null;
  decode_tokens_per_sec: number | null;
  active_generations: number;
}

/** Response of the topology cut an infer node can query. */
export interface InferenceHeadCandidate {
  node_id: string;
  short_id: string;
  hostname: string;
  start_layer: number;
  end_layer: number;
  total_layers: number;
  /** In-flight requests reported by the candidate; null means unreadable. */
  load: number | null;
  runtime_capacity: { n_ctx: number; max_new_tokens: number } | null;
  rtt_ms: number | null;
  rtt_age_ms: number | null;
  rtt_state: "fresh" | "stale" | "unmeasured";
  selected: boolean;
}

export interface InferenceHeadsResponse {
  model: string;
  status: "selected" | "incomplete_pipeline" | "no_candidate" | "invalid_model";
  selected_node_id: string | null;
  selection_policy: "coverage_then_weighted_rtt_load";
  rtt_valid_for_secs: number;
  candidates: InferenceHeadCandidate[];
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
  /**
   * Machines that contribute layers to this coverage and could not be read
   * on this pass.
   *
   * The coverage comes from what the peers announce in the gossip, not from
   * reading each one. Without this list, an unreadable peer counts as healthy and
   * the screen says "complete" for a model half of which nobody can query.
   */
  unreadable_nodes: string[];
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
  /** Projector loaded at the head of the active deployment, when there is vision. */
  mmproj_path?: string | null;
  total_layers: number;
  covered_layers: number;
  coverage: boolean[];
  complete: boolean;
  model_inventory: ModelCoverage[];
  peers: PeerView[];
  routing_edges: RoutingEdge[];
  candidate_edges: RoutingEdge[];
  node: NodeInfo | null;
  /** Diagnostics of this node's NAT traversal. Absent on an old node. */
  traversal?: TraversalView;
}

/**
 * On which rung the NAT traversal stopped. Without this, "the two nodes do not
 * talk to each other" can only be investigated with a debug log on the right machine.
 */
export interface TraversalView {
  nat: "unknown" | "public" | "private";
  /** Published endpoint addresses: direct `ip:port` and relay URLs. */
  addrs: string[];
  /** URLs of the relays the endpoint holds a live connection to. */
  reservations: string[];
  hole_punch_ok: number;
  hole_punch_failed: number;
  last_hole_punch_error: string | null;
}

/** Aggregate facts the Chat may read from every profile without topology access. */
export interface ChatCapabilities {
  active_model: string;
  vision: "supported" | "unsupported" | "unavailable";
  coverage: "complete" | "incomplete" | "unavailable";
  /** Thinking is available only when the active deployment leaves it enabled. */
  thinking: "supported" | "unsupported" | "unavailable";
  /**
   * A machine of the active pipeline unloaded its model for idleness, or is
   * loading it again: the next answer waits for the reload.
   */
  asleep?: boolean;
  /** The network has a RAG server and retrieval is on. Absent on an older node. */
  rag?: "supported" | "unavailable";
  /** Collections of the RAG server a message may keep its search to. */
  rag_collections?: string[];
}

export type ModelSource = "ollama" | "lmstudio" | "folder" | string;

export interface MultipartPart {
  index: number;
  path: string;
  bytes: number;
  tensor_count: number;
}

export interface DiscoveredModel {
  id: string;
  name: string;
  source: ModelSource;
  source_label: string;
  path: string;
  /** Adjacent mmproj GGUF chosen by llama.cpp's convention. */
  mmproj_path?: string | null;
  /** Every compatible projector to choose from in the deployment. */
  mmproj_candidates?: string[];
  /** Reason when the catalog could not suggest a compatible projector. */
  mmproj_reason?: string | null;
  size_bytes: number;
  /** The GGUF declares split.count > 1 and this item represents the set. */
  multipart?: boolean;
  /** Physical parts of the set, in split.no order. */
  multipart_parts?: MultipartPart[];
  /** Concrete reason that prevents reading an incomplete multipart set. */
  multipart_error?: string | null;
  architecture: string | null;
  /** `<arch>.embedding_length`: the width a projector or MTP draft must match. */
  embedding_length?: number | null;
  /** Header evidence: `mtp` is true when the GGUF carries its own `nextn` block. */
  capabilities?: { thinking?: boolean | null; mtp?: boolean | null } | null;
  /** True only when the GGUF metadata reports more than one expert. */
  is_moe?: boolean;
  total_layers: number | null;
  quant: string | null;
  shard_range: [number, number] | null;
  /** Key of the slicing round: `<model>-<short id>`. */
  shard_group: string | null;
  /** sha256 of the part, when the file is content addressed. */
  sha256: string | null;
  /** Label of the source model, from the `.pom`. */
  model_label: string | null;
  /** The round that produced this part. */
  pom_id: string | null;
  /** Tensor rank, when the part belongs to a TP round. */
  tp_rank?: number | null;
  /** Size of the tensor group, when the part belongs to a TP round. */
  tp_size?: number | null;
}

/** A whole `.gguf`, as opposed to a slice of one. Slices carry `shard_range`. */
export const isWholeModel = (m: DiscoveredModel): boolean => m.shard_range == null;

/** `src/models/deployment.rs` - in what form a model is in service. */
export type DeploymentKind = 'shards' | 'whole' | 'tensor_parallel' | 'mixed';

/** `src/models/deployment.rs` - a node of the deployment, and the config it came up with. */
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
 * `src/models/deployment.rs` - a configuration this `deploy_id` has already run.
 *
 * What tells one version from another are the parameters of the whole
 * deployment (`params`). Per-machine adjustments go into the `nodes` snapshot
 * but do not fork a version.
 */
/** One machine's own values for the fields it may set (`src/models/deployment.rs`, `NodeOverrides`). Absent = inherited. */
export interface NodeOverrides {
  n_ctx?: number;
  max_sessions?: number;
  n_gpu_layers?: number;
  offload_kqv?: boolean;
  /** The machine's whole configuration (engine, settings, POM settings), which replaces the version's for it. */
  params?: RuntimeParams;
}

export interface DeploymentVersion {
  version: number;
  created_at: string;
  created_by: string;
  params: RuntimeParams;
  nodes: DeploymentNode[];
  /** The active version's per-machine overrides, by `node_id`. */
  overrides?: Record<string, NodeOverrides>;
}

/**
 * `src/models/deployment.rs` - the link between a set of nodes and a model.
 *
 * The same model can be in service in more than one deployment, with different
 * machines or configuration; the `deploy_id` is what tells them apart. A
 * request picks one by the suffix `<model>@<deploy_id>`.
 *
 * `versions` is the history of configurations of the same `deploy_id`;
 * `active_version` points to the one in service. `nodes` is the flattened list
 * of the active version.
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
  versions: DeploymentVersion[];
  active_version: number;
}

/** What `GET /api/ui/deployments` answers. */
export interface DeploymentsResponse {
  node_id: string;
  deployments: Deployment[];
  /** The `deploy_id`s that serve a request made with the model name alone. */
  active: string[];
}

export interface ShardSet {
  /** Label of the model - what one reads. `group` identifies the round. */
  model_label: string;
  /** Projector adjacent to this set or to its original whole model. */
  mmproj_path?: string | null;
  /** Every compatible projector to choose from in the deployment. */
  mmproj_candidates?: string[];
  /** Reason when the catalog could not suggest a compatible projector. */
  mmproj_reason?: string | null;
  /** The slicing round that produced these parts. Identification, not a name. */
  pom_id: string;
  group: string;
  total_layers: number | null;
  total_layers_inferred: boolean;
  shards: DiscoveredModel[];
  /** Complete parts of the model, one per TP rank. */
  tp_parts?: DiscoveredModel[];
  complete: boolean;
}

/** Reads the TP list of catalog responses without taking the screen down if it is missing. */
export const tpPartsOf = (set: { tp_parts?: DiscoveredModel[] }): DiscoveredModel[] =>
  Array.isArray(set.tp_parts) ? set.tp_parts : [];

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
  /** Vision projectors and MTP drafts found in this node's folders. */
  sidecars?: Sidecar[];
  scanned: ScannedRoot[];
  /** False while the node is reading GGUF headers in the background. */
  metadata_complete: boolean;
}

export type SidecarKind = "mmproj" | "mtp";
export type SidecarMode = "file" | "embedded" | "none";

/** A vision projector or an external MTP draft on one machine's disk. */
export interface Sidecar {
  kind: SidecarKind;
  path: string;
  file_name: string;
  size_bytes: number;
  architecture: string | null;
  /** Projection width for a projector, embedding width for an MTP draft. */
  dim: number | null;
  general_name: string | null;
  quant: string | null;
}

/** A sidecar named the way it crosses machines: name + size. */
export interface SidecarFile {
  file_name: string;
  size_bytes: number;
}

/** The operator's choice of sidecar for one model, kept on the principal. */
export interface SidecarLink {
  model_key: string;
  kind: SidecarKind;
  mode: SidecarMode;
  file_name?: string | null;
  size_bytes?: number | null;
  updated_at?: string;
  updated_by?: string;
}

export interface SidecarHolder {
  node_id: string;
  hostname: string;
  path: string;
}

/** One compatible file and every machine that holds it. */
export interface SidecarOption {
  kind: SidecarKind;
  file_name: string;
  size_bytes: number;
  general_name: string | null;
  quant: string | null;
  holders: SidecarHolder[];
}

/** How one sidecar reaches the machine that loads it in a plan. */
export interface SidecarSlot {
  kind: SidecarKind;
  mode: SidecarMode;
  file: SidecarFile | null;
  from_link: boolean;
  target_node_id: string;
  target_host: string;
  target_path: string | null;
  source_node_id: string | null;
  source_host: string | null;
  source_path: string | null;
  problem: string | null;
}

export interface SidecarsPlan {
  model_key: string | null;
  embedded_mtp: boolean;
  options: SidecarOption[];
  vision: SidecarSlot | null;
  mtp: SidecarSlot | null;
  links_stale: boolean;
}

/** What the screen asks for one kind; no mode leaves the saved link in charge. */
export interface SidecarChoice {
  mode?: SidecarMode;
  file?: SidecarFile | null;
}

export interface PeerCatalog {
  node_id: string;
  hostname: string | null;
  is_self: boolean;
  models: DiscoveredModel[];
  shard_sets: ShardSet[];
  sidecars?: Sidecar[];
  scanned?: ScannedRoot[];
  /** False while this peer is reading GGUF headers in the background. */
  metadata_complete?: boolean;
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
  workspace_root: PathEntry;
  using_defaults: boolean;
  detected: PathEntry[];
  default_shard_dir: string;
  default_workspace_root: string;
  shard_max_gb: number;
  /** Environment variable that sets the whole model folders, when it applies. */
  model_dirs_env?: string | null;
  /** Environment variable that sets the slices folder, when it applies. */
  shard_dir_env?: string | null;
}

export type KvCacheType =
  | "f32"
  | "f16"
  | "bf16"
  | "q8_0"
  | "q4_0"
  | "q4_1"
  | "iq4_nl"
  | "q5_0"
  | "q5_1";

export interface CacheTypesResponse {
  cache_type_k: KvCacheType;
  cache_type_v: KvCacheType;
  choices: KvCacheType[];
}

/** Inventory of the machine chosen in the add dialog. */
export interface AddNodeOption {
  kind: "shards" | "whole";
  start_layer: number;
  end_layer: number;
  label: string;
}

export interface AddNodeOptionsResponse {
  node_id: string;
  options: AddNodeOption[];
  cache_types: CacheTypesResponse;
  defaults: {
    n_gpu_layers: number;
    offload_kqv: boolean;
  };
}

/** A slicing or apply run. `src/backend/webui/jobs_apply.rs` - `Job`. */
export interface Job {
  id: string;
  state: "running" | "done" | "failed";
  step: string;
  message: string;
  progress: number | null;
  /** Bytes observed in the current phase, such as hashing the source. */
  bytes_processed?: number | null;
  /** Reused by slicing phase telemetry and remote download telemetry. */
  current_file?: string | null;
  phase_started_at?: string;
  outputs: string[];
  started_at: string;
  finished_at: string | null;
  /** "slice" | "apply" — added by `GET /api/ui/jobs`. */
  kind?: string;
  /** Human label for the notification list. */
  title?: string;
  /** Fields present when `kind === "model_download"`. */
  repo_id?: string;
  revision?: string | null;
  filename?: string;
  quantization?: string | null;
  download_status?: RemoteDownloadStatus;
  bytes_downloaded?: number | null;
  total_bytes?: number | null;
  speed_bytes_per_sec?: number | null;
  eta_seconds?: number | null;
  local_path?: string | null;
  error?: string | null;
}

export type RemoteDownloadStatus =
  | "queued"
  | "starting"
  | "downloading"
  | "verifying"
  | "completed"
  | "cancelled"
  | "failed";

export interface RemoteModelSummary {
  repo_id: string;
  author: string | null;
  description: string | null;
  downloads: number | null;
  likes: number | null;
  private: boolean | null;
  gated: boolean | null;
  tags: string[];
  last_modified: string | null;
  has_gguf: boolean | null;
  installed: boolean;
  downloading: boolean;
}

export interface RemoteModelSearchResponse {
  models: RemoteModelSummary[];
  total: number;
  page: number;
  page_size: number;
  has_next: boolean;
}

export interface RemoteModelFile {
  filename: string;
  size_bytes: number | null;
  sha256: string | null;
  lfs: boolean | null;
  installed: boolean;
  downloading: boolean;
  download_id: string | null;
}

export interface RemoteGgufFile {
  filename: string;
  size_bytes: number | null;
  quantization: string | null;
  parameter_size: string | null;
  base_name: string | null;
  installed: boolean;
  downloading: boolean;
  download_id: string | null;
}

export interface RemoteStorage {
  available_bytes: number | null;
  destination_available: boolean;
  destination_paths: string[];
}

export interface RemoteModelProviderStatus {
  cli: {
    installed: boolean;
    version: string | null;
    authenticated: boolean;
    username: string | null;
  };
}

/** Safe state of the token: the raw secret is never part of the UI contract. */
export interface HuggingFaceTokenConfig {
  configured: boolean;
  masked: string | null;
  source: "HF_TOKEN" | "HUGGING_FACE_HUB_TOKEN" | "pom.json" | "~/.cache/huggingface/token" | "none";
}

export interface RemoteModelDetails {
  repo_id: string;
  description: string | null;
  last_modified: string | null;
  revision: string | null;
  sha: string | null;
  downloads: number | null;
  likes: number | null;
  private: boolean | null;
  gated: boolean | null;
  tags: string[];
  license: {
    id: string | null;
    name: string | null;
    url: string | null;
    raw_value: string | null;
    detected: boolean;
  };
  files: RemoteModelFile[];
  gguf_files: RemoteGgufFile[];
  storage: RemoteStorage;
}

export interface TransferJob {
  id: string;
  name: string;
  /** Which side of this machine the transfer is on. The id is the same on both. */
  direction: "sending" | "receiving";
  /** The other machine. */
  peer_host: string;
  /** Local file involved. */
  path: string;
  state: "running" | "verifying" | "done" | "failed" | "cancelled";
  bytes_done: number;
  bytes_total: number;
  bytes_per_sec: number;
  eta_secs: number | null;
  detail: string;
  /**
   * Which model this transfer belongs to, and which layers it brings - from the `.pom`.
   *
   * The `name` of a part is its sha256, and a hash identifies nothing one can
   * read. Absent when the file is not part of any slicing (a whole model) or
   * when the node is of an earlier version.
   */
  model_label?: string | null;
  start_layer?: number | null;
  end_layer?: number | null;
  /**
   * When the job entered a terminal state, in RFC 3339. Absent while it runs,
   * and also on a node of an earlier version, which did not stamp the time.
   */
  finished_at?: string | null;
}

export interface TransferOffer {
  /** Explicit kind in the new contract; absent only when the server does not yet
   * annotate slice offers, which remain the default case. */
  kind?: "slice";
  /** Terminal result, when the server keeps the outcome in the bell. */
  status?: "pending" | "accepted" | "rejected" | "expired";
  id: string;
  from_node_id: string;
  from_host: string;
  from_gateway: string;
  remote_path: string;
  grant: string | null;
  name: string;
  size_bytes: number;
  /**
   * Always empty. The one that checks the file is this node, after accepting,
   * with the digest it asks for in `/api/transfer/manifest`; computing it to offer
   * held the notification for the time it takes to read the whole file.
   */
  md5: string;
  start_layer: number | null;
  end_layer: number | null;
  /**
   * The manifest of the slicing this part belongs to, when it is one.
   *
   * It travels **along with the offer**, and not afterwards: it is what says which
   * model the part is from. Without this the notification only had the `<sha256>.gguf` to show.
   */
  pom?: OfferManifest | null;
}

/** Offer on the same bell that transfers the principal role. */
export interface AdminTransferOffer {
  kind: "principal_transfer";
  id: string;
  from_node_id: string;
  from_host: string;
  target_node_id: string;
  target_host: string;
  status: "pending" | "accepted" | "rejected" | "expired";
  issued_at: string;
}

/** Every request that can show up in the activity bell. */
export type NetworkOffer =
  | TransferOffer
  | AdminTransferOffer;

/** `crate::pom::PomManifest`, in the part the screen uses. */
export interface OfferManifest {
  pom_id: string;
  model: {
    name: string;
    label: string;
    total_layers: number;
    quant?: string | null;
  };
}

/** A live endpoint connection with the peer, as its selected path stands now. */
export interface PeerRouteLink {
  address: string;
  relayed: boolean;
  /** URL of the relay, when the path goes through one. */
  relay_url?: string | null;
  hole_punched: boolean;
}

/**
 * Through where this node actually talks to the peer, observed - not the
 * address the peer announces. `http`: a direct address answered; `p2p_direct`:
 * direct endpoint path (dialed or by hole punch); `relayed`: only through a
 * relay. `relay_kind` says, in `relayed`, whether the relay is dedicated
 * (configured), public (one of n0's) or a cluster member serving as relay.
 */
export interface PeerRoute {
  kind: "http" | "p2p_direct" | "relayed" | "none";
  address?: string | null;
  relay_kind?: "dedicated" | "public" | "member" | null;
  hole_punched: boolean;
  links: PeerRouteLink[];
}

export interface AdminNode {
  node_id: string;
  short_id: string;
  hostname: string;
  /** What the peer announces; the route in use is in `route`. */
  addr: string;
  route?: PeerRoute | null;
  role?: string;
  /** The node's role in the network: `admin`, `worker` or `visitor`. */
  profile?: string;
  /** The trust chain version the node announces. */
  trust_version?: string | null;
  model?: string;
  start_layer?: number;
  end_layer?: number;
  total_layers?: number;
  is_self: boolean;
  connected: boolean;
  blocked: boolean;
  verified: boolean;
}

export type AdminAuthorityViewer = "principal" | "admin" | "member";

export interface AdminAuthorityMember {
  node_id: string;
  short_id: string;
  hostname: string;
  active: boolean;
}

export interface AdminGrant {
  sequence: number;
  node_id: string;
  node_hostname: string;
  promoted_by_node_id: string;
  promoted_by_hostname: string;
}

export type AdminResignationState =
  | { kind: "eligible"; successor: AdminAuthorityMember }
  | { kind: "no_secondary" }
  | { kind: "no_active_secondary" };

/** Presentation contract. It does not carry the choice of key, signature or storage. */
export interface AdminAuthorityView {
  viewer: AdminAuthorityViewer;
  principal: AdminAuthorityMember;
  secondary: AdminAuthorityMember[];
  grants: AdminGrant[];
  resignation: AdminResignationState;
  pending_transfer: AdminTransferOffer | null;
  permissions: {
    promote: boolean;
    resign: boolean;
    demote: boolean;
  };
}

/**
 * This machine's root of trust.
 *
 * Presentation contract, like the authority one: the fingerprint is public
 * and exists for the operator to check it by eye; the decision to install is
 * the server's, and `can_install` only says whether the form is worth drawing.
 */
export interface TrustAnchorView {
  anchored: boolean;
  fingerprint: string | null;
  cluster_id: string | null;
  anchor_node_id: string | null;
  /** The root is this node's key, and not one it received. */
  is_anchor: boolean;
  /** This connection arrived over loopback, measured on the real socket. */
  loopback: boolean;
  can_install: boolean;
  trust_filename: string;
}

/** Refusal codes of the server when installing the anchor, translated on the screen. */
export type TrustAnchorRefusal =
  | "not_loopback"
  | "already_anchored"
  | "invalid_document"
  | "fingerprint_mismatch"
  | "install_failed";

export interface ApiKey {
  /** Tracking id: identifies the key in every node's logs. Never a secret. */
  id: string;
  /** Start and end of the secret (`sk-pom-3f9a…c41e`). The secret itself is
   *  never listed: the network keeps only its hash. */
  key_hint: string;
  label: string;
  node_id: string;
  origin_node_id?: string;
  active: boolean;
  created_at: string;
}

/** The response that mints a key: the only one that carries the secret. */
export interface IssuedApiKey extends ApiKey {
  key: string;
}

/** Request-level performance, aggregated over the whole inference pipeline. */
export interface GenerationTimings {
  local_engine_first_event_ms?: number | null;
  pipeline_first_token_ms?: number;
  ttft_ms: number;
  /** Gateway TTFT from request ingress to protocol emission. */
  server_ttft_ms?: number | null;
  /** First-token timing measured by the inference peer. */
  engine_ttft_ms?: number | null;
  /** Browser TTFT measured with performance.now(). */
  client_ttft_ms?: number | null;
  prefill_ms: number;
  prefill_tokens: number;
  prefill_tokens_per_sec: number | null;
  generation_ms: number;
  generation_tokens: number;
  generation_tokens_per_sec: number | null;
  total_ms: number;
  total_tokens_per_sec: number;
}

/** Process log line persisted by the principal node. */
export interface ClusterLogEntry {
  received_seq: number;
  received_at: string;
  node_id: string;
  node_name: string;
  deploy_id: string;
  origin_at: string;
  level: string;
  target: string;
  message: string;
}

export interface ClusterLogResponse {
  entries: ClusterLogEntry[];
  dropped: number;
}

/** One slice of a size-driven plan. `src/models/plan.rs` — `SizedRange`. */
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

/** A cut accepted by the model's real GGUF layout. */
export interface LayerCut {
  end_layer: number;
  layers: number;
  size_bytes: number;
  remainder_bytes: number;
}

/** Layer cuts the GGUF accepts for an independent slice. */
export interface SliceLayerOptions {
  total_layers: number;
  valid_starts: number[];
  cuts: LayerCut[];
  granularity: number | null;
  rule: string;
}

/** `src/models/plan.rs` — `SizePlan`, what `POST /api/ui/slice/plan` answers. */
export interface SizePlan {
  mode?: "pipeline";
  total_layers: number;
  model_bytes: number;
  /** Source GGUF size before embeddings and slice metadata are duplicated. */
  source_model_bytes?: number;
  /** Real required head payload repeated by each non-final slice. */
  required_head_bytes?: number;
  ranges: SizedRange[];
  covered_layers: number;
  complete: boolean;
  missing_range: [number, number] | null;
  missing_bytes: number;
  blocked: boolean;
  warnings: string[];
  destination_path?: string;
  destination_free_bytes?: number | null;
  space_margin_bytes?: number | null;
  space_status?: "ok" | "tight" | "insufficient" | "unknown";
}

/** One rank of a tensor slicing plan. */
export interface TpSliceRank {
  rank: number;
  start_layer: number;
  end_layer: number;
}

/** `src/backend/webui/slicing.rs` - Tensor Parallelism slicing plan. */
export interface TpSlicePlan {
  mode: "tensor_parallel";
  total_layers: number;
  model_bytes: number;
  tp_size: number;
  ranks: TpSliceRank[];
  complete: boolean;
  blocked: boolean;
  warnings: string[];
  destination_path?: string;
  destination_free_bytes?: number | null;
  space_margin_bytes?: number | null;
  space_status?: "ok" | "tight" | "insufficient" | "unknown";
}

export type SliceMode = "pipeline" | "tensor_parallel";
export type SlicePlan = SizePlan | TpSlicePlan;

/** One peer in a memory-budget plan. `src/models/plan.rs` — `PlannedShard`. */
export interface PlannedShard {
  node_id: string;
  label: string;
  budget_mb: number;
  usable_mb: number;
  start_layer: number;
  end_layer: number;
  layers: number;
  weight_mb: number;
  weight_bytes: number;
  kv_bytes: number;
  total_bytes: number;
  is_first: boolean;
  is_last: boolean;
  has_model: boolean;
}

/** `src/models/plan.rs` — `SplitPlan`, what `POST /api/ui/plan` answers. */
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

/** One machine offered as the target of one slice. `src/backend/deploy/`. */
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
  /** Range actually executed; absent in old responses. */
  exec_start?: number;
  exec_end?: number;
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
  /** Per-machine prefill/decode role; absent means the deployment's `params.inference_role`. */
  inference_role?: InferenceRole | null;
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
  /** Projected VRAM/RAM this step would take on its target, under the plan's
   *  settings. Absent until a locally-materialized step is previewed, or when
   *  that preview failed. TP plans and not-yet-merged steps never carry it. */
  capacity?: CapacityPreview | null;
}

/** One GPU's projected use for a deploy step or an aggregated machine.
 *  `src/backend/webui/params.rs` - `GpuCapacityProjection`. */
export interface GpuCapacityProjection {
  index: number;
  vram_total_mb: number;
  /** Measured now, before this deployment. */
  vram_used_mb: number;
  /** `vram_used_mb` plus the share of this step/machine that would land here. */
  vram_projected_mb: number;
}

/** A single deploy step's capacity projection. `src/backend/webui/params.rs` -
 *  `CapacityPreview`. */
export interface CapacityPreview {
  /** `vram_projected_mb <= vram_total_mb` on every device. `true` on a
   *  machine with no discrete GPU — nothing to run out of. */
  fits: boolean;
  /** Raw estimate does not fit, but the default all-layers request has
   *  `fit_memory` enabled and the real load may trim it. This is a heuristic. */
  degraded?: boolean;
  /** Operator-ready sentence: possible adjustment, analytic split, or KV in
   *  host RAM. The real load confirms the final placement. */
  note?: string | null;
  effective_n_gpu_layers?: number | null;
  effective_n_ctx?: number | null;
  gpus: GpuCapacityProjection[];
}

/** A whole machine's projection: the sum, per GPU, of every step it would
 *  take. `src/backend/deploy/planning.rs` - `MachineCapacity`. */
export interface MachineCapacity {
  fits: boolean;
  /** Some step of this machine received the same possible-adjustment heuristic. */
  degraded?: boolean;
  note?: string | null;
  gpus: GpuCapacityProjection[];
}

/** One pre-sliced tensor-parallel rank in a deploy plan. */
export interface DeployTpTarget {
  rank: number;
  size: number;
  start_layer: number;
  end_layer: number;
  total_layers: number | null;
  name: string;
  size_bytes: number;
  target_node_id: string;
  target_host: string;
  ready: boolean;
  source_node_id: string | null;
  source_host: string | null;
  source_path: string | null;
  source_sha256: string | null;
  target_path: string | null;
  problem: string | null;
  n_gpu_layers: number | null;
  offload_kqv: boolean | null;
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
  gpu_free_mb: number;
  ram_free_mb: number;
  /** Live metrics of the cluster view, absent in old responses. */
  gpu_used_pct?: number | null;
  ram_used_pct?: number | null;
  disk_free_mb: number;
  rtt_ms: number | null;
  /** In-flight requests observed by the survey; `null` means not observed. */
  load: number | null;
  holds: Array<[number, number]>;
  taking: [number, number] | null;
  /** Post-deployment projection for this machine — the sum of the steps it
   *  would take, per GPU. Absent until previewed, or when no step of this
   *  machine is materialized locally yet. */
  capacity?: MachineCapacity | null;
}

export type DeployStrategy = "simple" | "complete";

export interface DeployRouteOption {
  from_node_id: string;
  to_node_id: string;
  rtt_ms: number | null;
  selected: boolean;
  note: string;
}

export interface DeployLoadDistribution {
  node_id: string;
  current: number | null;
  projected: number | null;
  selected: boolean;
}

/**
 * Execution settings a launch carries. `src/backend/webui/jobs.rs` - `RuntimeParams`.
 *
 * Cluster-wide on purpose: `n_ctx` is the full window for each session, the
 * session count multiplies the total KV-cache reservation, and the batch
 * decides the prompt chunks the gateway sends. Sampling only runs on the last
 * peer.
 * `n_gpu_layers` is the default a peer takes when it has no override of its own.
 * `max_new_tokens` is the backend leg size only. The total output capacity is
 * derived from the live runtime context and is read from `/v1/models`; there
 * is no absolute output-capacity knob in this deployment shape.
 */
export type MtpPlacement = "auto" | "vram" | "ram";

export interface RuntimeParams {
  /** Engine preference; the runtime decides from GGUF metadata in auto mode. */
  inference_mode: InferenceMode;
  /** The plugin whose inference service runs the whole model instead of
   *  llama.cpp (`"strata"`); absent is POM's own engine. */
  inference_service?: string | null;
  /** That engine's settings, by the keys it publishes; absent = its defaults. */
  engine_settings?: EngineSettingValues;
  /** Format negotiated for the hidden state between peers; absent in old config = f16. */
  hidden_state_dtype?: WireDtype;
  /** A whole-model plugin engine's part in a prefill/decode split; absent = both. */
  inference_role?: InferenceRole;
  n_gpu_layers: number;
  n_ctx: number;
  offload_kqv: boolean;
  fit_memory: boolean;
  /** VRAM left free by llama.cpp's automatic layer fit, per device. */
  fit_memory_margin_mb: number;
  n_batch: number;
  max_new_tokens: number;
  max_sessions: number;
  prefill_chunk_tokens: number;
  /** Maximum number of prefill chunks allowed in flight. */
  prefill_max_inflight_chunks: number;
  temperature: number;
  top_p: number;
  top_k: number;
  repeat_penalty: number;
  repeat_last_n: number;
  /** Subtracted once from the logit of each token seen in the window; 0 is off. */
  presence_penalty?: number;
  /** Subtracted per occurrence of a token in the window; 0 is off. */
  frequency_penalty?: number;
  /** Drops candidates less likely than this fraction of the most likely one; 0 is off. */
  min_p?: number;
  /** Seed of the sampler's generator, as text: a 64-bit integer does not fit a JS number. */
  seed?: string;
  /** DRY sampler: penalty for extending a sequence already seen; 0 is off. */
  dry_multiplier?: number;
  dry_base?: number;
  dry_allowed_length?: number;
  dry_penalty_last_n?: number;
  /** Physical micro-batch under the memory fit; never more than `n_batch`. */
  n_ubatch?: number;
  /** Token limit of a thinking block; -1 (or absent) is no limit. */
  reasoning_budget?: number;
  no_thinking: boolean;
  no_embeddings: boolean;
  /** Operator's kill-switch: even with every peer capable,
   *  speculation only turns on here. */
  speculative_decoding_enabled: boolean;
  /** Width of the draft: the confirmed token plus up to `k - 1` guesses. */
  speculative_k: number;
  /** Largest n-gram the prompt-lookup draft tries to repeat. */
  speculative_lookup_window: number;
  /** Multi-token prediction by the model's own `nextn` block.
   *  A switch **separate** from the one above: they are different mechanisms, and
   *  only this one uses trained weights. It only kicks in when one machine serves
   *  the whole model - see the engine's `mtp` module (`llama-engine`). */
  mtp_enabled: boolean;
  /** How many tokens the `nextn` head drafts per round. */
  mtp_draft_tokens: number;
  /** Minimum confidence for the draft to continue; 0 accepts any guess. */
  mtp_min_probability: number;
  /** Allows the MTP to fall back to weights and cache in RAM when VRAM fails. */
  mtp_cpu_fallback_enabled?: boolean;
  /** Where the MTP block goes: with the model when it all fits (auto), always in VRAM, or always in RAM. */
  mtp_placement?: MtpPlacement;
  unload_if_idle?: boolean;
  idle_minutes?: number;
  /** Every routed MoE expert stays in RAM and the CPU computes it (llama.cpp `--cpu-moe`). */
  moe_experts_in_ram?: boolean;
  /** GPU cache, in MiB, for the experts kept in RAM; 0 turns it off. */
  moe_cache_mib?: number;
  /** The gateway picks each prefill chunk; off, it uses `prefill_chunk_tokens` as is. */
  adaptive_prefill_chunks?: boolean;
}

/** Precisions the inference protocol can convert end to end. */
export type WireDtype = "f32" | "f16" | "f8" | "i4g64" | "i4g128";

export type CapabilityState = "supported" | "unsupported" | "unknown";

export interface RuntimeCapability {
  state: CapabilityState;
  source: string;
  reason: string;
  /** Execution limitation that does not prove absence in the selected model. */
  warning?: string;
}

export interface DeploymentCapabilities {
  fit_memory: RuntimeCapability;
  no_thinking: RuntimeCapability;
  no_embeddings: RuntimeCapability;
  speculative_decoding: RuntimeCapability;
  mtp: RuntimeCapability;
  /** Ability of every selected peer to receive f8 hidden state. */
  hidden_state_f8?: RuntimeCapability;
  /** Ability of every selected peer to receive i4g64 hidden state. */
  hidden_state_i4g64?: RuntimeCapability;
  /** Ability of every selected peer to receive i4g128 hidden state. */
  hidden_state_i4g128?: RuntimeCapability;
}

export type InferenceMode = "auto" | "dense" | "moe";

/** What the operator picked, as `POST /api/ui/deploy{,/plan}` accepts it. */
export interface DeploySelection {
  start_layer: number;
  end_layer: number;
  target_node_id: string;
  /** Overrides `params.n_gpu_layers` for this machine alone. */
  n_gpu_layers?: number | null;
  /** This machine's part in a prefill/decode split of a whole model on a plugin's
   *  inference service; `null` takes `params.inference_role`. */
  inference_role?: InferenceRole | null;
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
 * What is being put into service. `src/backend/deploy/types.rs` - `DeployKind`.
 *
 * Not two flows: a whole model is read by the node as a single range covering
 * every layer, so the plan, the copies and the launch are the same ones the
 * slice path uses. What the screen does differently is stop asking questions
 * that only have one answer — the order of a pipeline of one, and which layers
 * of all of them to include.
 */
export type DeployKind = "shards" | "whole" | "tensor_parallel";

/**
 * Tensor parallelism group (path C), `src/backend/deploy/launch.rs` - `TpLaunch`.
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
  deployment_name: string;
  strategy: DeployStrategy;
  /**
   * Echoed by the node, so the screen never guesses which question it asked.
   *
   * Optional because a peer running an older binary does not send it, and that
   * peer only ever answers about shards — read it as `"shards"` when absent,
   * the same default the node applies to a request without a `kind`.
   */
  kind?: DeployKind;
  /** Adjacent projector found for the chosen model. */
  mmproj_path?: string | null;
  /** Every compatible projector on the head peer. */
  mmproj_candidates?: string[];
  /** Reason when the catalog could not suggest a compatible projector. */
  mmproj_reason?: string | null;
  /** True only when the surveyed model's actual GGUF metadata is MoE. */
  is_moe?: boolean;
  /** Whether this plan can honor the selected inference mode. */
  inference_mode_supported?: boolean;
  /** Why the selected inference mode cannot be honored, when applicable. */
  inference_mode_reason?: string | null;
  /** Conditional capabilities worked out by the backend for the current targets. */
  capabilities?: DeploymentCapabilities;
  /** `null` means no slice declares it and none starts at layer 0, which is a
   *  `blocked` plan rather than a model of zero layers. */
  total_layers: number | null;
  /** True when the total was read off the slices' coverage instead of their
   *  metadata. The deploy still runs; the screen has to say so. */
  total_layers_inferred: boolean;
  steps: DeployStep[];
  /** TP ranks are alternatives, not additional pipeline stages. */
  tp_targets?: DeployTpTarget[];
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
  route_options: DeployRouteOption[];
  load_distribution: DeployLoadDistribution[];
  /** Vision projector for the head and MTP draft for the tail. */
  sidecars?: SidecarsPlan;
}
