/**
 * Shapes the node actually serves.
 *
 * Authoritative sources, in the Rust crate: `src/webui.rs` (node, cluster,
 * catalog, jobs), `src/sysmon.rs` (system load), `src/runtime.rs` (runtime
 * status and node config), `src/transfer.rs` (transfers), `src/admin.rs`
 * (nodes and keys). Anything typed loosely here is loose in the API too.
 */
import type { Profile } from "./profile";

export interface MemStat {
  total_mb: number;
  used_mb: number;
  available_mb: number;
  used_pct: number;
}

/** De quem é a placa. `GpuKind` descreve a máquina; isto, uma peça. */
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

/** Resumo da máquina. `mixed` = placas de marcas diferentes juntas. */
export type GpuKind = "nvidia" | "amd" | "mixed" | "unified" | "none";

export interface DiskStat {
  total_mb: number;
  free_mb: number;
  mount_point: string;
  /** Dispositivo por trás da montagem (`/dev/nvme0n1p2`); vazio quando não se sabe. */
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
  /** O volume onde as fatias moram - o que responde "cabe outra?". */
  disk: DiskStat;
  /**
   * Todo volume montado na máquina, um por partição.
   *
   * Lista vazia é "não perguntei", não "não tem disco": um nó velho não
   * publica este campo, e a tela recai no `disk`.
   */
  disks: DiskStat[];
  sampled_at: string;
}

/**
 * The inference server's state on a node — `ServerState` in `src/runtime.rs`,
 * serialised lowercase. No `| string` escape hatch on purpose: the first
 * version of this type invented "stopped" and "running", and because anything
 * was assignable every screen compared against a value the node never sends.
 * A machine serving layers looked idle on every panel.
 */
export type ServerState = "disabled" | "starting" | "ready" | "idle" | "failed" | "thermal";

/** Motor que o backend afirma ter executado, sem inferência da interface. */
export type ExecutionEngine = "llama.cpp" | "moe" | "unknown";

export interface RuntimeStatus {
  state: ServerState;
  detail: string;
  pid: number | null;
  restarts: number;
  log_path: string | null;
  reported_total_layers: number | null;
  /** Camadas que o llama.cpp de fato pôs na GPU, relatadas pelo motor depois de
   *  carregar. Não confundir com `NodeConfig.n_gpu_layers`, que é o pedido.
   *  Negativo é "todas", na convenção do llama.cpp. Ver `layerPlacement`. */
  reported_gpu_layers: number | null;
  changed_at: string;
}

export interface NodeConfig {
  model_path: string;
  /** Projector multimodal carregado neste nó, quando a visão está ligada. */
  mmproj_path?: string | null;
  model_label: string;
  /** A implantação de que este nó faz parte; vazio se ele não veio de uma. */
  deploy_id: string;
  /** Nome operacional da implantação, quando informado pelo operador. */
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

/** Fronteira da superfície administrativa, distinta das rotas da pipeline. */
export type AdminExposure = "localhost" | "network";

/** Metadados públicos usados para decidir se a catraca deve aparecer. */
export interface AdminAccessStatus {
  admin_exposure: AdminExposure;
  requires_password: boolean;
  /** A senha em vigor é a padrão do primeiro acesso (`admin`). */
  default_password?: boolean;
  /** Identifies this run of the node; changes on every restart. */
  boot_id?: string;
  gateway_port: number;
}

/** Estado administrativo consultado depois que a página já foi autenticada. */
export interface AdminAccessInfo {
  admin_exposure: AdminExposure;
  gateway_port: number;
  /** Only presence is exposed; the Argon2id hash never crosses the API. */
  password_configured: boolean;
}

/**
 * Estado local de um plugin independente (ver `crates/node/src/plugins.rs`),
 * comprado/ativado/desativado separadamente dos demais. O nó cria runtimes
 * dinamicamente para os `plugin_code` concedidos pelo servidor; a tela de
 * Plugins cruza esses estados locais com a vitrine pública, que também contém
 * plugins sem status local nesta instalação.
 */
export type PluginLicenseState =
  | "needs_activation"
  | "active"
  | "degraded"
  | "expired"
  | "revoked"
  | "unavailable";

/** Uma atualização de versão disponível - do plugin em si, nunca do `pom_core`. */
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
  /** Descrição por idioma declarada no manifesto do repositório. */
  description?: Record<string, string>;
  /** Capturas de tela servidas direto pelo GitHub (`raw.githubusercontent.com`). */
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
  stage?: "inspect" | "metadata" | "compile" | "install";
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
 * Atualização do binário `pom` em si. Fronteira de produto deliberadamente
 * separada dos plugins (ver `crates/node/src/pom_core_update.rs`): nunca
 * misture este tipo com `PluginStatus`/`PluginUpdateInfo` na UI.
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
  /**
   * A chave de conexão do cluster, que esta interface mostra em "Chave de
   * Conexão" e guarda para autenticar as chamadas que a exigem.
   */
  connection_key: string;
  /**
   * Autoridade administrativa **efetiva** deste nó, e não o perfil: um
   * `profile: admin` sem a concessão assinada vem `null`. É o que decide se a
   * tela oferece um ato administrativo ou só o mostra.
   */
  authority_level: "primary" | "secondary" | null;
  /** Impressão digital do principal ancorado, quando este nó é `admin`. */
  authority_fingerprint: string | null;
  p2p_port: number;
  gateway_port: number;
  admin_exposure: AdminExposure;
  admin_password_configured: boolean;
  inference_port: number | null;
  config: NodeConfig | null;
  status: RuntimeStatus;
  system: SystemLoad;
  /** O que os plugins em execução nesta máquina servem para a rede. Ausente num nó antigo. */
  plugin_services?: PluginService[];
  /** Disco usado pelo diretório de snapshots de KV desta máquina (`kv-slots`). Ausente num nó antigo. */
  kv_cache?: KvCacheDisk;
  peer_count: number;
  load: number;
  active_model: string;
  active_deployment?: string;
  /** True while the operator has taken this node off the network. */
  detached?: boolean;
  /** Commit de que o binário foi construído. */
  build_commit: string;
  /** Este nó aceita atualizar-se pela interface (POM_DEV_UPDATE=1). */
  dev_update: boolean;
  /** Se os controles POM exclusivos de desenvolvimento estão expostos. */
  pom_desenv: boolean;
  /** Endereço fixado pelo operador. `null` quando nada foi fixado. */
  advertise_addr: string | null;
  /** O que este nó anuncia agora de fato (fixado, observado, ou o sentinela). */
  effective_address: string;
  /** Se o cluster fala TLS entre peers agora. `webui.rs` - `NodeInfo`. */
  tls_required: boolean;
  /** Estado do pedido deste nó infer para participar como node. */
  node_promotion_request?: "none" | "pending" | "rejected";
  /** Se **este** nó pode mudar isso: só o principal pode. */
  tls_authority: boolean;
  /** Se o cluster prefere HTTP/3 sobre QUIC nos novos hops. */
  quic_enabled: boolean;
  /** Só o principal pode alterar a preferência QUIC. */
  quic_authority: boolean;
  /** Se este nó coleta e envia logs de processo do cluster. */
  cluster_logs_enabled: boolean;
  /** Só o principal pode mudar a política de logs. */
  cluster_logs_authority: boolean;
  /** Se o canal global de conversa está ligado neste nó. */
  network_chat_enabled: boolean;
  /** Só o principal pode mudar a política do Chat da rede. */
  network_chat_authority: boolean;
  thermal: ThermalPolicy;
}

export type NetworkVisibility = "local" | "private" | "public";
export type NetworkAccessPolicy = "authorized" | "anyone_with_anchor" | "local_only";

/** A role a new machine can enroll with; `admin` is never an enrollment. */
export type EnrollmentRole = "node" | "infer";
export const ENROLLMENT_ROLES: EnrollmentRole[] = ["node", "infer"];

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
  country_code: string;
  country_name: string;
  city: string;
  geo_label: string;
  visibility: NetworkVisibility;
  access_policy: NetworkAccessPolicy;
  /** Roles the network advertises as open to new members; empty means closed. */
  accepted_enrollments: EnrollmentRole[];
  /** Opens the lobby room of the network Chat to machines that are not members. */
  lobby_chat: boolean;
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

export interface GeoCityReference {
  id: string;
  name: string;
  latitude: number;
  longitude: number;
}

export interface GeoCountryReference {
  code: string;
  name: string;
  cities: GeoCityReference[];
}

export interface GeoReferenceResponse {
  countries: GeoCountryReference[];
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
    /** Só em `plugin_notice` (aviso de plugin para a rede). */
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
  /** Só em `plugin_notice`. */
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
  /** This machine's admission request on that network, when it made one. */
  admission?: {
    key: string;
    name: string;
    role: EnrollmentRole;
    status: "pending" | "approved" | "rejected";
    /** Approved without having asked now: this machine was admitted before. */
    standing?: boolean;
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
  /** Avisos de plugins recebidos nas últimas 24 h; só em memória no nó. */
  notices?: NetworkChatEvent[];
  dropped: number;
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
  /** In-flight requests as reported by the peer itself; `null` means it was not readable. */
  load: number | null;
  is_self: boolean;
  node: NodeInfo | null;
  unreachable: string | null;
  /**
   * Todo endereço conhecido deste par está desacreditado e o nó já pediu à
   * rede o endereço atual dele.
   *
   * Não é o mesmo que `unreachable`: aqui há trabalho em curso. Endereços IPv6
   * temporários rodam sozinhos, e a tela precisa dizer "reencontrando" em vez
   * de anunciar um beco sem saída que se resolve na volta seguinte do gossip.
   */
  rediscovering: boolean;
}

/** Quantos bytes de um grupo de tensores ficaram num buffer. */
export interface BufferBytes {
  buffer: string;
  bytes: number;
}

/**
 * Onde o llama.cpp pôs cada camada, medido depois da carga.
 *
 * `device` é o dispositivo que o llama.cpp atribuiu ao bloco; `buffers` é onde
 * os pesos dele estão de fato. Os dois divergem quando um override de buffer
 * type entra - o caso clássico é o `fit` mandando os experts de um MoE para a
 * CPU numa camada cujo dispositivo continua sendo a GPU. Por isso a tela lê os
 * buffers, não o dispositivo.
 */
export interface LayerPlacement {
  layer: number;
  device: string;
  buffers: BufferBytes[];
}

export interface ModelPlacement {
  n_layer: number;
  /** O que o llama.cpp acabou usando, não o que foi pedido. */
  n_gpu_layers: number;
  has_tensor_overrides: boolean;
  devices: Array<{ name: string; description: string }>;
  layers: LayerPlacement[];
  /** Embeddings, norma final e cabeça: tudo que não pertence a um bloco. */
  output: { device: string; buffers: BufferBytes[] };
  totals: BufferBytes[];
}

/**
 * A memória do contexto por buffer. `context` é o cache KV - a resposta real
 * para "o KV ficou na GPU?", que a flag `offload_kqv` só pede.
 */
export interface ContextMemoryBuffer {
  buffer: string;
  device: string;
  model: number;
  context: number;
  compute: number;
}

/**
 * Os parâmetros de llama.cpp com que o contexto de um nó subiu.
 *
 * Vem do contexto vivo, não do `EngineConfig` do nó: o fallback de KV em RAM
 * reconstrói o contexto com outra configuração, e o operador precisa ver o que
 * está rodando, não o que foi pedido.
 */
export interface PeerLoadParams {
  n_ctx: number;
  n_ctx_seq: number;
  n_batch: number;
  n_ubatch: number;
  n_seq_max: number;
  n_threads: number;
  n_threads_batch: number;
  /** `null` quando a ponte de diagnóstico não respondeu. */
  flash_attn: boolean | null;
  /** O Flash Attention veio da sonda `AUTO` ou de um pedido explícito? */
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
 * Um serviço que um plugin de uma máquina oferece ao cluster (`host.services`),
 * como o Qdrant servindo de banco de RAG. O nó valida e publica; a tela só
 * desenha.
 */
export interface PluginService {
  plugin_code: string;
  id: string;
  /** Para que serve: `rag`. Vocabulário aberto; um tipo desconhecido aparece como veio. */
  kind: string;
  name: string;
  status: "ready" | "starting" | "installing" | "waiting" | "error";
  version?: string;
  detail?: string;
}

/** O diretório de snapshots de KV de uma máquina, medido pelo próprio nó. */
export interface KvCacheDisk {
  /** `false` quando `slot_save_path` está desligado naquela máquina. */
  enabled: boolean;
  path: string | null;
  snapshots: number;
  bytes: number;
}

/** O cache de KV em RAM (`--cache-ram`) do servidor de inferência. */
export interface KvCacheRam {
  enabled: boolean;
  /** `null` com o cache ligado: máquina com menos de 12 GiB de RAM, ou ainda não medida. */
  limit_bytes: number | null;
  user_reserve_bytes: number | null;
  bytes: number;
  entries: number;
  tokens: number;
  parked: number;
  restored: number;
  evicted: number;
}

/** O que `POST /api/ui/kv-cache/clear` devolve. */
export interface KvCacheCleared {
  cleared: boolean;
  node_id: string;
  disk_files: number;
  disk_bytes: number;
  ram_entries: number;
  ram_bytes: number;
}

/** O `/health` do servidor de inferência de uma máquina, como a tela o lê. */
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
  /** Caminho selecionado pelo GGUF; o motor por requisição vem da trace. */
  selected_path?: string;
  /** Ownership efetivo dos experts neste nó, separado do caminho selecionado. */
  moe_requested?: boolean;
  moe_executor?: "cpu-reference" | "cuda" | "not-installed" | "not-requested" | "not-run" | "mixed";
  moe_executor_dtypes?: number[];
  /** Razão concreta quando o executor MoE não foi instalado ou usa CPU. */
  moe_executor_reason?: string | null;
  /** Cache de KV em RAM deste nó. Ausente num nó antigo. */
  cache_ram?: KvCacheRam;
  mtp_ready: boolean;
  /** Motivo retornado pelo runtime quando o MTP pedido não foi anexado. */
  mtp_unavailable_reason?: string | null;
  /** Local dominante dos buffers do contexto MTP pronto. */
  mtp_allocation?: "vram" | "ram" | null;
  /** Com que parâmetros de llama.cpp este contexto subiu. */
  load_params?: PeerLoadParams;
  /** `null` num nó cuja ponte de diagnóstico não respondeu. */
  placement: ModelPlacement | null;
  context_memory: { buffers: ContextMemoryBuffer[] } | null;
}

/** Resposta de `/api/ui/peer/:node_id/runtime`. */
export interface PeerRuntime {
  loaded: boolean;
  /** Por que não há dados, quando `loaded` é falso. */
  reason?: string;
  health?: PeerRuntimeHealth;
}

/** RTT direcional medido entre dois peers da execução exibida no chat. */
export interface ChatTelemetryRtt {
  from_node_id: string;
  to_node_id: string;
  rtt_ms: number;
  transport: string;
  /** A aresta passa por um circuito num relé libp2p: rota a rever. */
  relayed?: boolean;
}

export interface ChatTelemetryNetwork {
  measurements: ChatTelemetryRtt[];
  /** Custo observado do ciclo completo, quando o trace fechou todas as pernas. */
  pipeline_cost?: {
    legs_ms: Array<number | null>;
    compute_ms: number | null;
  } | null;
  /** Todas as rotas completas possíveis para o modelo ativo, do head à cauda. */
  pipeline_routes?: Array<{
    node_ids: string[];
    legs_ms: Array<number | null>;
    rtt_ms: number | null;
    /** Compute de um passo de decode somado nos peers da rota. */
    compute_ms?: number | null;
    /** Rede paga por token no modo de volta escolhido (não a soma das pernas). */
    network_ms?: number | null;
    return_mode?: "direct" | "ring" | null;
    /** Um token pela rota: rede no modo escolhido mais compute. */
    total_ms?: number | null;
    tokens_per_sec?: number | null;
    /** Alguma perna da rota passa por relé. */
    relayed?: boolean;
    /** Quais pernas passam por relé, como `[de, para]`. */
    relayed_legs?: Array<[string, string]>;
    /** A volta direta passaria por relé e o anel a contorna. */
    relay_bypassed_by_ring?: boolean;
  }>;
  /** Velocidade medida de cada peer do modelo ativo. */
  peer_perf?: Record<string, ChatTelemetryPeerPerf>;
}

/** Velocidade medida de um peer: teste ao servir e tráfego real. */
export interface ChatTelemetryPeerPerf {
  prefill_tokens_per_sec: number | null;
  decode_tokens_per_sec: number | null;
  active_generations: number;
}

/** Resposta do recorte de topologia que um nó infer pode consultar. */
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
   * Máquinas que contribuem camadas para esta cobertura e não puderam ser
   * lidas nesta passagem.
   *
   * A cobertura sai do que os pares anunciam no gossip, não da leitura de
   * cada um. Sem esta lista, um par ilegível conta como saudável e a tela diz
   * "completo" para um modelo cuja metade ninguém consegue consultar.
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
  /** Projetor carregado na cabeça da implantação ativa, quando há visão. */
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
  /** Diagnóstico da travessia de NAT deste nó. Ausente em nó antigo. */
  traversal?: TraversalView;
}

/**
 * Em que degrau a travessia de NAT parou. Sem isto, "os dois nós não se
 * falam" só se investiga com log de depuração na máquina certa.
 */
export interface TraversalView {
  nat: "unknown" | "public" | "private";
  /** Multiaddresses publicados, inclusive o circuito de relé. */
  addrs: string[];
  /** PeerId dos relés que aceitaram reserva. */
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
  /** GGUF mmproj adjacente escolhido pela convenção do llama.cpp. */
  mmproj_path?: string | null;
  /** Todos os projetores compatíveis para escolha na implantação. */
  mmproj_candidates?: string[];
  /** Motivo quando o catálogo não conseguiu sugerir um projetor compatível. */
  mmproj_reason?: string | null;
  size_bytes: number;
  /** O GGUF declara split.count > 1 e este item representa o conjunto. */
  multipart?: boolean;
  /** Partes físicas do conjunto, na ordem split.no. */
  multipart_parts?: MultipartPart[];
  /** Motivo concreto que impede ler um conjunto multipart incompleto. */
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
  /** Chave da rodada de fatiamento: `<modelo>-<id curto>`. */
  shard_group: string | null;
  /** sha256 da parte, quando o arquivo é endereçado por conteúdo. */
  sha256: string | null;
  /** Rótulo do modelo de origem, do `.pom`. */
  model_label: string | null;
  /** A rodada que produziu esta parte. */
  pom_id: string | null;
  /** Rank tensorial, quando a parte pertence a uma rodada TP. */
  tp_rank?: number | null;
  /** Tamanho do grupo tensorial, quando a parte pertence a uma rodada TP. */
  tp_size?: number | null;
}

/** A whole `.gguf`, as opposed to a slice of one. Slices carry `shard_range`. */
export const isWholeModel = (m: DiscoveredModel): boolean => m.shard_range == null;

/** `src/models/deployment.rs` — de que forma um modelo está no ar. */
export type DeploymentKind = 'shards' | 'whole' | 'tensor_parallel' | 'mixed';

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
 * `src/models/deployment.rs` — uma configuração que este `deploy_id` já rodou.
 *
 * O que distingue uma versão da outra são os parâmetros do deployment inteiro
 * (`params`). Ajustes por máquina entram no snapshot `nodes` mas não forkam
 * uma versão.
 */
export interface DeploymentVersion {
  version: number;
  created_at: string;
  created_by: string;
  params: RuntimeParams;
  nodes: DeploymentNode[];
}

/**
 * `src/models/deployment.rs` — a ligação entre um conjunto de nós e um modelo.
 *
 * O mesmo modelo pode estar no ar em mais de uma implantação, com máquinas ou
 * configuração diferentes; o `deploy_id` é o que as distingue. Uma requisição
 * escolhe uma pelo sufixo `<modelo>@<deploy_id>`.
 *
 * `versions` é o histórico de configurações do mesmo `deploy_id`;
 * `active_version` aponta a que está no ar. `nodes` é a lista achatada da
 * versão ativa.
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

/** O que `GET /api/ui/deployments` responde. */
export interface DeploymentsResponse {
  node_id: string;
  deployments: Deployment[];
  /** Os `deploy_id` que atendem um pedido feito só com o nome do modelo. */
  active: string[];
}

export interface ShardSet {
  /** Rótulo do modelo — o que se lê. `group` identifica a rodada. */
  model_label: string;
  /** Projector adjacent to this set or to its original whole model. */
  mmproj_path?: string | null;
  /** Todos os projetores compatíveis para escolha na implantação. */
  mmproj_candidates?: string[];
  /** Motivo quando o catálogo não conseguiu sugerir um projetor compatível. */
  mmproj_reason?: string | null;
  /** A rodada de fatiamento que produziu estas partes. Identificação, não nome. */
  pom_id: string;
  group: string;
  total_layers: number | null;
  total_layers_inferred: boolean;
  shards: DiscoveredModel[];
  /** Partes completas do modelo, uma por rank TP. */
  tp_parts?: DiscoveredModel[];
  complete: boolean;
}

/** Lê a lista TP de respostas de catálogo sem derrubar a tela se ela faltar. */
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
  /** Variável de ambiente que define as pastas de modelos inteiros, quando vale. */
  model_dirs_env?: string | null;
  /** Variável de ambiente que define a pasta das fatias, quando vale. */
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

/** Inventário da máquina escolhida no diálogo de adicionar. */
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

/** A slicing or apply run. `src/webui.rs` — `Job`. */
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
  /** Campos presentes quando `kind === "model_download"`. */
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

/** Estado seguro do token: o segredo bruto nunca faz parte do contrato da UI. */
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
  /**
   * De que modelo esta transferência é, e que camadas ela traz — do `.pom`.
   *
   * O `name` de uma parte é o sha256 dela, e hash não identifica nada que se
   * leia. Ausentes quando o arquivo não é parte de fatiamento nenhum (um
   * modelo inteiro) ou quando o nó é de versão anterior.
   */
  model_label?: string | null;
  start_layer?: number | null;
  end_layer?: number | null;
  /**
   * Quando o job entrou num estado terminal, em RFC 3339. Ausente enquanto
   * corre, e também num nó de versão anterior, que não carimbava a hora.
   */
  finished_at?: string | null;
}

export interface TransferOffer {
  /** Tipo explícito no contrato novo; ausente apenas quando o servidor ainda
   * não anota ofertas de fatia, que continuam sendo o caso padrão. */
  kind?: "slice";
  /** Resultado terminal, quando o servidor mantém o desfecho no sino. */
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
   * Sempre vazio. Quem confere o arquivo é este nó, depois de aceitar, com o
   * digest que ele pede em `/api/transfer/manifest`; calculá-lo para oferecer
   * segurava a notificação pelo tempo de ler o arquivo inteiro.
   */
  md5: string;
  start_layer: number | null;
  end_layer: number | null;
  /**
   * O manifesto do fatiamento a que esta parte pertence, quando ela é uma.
   *
   * Viaja **junto com a oferta**, e não depois: é ele que diz de que modelo a
   * parte é. Sem isto a notificação só tinha o `<sha256>.gguf` para mostrar.
   */
  pom?: OfferManifest | null;
}

/** Oferta do mesmo sino que transfere o papel de principal. */
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

/** Pedido de um nó infer para entrar no processamento da rede. */
export interface NodePromotionOffer {
  kind: "node_promotion";
  id: string;
  from_node_id: string;
  from_host: string;
  target_node_id: string;
  target_host: string;
  status: "pending" | "accepted" | "rejected";
  issued_at: string;
}

/** Aviso informativo emitido quando outro nó desce para infer. */
export interface NodeRelinquishNotice {
  kind: "node_relinquish_notice";
  id: string;
  from_node_id: string;
  from_host: string;
  target_node_id: string;
  target_host: string;
  status: "notice";
  issued_at: string;
  active_model?: string | null;
}

/** Todos os pedidos que podem aparecer no sino de atividade. */
export type NetworkOffer =
  | TransferOffer
  | AdminTransferOffer
  | NodePromotionOffer
  | NodeRelinquishNotice;

/** `crate::pom::PomManifest`, na parte que a tela usa. */
export interface OfferManifest {
  pom_id: string;
  model: {
    name: string;
    label: string;
    total_layers: number;
    quant?: string | null;
  };
}

/** Uma conexão libp2p viva com o peer. */
export interface PeerRouteLink {
  address: string;
  relayed: boolean;
  relay_peer_id?: string | null;
  hole_punched: boolean;
}

/**
 * Por onde este nó fala de fato com o peer, observado - não o endereço que o
 * peer anuncia. `http`: um endereço direto respondeu; `p2p_direct`: conexão
 * libp2p direta (discada ou por hole punch); `relayed`: só pelo relé.
 * `relay_kind` diz, em `relayed`, se o relé é dedicado (configurado) ou um
 * membro do cluster servindo de relé - o último degrau da escada.
 */
export interface PeerRoute {
  kind: "http" | "p2p_direct" | "relayed" | "none";
  address?: string | null;
  relay_kind?: "dedicated" | "member" | null;
  hole_punched: boolean;
  links: PeerRouteLink[];
}

export interface AdminNode {
  node_id: string;
  short_id: string;
  hostname: string;
  /** O que o peer anuncia; a rota em uso está em `route`. */
  addr: string;
  route?: PeerRoute | null;
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

export type AdminAuthorityViewer = "principal" | "secondary" | "node";

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

/** Contrato de apresentação. Não carrega a escolha de chave, assinatura ou armazenamento. */
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
 * A raiz de confiança desta máquina.
 *
 * Contrato de apresentação, como o de autoridade: a impressão digital é
 * pública e existe para o operador a conferir a olho; a decisão de instalar
 * é do servidor, e `can_install` só diz se vale a pena desenhar o formulário.
 */
export interface TrustAnchorView {
  anchored: boolean;
  fingerprint: string | null;
  cluster_id: string | null;
  anchor_node_id: string | null;
  /** A raiz é a chave deste nó, e não uma que ele recebeu. */
  is_anchor: boolean;
  /** Esta ligação chegou pelo loopback, medido no socket real. */
  loopback: boolean;
  can_install: boolean;
  trust_filename: string;
}

/** Códigos de recusa do servidor ao instalar a âncora, traduzidos na tela. */
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

/** Linha do log de processo persistida pelo nó principal. */
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

/** A cut accepted by the model's real GGUF layout. */
export interface LayerCut {
  end_layer: number;
  layers: number;
  size_bytes: number;
  remainder_bytes: number;
}

/** Cortes de camada aceitos pelo GGUF para uma fatia independente. */
export interface SliceLayerOptions {
  total_layers: number;
  valid_starts: number[];
  cuts: LayerCut[];
  granularity: number | null;
  rule: string;
}

/** `src/plan.rs` — `SizePlan`, what `POST /api/ui/slice/plan` answers. */
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

/** Um rank de um plano de fatiamento tensorial. */
export interface TpSliceRank {
  rank: number;
  start_layer: number;
  end_layer: number;
}

/** `src/backend/webui.rs` - plano de fatiamento Tensor Parallelism. */
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
  weight_bytes: number;
  kv_bytes: number;
  total_bytes: number;
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
  /** Faixa efetivamente executada; ausente em respostas antigas. */
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
 *  `src/backend/webui.rs` — `GpuCapacityProjection`. */
export interface GpuCapacityProjection {
  index: number;
  vram_total_mb: number;
  /** Measured now, before this deployment. */
  vram_used_mb: number;
  /** `vram_used_mb` plus the share of this step/machine that would land here. */
  vram_projected_mb: number;
}

/** A single deploy step's capacity projection. `src/backend/webui.rs` —
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
 *  take. `src/backend/deploy.rs` — `MachineCapacity`. */
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
  /** Métricas live da visão do cluster, ausentes em respostas antigas. */
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
 * Execution settings a launch carries. `src/webui.rs` — `RuntimeParams`.
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
  /** Formato negociado para o estado oculto entre peers; ausente em config antiga = f16. */
  hidden_state_dtype?: WireDtype;
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
  /** Permite o recuo do MTP para pesos e cache em RAM quando a VRAM falhar. */
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

/** Precisões que o protocolo de inferência consegue converter ponta a ponta. */
export type WireDtype = "f32" | "f16" | "f8" | "i4g64" | "i4g128";

export type CapabilityState = "supported" | "unsupported" | "unknown";

export interface RuntimeCapability {
  state: CapabilityState;
  source: string;
  reason: string;
  /** Limitação de execução que não prova ausência no modelo selecionado. */
  warning?: string;
}

export interface DeploymentCapabilities {
  fit_memory: RuntimeCapability;
  no_thinking: RuntimeCapability;
  no_embeddings: RuntimeCapability;
  speculative_decoding: RuntimeCapability;
  mtp: RuntimeCapability;
  /** Capacidade de todos os peers selecionados de receber estado oculto f8. */
  hidden_state_f8?: RuntimeCapability;
  /** Capacidade de todos os peers selecionados de receber estado oculto i4g64. */
  hidden_state_i4g64?: RuntimeCapability;
  /** Capacidade de todos os peers selecionados de receber estado oculto i4g128. */
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
  /** Projector adjacente encontrado para o modelo escolhido. */
  mmproj_path?: string | null;
  /** Todos os projetores compatíveis no peer da cabeça. */
  mmproj_candidates?: string[];
  /** Motivo quando o catálogo não conseguiu sugerir um projetor compatível. */
  mmproj_reason?: string | null;
  /** True only when the surveyed model's actual GGUF metadata is MoE. */
  is_moe?: boolean;
  /** Whether this plan can honor the selected inference mode. */
  inference_mode_supported?: boolean;
  /** Why the selected inference mode cannot be honored, when applicable. */
  inference_mode_reason?: string | null;
  /** Capacidades condicionais apuradas pelo backend para os alvos atuais. */
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
