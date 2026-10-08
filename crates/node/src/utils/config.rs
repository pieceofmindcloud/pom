// src/config.rs
use anyhow::{Context, Result};
use argon2::password_hash::PasswordHash;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Testes que mudam `POM_CONFIG` ou `POM_HOME` compartilham o ambiente do
/// processo. A trava mantém esses testes assíncronos determinísticos quando a
/// suíte roda em paralelo.
#[cfg(test)]
pub static TEST_ENVIRONMENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Nome do arquivo de config: derivado da marca (`brand.json`), não escrito à
/// mão, para renomear o produto ser uma edição só.
fn config_filename() -> String {
    crate::brand::config_filename()
}

/// Arquivo separado que guarda a âncora e a cadeia de concessões de admin.
/// Nunca é derivado de `Config.profile`: editar `pom.json` não pode criar
/// autoridade.
pub fn admin_trust_filename() -> String {
    format!(".{}-admin-trust.json", crate::brand::slug())
}

pub fn admin_trust_path() -> PathBuf {
    crate::brand::env("ADMIN_TRUST")
        .map(PathBuf::from)
        .unwrap_or_else(|_| project_root().join(admin_trust_filename()))
}

/// Superfície administrativa do gateway. As rotas da pipeline não usam esta
/// política: continuam ouvindo em IPv4 e IPv6 para que o túnel libp2p tenha um
/// caminho HTTP de reserva.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdminExposure {
    Localhost,
    Network,
}

impl Default for AdminExposure {
    fn default() -> Self {
        Self::Localhost
    }
}

impl AdminExposure {
    pub fn requires_password(self) -> bool {
        matches!(self, Self::Network)
    }
}

/// Visibility chosen during first access. It describes the operator's intent
/// for the network and does not replace the transport or administrator trust
/// policy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkVisibility {
    Local,
    Private,
    Public,
}

impl Default for NetworkVisibility {
    fn default() -> Self {
        Self::Local
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkAccessPolicy {
    Authorized,
    AnyoneWithAnchor,
    LocalOnly,
}

impl Default for NetworkAccessPolicy {
    fn default() -> Self {
        Self::Authorized
    }
}

/// A role a new machine can enroll with. `admin` is absent on purpose: it is
/// never an enrollment, administrative authority comes from the grant chain.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentRole {
    Node,
    Infer,
}

/// The enrollment roles a network currently advertises as open. Empty means
/// the network is not accepting new members. The default is every role, so a
/// config written before the field existed stays open.
///
/// This is an advertisement read by the public directory, not admission:
/// `Network::public_peer_is_admitted` does not consult it, and members that
/// already joined are never affected by a change here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct AcceptedEnrollments(pub Vec<EnrollmentRole>);

impl Default for AcceptedEnrollments {
    fn default() -> Self {
        Self(vec![EnrollmentRole::Node, EnrollmentRole::Infer])
    }
}

impl AcceptedEnrollments {
    /// Sorted and without repeats, so equal sets serialize (and sign) equally.
    pub fn normalized(mut self) -> Self {
        self.0.sort();
        self.0.dedup();
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetworkLocation {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub accuracy_m: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NetworkSettings {
    #[serde(default)]
    pub public_name: String,
    #[serde(default)]
    pub organization: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub rules: String,
    #[serde(default)]
    pub channels: String,
    /// Reference-country code selected in the first-access location step.
    #[serde(default)]
    pub country_code: String,
    /// Human-readable country name from the local reference table.
    #[serde(default)]
    pub country_name: String,
    /// Reference-table city or an operator-entered manual city.
    #[serde(default)]
    pub city: String,
    /// Public discovery index label. This is metadata in the pkarr record, not
    /// the pkarr public key itself.
    #[serde(default)]
    pub geo_label: String,
    #[serde(default)]
    pub visibility: NetworkVisibility,
    #[serde(default)]
    pub access_policy: NetworkAccessPolicy,
    /// Which enrollment roles the network advertises as open right now.
    #[serde(default)]
    pub accepted_enrollments: AcceptedEnrollments,
    /// Opens the `lobby` room of the network Chat to machines that are not
    /// members, so an admin can talk to a candidate before admitting it
    /// (`network/network_lobby.rs`). Only takes effect on a public network
    /// with the network Chat on.
    #[serde(default)]
    pub lobby_chat: bool,
    /// Publishes the served models, the node count and the total VRAM to
    /// machines that are not members, answered live by the principal
    /// (`network_lobby::PublicStats`). Only takes effect on a public network.
    #[serde(default)]
    pub public_stats: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<NetworkLocation>,
}

impl NetworkSettings {
    /// Whether membership is open to anyone holding the trust document, with
    /// no connection key: a public network left on `anyone_with_anchor`.
    ///
    /// Every other network is gated by the connection key, which an admin
    /// hands over by approving an admission request (`network_lobby.rs`). The
    /// open mode is what networks created before admission requests use; the
    /// interface no longer offers it.
    pub fn open_admission(&self) -> bool {
        self.visibility == NetworkVisibility::Public
            && self.access_policy == NetworkAccessPolicy::AnyoneWithAnchor
    }
}

/// Validate the network block wherever it is edited, not only during setup.
/// Public networks need the coarse geographic key because discovery groups by
/// country and city before measuring peer latency.
pub fn validate_network_settings(settings: &NetworkSettings) -> Result<()> {
    if settings.visibility == NetworkVisibility::Public {
        if settings.public_name.trim().is_empty() {
            anyhow::bail!("redes públicas precisam de um nome público");
        }
        if settings.country_code.trim().is_empty() || settings.city.trim().is_empty() {
            anyhow::bail!("redes públicas precisam de país e cidade");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResourcePolicy {
    /// Percentage of memory kept available for the operating system.
    #[serde(default = "default_resource_reserve_pct")]
    pub ram_reserve_pct: f64,
    /// Percentage of CPU capacity kept available for the operating system.
    #[serde(default = "default_resource_reserve_pct")]
    pub cpu_reserve_pct: f64,
}

impl Default for ResourcePolicy {
    fn default() -> Self {
        Self {
            ram_reserve_pct: default_resource_reserve_pct(),
            cpu_reserve_pct: default_resource_reserve_pct(),
        }
    }
}

/// Tipos de dados aceitos pelo cache KV do llama.cpp desta instalação.
///
/// A definição mudou para `llama-engine` (`kv_cache.rs`) porque o lado FFI
/// precisa dela; aqui é só um campo de `Config`. Re-exportada para que
/// `crate::config::KvCacheType` continue valendo.
pub use llama_engine::KvCacheType;

/// Older `pom.json` files carried API keys with their secrets in plain text.
/// The database owns the keys now, so whatever the file holds is read past and
/// dropped instead of loaded (or refused for the missing `key_hash`).
fn discard_legacy_api_keys<'de, D>(deserializer: D) -> Result<Vec<ApiKey>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::de::IgnoredAny::deserialize(deserializer)?;
    Ok(Vec::new())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    /// Tracking id: the stable public identity the audit trail and the logs
    /// plugin record for every request. Random (UUID v4), so it is unique
    /// across every node of the network. Never a credential.
    #[serde(default)]
    pub id: String,
    /// SHA-256 (hex) of the secret. The secret itself is shown once, when the
    /// key is minted, and is never stored or sent between nodes: inference
    /// validates a presented token by hashing it (`auth::api_key_hash`).
    pub key_hash: String,
    /// The start and the end of the secret (`sk-pom-3f9a…c41e`), enough for a
    /// person or another node to tell keys apart without being able to use one.
    #[serde(default)]
    pub key_hint: String,
    pub label: String,
    pub node_id: String,
    /// Node that minted this key. It remains stable when the key is
    /// synchronized to another node or assigned to a consumer node.
    #[serde(default)]
    pub origin_node_id: String,
    #[serde(default)]
    pub active: bool,
    pub created_at: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Product profile owned by this node. It is not selected by the browser
    /// and is intentionally required in the new schema. Administrative
    /// capability comes only from the separate signed grant chain.
    pub profile: crate::profile::NodeProfile,
    /// This file is a local preference shell waiting for first access. It is
    /// set when leaving a network so setup can preserve machine preferences
    /// without booting the old network.
    #[serde(default)]
    pub setup_pending: bool,
    /// Responder de bootstrap local. O wizard usa um listener transitório;
    /// depois do setup esta preferência continua desligada até o admin ligar.
    #[serde(default)]
    pub local_auto_discovery: bool,
    #[serde(default)]
    pub connection_key: String,
    #[serde(default = "default_gateway_port")]
    pub gateway_port: u16,
    /// Onde as rotas administrativas podem ser usadas. Ausente significa
    /// localhost, inclusive em configurações gravadas antes deste campo.
    #[serde(default)]
    pub admin_exposure: AdminExposure,
    /// Hash PHC Argon2id da senha da pessoa que abre a superfície administrativa.
    /// O nome serializado permanece por compatibilidade com configurações
    /// antigas, cujo token literal é migrado no primeiro `Config::load`.
    /// Não é a `connection_key`, que continua autenticando nós e pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_access_token: Option<String>,
    /// A senha administrativa ainda é a padrão do primeiro acesso
    /// (`auth::DEFAULT_ADMIN_PASSWORD`). Enquanto for, a interface entra sem
    /// perguntá-la e pede para trocar. É uma marca gravada junto do hash para a
    /// rota pública `/api/access` não precisar rodar Argon2 a cada visita.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub admin_password_default: bool,
    #[serde(default = "default_p2p_port")]
    pub p2p_port: u16,
    #[serde(default)]
    pub inference_port: Option<u16>,
    #[serde(default)]
    pub bootstrap_nodes: Vec<String>,
    /// Chaves pkarr (`pk:...`) de relés para a travessia de NAT, somadas às de
    /// `--relay-peers`. Resolvidas uma vez no arranque; só entra o registro
    /// que se declara relé. Um relé que este nó já alcança como bootstrap não
    /// precisa estar aqui (ver `relay_reservation_addrs` em `discovery.rs`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relay_peers: Vec<String>,
    /// Address other nodes should keep for this one, e.g.
    /// `"[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777"` or just the bare IPv6.
    ///
    /// Empty means "let each peer read it off the packet", which is right on a
    /// LAN. On the public internet it is not: with privacy extensions the source
    /// address is a temporary IPv6 that rotates, so peers end up holding an
    /// address that has stopped answering. See `network::advertised_addr`.
    #[serde(default)]
    pub advertise_addr: Option<String>,
    /// Human-facing network settings captured during first access.
    #[serde(default)]
    pub network: NetworkSettings,
    /// In-memory snapshot of `pom.db`'s `api_keys`, which owns them. Never
    /// written to `pom.json`: the file is versioned in the cluster config repo.
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "discard_legacy_api_keys"
    )]
    pub api_keys: Vec<ApiKey>,
    #[serde(default)]
    pub blocked_nodes: Vec<String>,
    /// Todo HTTP entre peers vai por TLS quando isto é `true`.
    ///
    /// A decisão é do nó principal e vale para o cluster inteiro; os demais nós
    /// gravam aqui o que buscaram dele por canal autenticado. **Nunca grave
    /// isto a partir do gossip**: o UDP de descoberta é em claro e não
    /// autenticado, e um anúncio forjado rebaixaria a rede para texto claro,
    /// que é exatamente o ataque que o TLS existe para impedir. Ver
    /// `tls_policy`.
    #[serde(default)]
    pub tls_required: bool,
    #[serde(default)]
    pub quic_enabled: bool,
    /// Coleta de logs de processo para o nó principal. A decisão é do
    /// principal e os demais nós só adotam a resposta autenticada da catraca.
    #[serde(default)]
    pub cluster_logs_enabled: bool,
    /// Canal global de conversa, decidido pelo principal e confirmado pelos
    /// demais nós via leitura autenticada.
    #[serde(default)]
    pub network_chat_enabled: bool,
    /// Nível mínimo capturado no histórico de logs do cluster. O filtro é
    /// local à captura e não altera o nível do console ou de outros sinks.
    #[serde(default)]
    pub cluster_logs_min_level: crate::cluster_logs::ClusterLogLevel,
    /// Directories holding **whole** models.
    ///
    /// `None` — the field missing from an old `pom.json`, or never edited —
    /// means "whatever LM Studio and Ollama have on this machine", exactly what
    /// the scan did before it was configurable. Once the operator saves the list
    /// it becomes law, and an empty list really means "look nowhere": the
    /// captain moving every GGUF to `/Volumes/sdcard/models` must not keep
    /// getting the old locations back.
    #[serde(default)]
    pub model_dirs: Option<Vec<String>>,
    /// Where slices produced on this machine are written and looked for.
    /// `None` keeps the historical `<project_root>/models`.
    #[serde(default)]
    pub shard_dir: Option<String>,
    /// One shared, persistent root for user projects and dynamic plugin data.
    /// The effective default is `~/pom_workspace`; model directories
    /// remain separate from this workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    /// Maximum disk space reserved for generated model slices, in GB.
    #[serde(default = "default_shard_max_gb")]
    pub shard_max_gb: f64,
    /// Local resource reservation selected during first access.
    #[serde(default)]
    pub resources: ResourcePolicy,
    /// Optional model selected by the first-access catalog. Loading remains a
    /// later deployment action and is deliberately not started by setup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_model: Option<String>,
    /// Quais GPUs desta máquina a inferência pode usar, pelo `index` que o nó
    /// publica em `system.gpus[]` - o mesmo número que o painel mostra.
    ///
    /// Três estados, e os três querem dizer coisas diferentes:
    ///
    /// - **ausente** (`None`): todas as placas disponíveis. É o padrão, e é o
    ///   que uma `pom.json` antiga significa sem ter sido editada.
    /// - **lista com itens**: só essas. Numa máquina com Radeon e NVIDIA, é
    ///   assim que se reserva uma placa para outro uso.
    /// - **lista vazia**: nenhuma - a inferência fica em CPU. Vazio é uma
    ///   escolha explícita, não "não configurado", mesma regra do `model_dirs`.
    ///
    /// O índice é o do nó, não o do backend: `gpu_selection` traduz para o
    /// `CUDA_VISIBLE_DEVICES` e o `GGML_VK_VISIBLE_DEVICES` que cada backend
    /// entende, que numeram as placas em espaços separados.
    #[serde(default)]
    pub gpus: Option<Vec<u32>>,
    /// Política local de proteção térmica da GPU.
    #[serde(default)]
    pub thermal: crate::thermal::ThermalPolicy,
    /// Tipo numérico do cache K usado por todos os contextos desta instalação.
    #[serde(default)]
    pub cache_type_k: KvCacheType,
    /// Tipo numérico do cache V usado por todos os contextos desta instalação.
    #[serde(default)]
    pub cache_type_v: KvCacheType,
    /// Human-readable name for this node shown in the dashboard and peer lists.
    /// Falls back to the OS hostname when absent.
    #[serde(default)]
    pub node_name: Option<String>,
    /// Token opcional salvo pela tela de Configurações. O provider nunca o
    /// devolve pela API: a interface recebe apenas estado e máscara.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub huggingface_token: Option<String>,
    /// Directory where KV cache snapshots are written and read, like
    /// llama-server's `--slot-save-path`. A machine property, not a
    /// deployment one: each peer writes its own slice here. The
    /// `POM_SLOT_SAVE_PATH` environment variable takes precedence over this
    /// field - use `slot_save_path_effective()`, never the raw field.
    /// Absent in every source means the default `~/.pom/kv-slots`
    /// (`default_slot_save_dir`); an empty value turns the feature off and the
    /// snapshot routes answer "not configured".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_save_path: Option<String>,
    /// Turns on automatic persistence of the prefix cache: at the end of each
    /// turn the gateway writes the retained KV to `slot_save_path` and, on a
    /// miss after a restart, restores it instead of redoing a cold prefill.
    /// Requires `slot_save_path` on every hop. On by default (a `pom.json`
    /// without the field gets `true`); `false` here or
    /// `POM_SLOT_PERSIST_PREFIX_CACHE=0` turns it off. Use
    /// `slot_persist_prefix_cache_enabled()`.
    #[serde(default = "default_slot_persist_prefix_cache")]
    pub slot_persist_prefix_cache: bool,
    /// Orçamento de disco para os snapshots automáticos de prefixo, somado
    /// entre as fatias. `None` usa `DEFAULT_SLOT_AUTO_MAX_BYTES` (2 GiB). Use
    /// `slot_auto_max_bytes()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_auto_max_bytes: Option<u64>,
    /// TTL de um snapshot automático de prefixo sem uso, em segundos. `None`
    /// usa `DEFAULT_SLOT_AUTO_TTL_SECS` (7 dias). A env `POM_SLOT_AUTO_TTL_SECS`
    /// vence o campo. Use `slot_auto_ttl_secs()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_auto_ttl_secs: Option<u64>,
    /// Host-memory KV cache for retained sessions that lose their sequence to
    /// another conversation - llama-server's `--cache-ram`, in MiB: a positive
    /// ceiling, `0` for off, absent or `-1` for as much as the machine allows.
    /// Whatever the value, half of the host RAM stays available to the user
    /// and a host whose half is under 6 GiB caches nothing
    /// (`infer_server/ram_cache.rs`). A machine property; the `POM_CACHE_RAM`
    /// environment variable takes precedence. Use `cache_ram_mib_effective()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ram_mib: Option<i64>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("profile", &self.profile)
            .field(
                "connection_key",
                if self.connection_key.is_empty() {
                    &"" as &dyn fmt::Debug
                } else {
                    &"[redacted]" as &dyn fmt::Debug
                },
            )
            .field("gateway_port", &self.gateway_port)
            .field("admin_exposure", &self.admin_exposure)
            .field(
                "admin_access_token",
                &self.admin_access_token.as_ref().map(|_| "[redacted]"),
            )
            .field("p2p_port", &self.p2p_port)
            .field("inference_port", &self.inference_port)
            .field("bootstrap_nodes", &self.bootstrap_nodes)
            .field("advertise_addr", &self.advertise_addr)
            .field("network", &self.network)
            .field("api_key_count", &self.api_keys.len())
            .field("blocked_nodes", &self.blocked_nodes)
            .field("tls_required", &self.tls_required)
            .field("cluster_logs_enabled", &self.cluster_logs_enabled)
            .field("network_chat_enabled", &self.network_chat_enabled)
            .field("cluster_logs_min_level", &self.cluster_logs_min_level)
            .field("model_dirs", &self.model_dirs)
            .field("shard_dir", &self.shard_dir)
            .field("workspace_root", &self.workspace_root)
            .field("shard_max_gb", &self.shard_max_gb)
            .field("resources", &self.resources)
            .field("setup_model", &self.setup_model)
            .field("gpus", &self.gpus)
            .field("thermal", &self.thermal)
            .field("cache_type_k", &self.cache_type_k)
            .field("cache_type_v", &self.cache_type_v)
            .field("node_name", &self.node_name)
            .field(
                "huggingface_token",
                &self.huggingface_token.as_ref().map(|_| "[redacted]"),
            )
            .field("slot_save_path", &self.slot_save_path)
            .field("slot_persist_prefix_cache", &self.slot_persist_prefix_cache)
            .field("slot_auto_max_bytes", &self.slot_auto_max_bytes)
            .field("slot_auto_ttl_secs", &self.slot_auto_ttl_secs)
            .field("cache_ram_mib", &self.cache_ram_mib)
            .finish()
    }
}

fn default_gateway_port() -> u16 {
    8080
}

pub fn validate_gateway_port(port: u16) -> anyhow::Result<()> {
    anyhow::ensure!(port != 0, "a porta da interface deve estar entre 1 e 65535");
    Ok(())
}
fn default_p2p_port() -> u16 {
    42777
}

fn default_shard_max_gb() -> f64 {
    crate::storage::DEFAULT_SHARD_MAX_GB
}

fn default_resource_reserve_pct() -> f64 {
    20.0
}

/// Put a peer address the operator typed into the form the resolver accepts.
///
/// The failure this exists for is an address with **no port**: the resolver
/// splits on the last colon, so a bare IPv6 hands it a group of hex digits
/// where a port belongs and answers `invalid port value`. The gossip loop then
/// repeats `Failed to send to …: invalid port value` forever, and the message
/// names neither the offending entry nor the fix — it comes from a layer below
/// anything the operator ever typed. So a bare address gets the default P2P
/// port, which is the only port it could have meant.
///
/// Missing brackets are handled too, though measurement says they are *not* the
/// culprit: `2804:d55::1:42777` resolves fine, because the split leaves a valid
/// IPv6 on the left. Normalising it anyway keeps what we store equal to what we
/// print, which is what makes a peer list comparable by eye.
pub fn normalize_peer_addr(raw: &str) -> String {
    use std::net::{IpAddr, SocketAddr};

    let s = raw.trim();
    if s.is_empty() {
        return String::new();
    }
    // Already valid — `[v6]:port`, `v4:port`.
    if let Ok(socket) = s.parse::<SocketAddr>() {
        return if socket.port() == 0 {
            String::new()
        } else {
            s.to_string()
        };
    }
    // An address and nothing else: supply the port it must have meant.
    if let Ok(ip) = s.parse::<IpAddr>() {
        return SocketAddr::new(ip, default_p2p_port()).to_string();
    }
    if s.rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        == Some(0)
    {
        return String::new();
    }
    // Unbracketed IPv6 with a port: resolvable as typed, stored canonically.
    if let Some((host, port)) = s.rsplit_once(':') {
        if host.parse::<std::net::Ipv6Addr>().is_ok() && port.parse::<u16>().is_ok() {
            return format!("[{host}]:{port}");
        }
    }
    // Hostnames resolve at send time; leave anything else untouched rather than
    // rejecting a name we simply do not understand here.
    s.to_string()
}

/// Pastas de modelos inteiros vindas de `POM_MODEL_DIRS`, separadas como o
/// `PATH` do sistema (`:` no Unix, `;` no Windows). Vence `model_dirs` do
/// `pom.json` e, como toda variável de ambiente, não é persistida: `save()`
/// continua gravando só o campo do arquivo, e tirar a variável devolve o que
/// estava salvo. Vazia conta como ausente.
pub fn model_dirs_from_env() -> Option<Vec<PathBuf>> {
    parse_model_dirs(&std::env::var_os(crate::brand::env_var("MODEL_DIRS"))?)
}

/// Pasta das fatias vinda de `POM_SHARD_DIR`, com as mesmas regras de
/// `model_dirs_from_env`.
pub fn shard_dir_from_env() -> Option<PathBuf> {
    parse_shard_dir(&std::env::var_os(crate::brand::env_var("SHARD_DIR"))?)
}

fn parse_model_dirs(raw: &std::ffi::OsStr) -> Option<Vec<PathBuf>> {
    let dirs: Vec<PathBuf> = std::env::split_paths(raw)
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect();
    (!dirs.is_empty()).then_some(dirs)
}

fn parse_shard_dir(raw: &std::ffi::OsStr) -> Option<PathBuf> {
    let trimmed = raw.to_string_lossy().trim().to_string();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

/// Port the local inference server listens on when the config does not name one.
///
/// Every node advertises it from startup, before any slice is assigned:
/// without a port `apply` refused every model and the UI could configure
/// nothing at all. The server is spawned when a slice is actually assigned.
pub const DEFAULT_INFERENCE_PORT: u16 = 8181;

/// Directory `pom.json`, `models/` and `venv/` are resolved against.
///
/// Everything used to be resolved against the working directory, so launching
/// the binary by absolute path from somewhere else (`ssh host '/path/to/pom'`
/// runs in `$HOME`) silently read a different config and found no models at all —
/// the node then reported it had none of the shards sitting right next to it.
///
/// Resolution order, first hit wins:
///  1. `$POM_HOME`, for anyone who wants to be explicit;
///  2. the working directory, when it actually looks like the project;
///  3. the directory holding the executable, climbing out of `target/release`.
pub fn project_root() -> PathBuf {
    if let Ok(home) = crate::brand::env("HOME") {
        return PathBuf::from(home);
    }

    let looks_like_project =
        |d: &std::path::Path| d.join(config_filename()).exists() || d.join("models").is_dir();

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if looks_like_project(&cwd) {
        return cwd;
    }

    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(PathBuf::from);
        // …/target/release/pom → climb to the project directory.
        for _ in 0..3 {
            let Some(d) = dir else { break };
            if looks_like_project(&d) {
                return d;
            }
            dir = d.parent().map(PathBuf::from);
        }
    }

    cwd
}

/// Directory that contains the runtime SQLite database.
///
/// `POM_DB_DIR` is intended for a Docker volume. Without it the database is
/// kept beside `pom.json`, as before this setting existed.
pub fn database_dir() -> PathBuf {
    let configured = crate::brand::env("DB_DIR").ok();
    database_dir_for(project_root(), configured.as_deref())
}

fn database_dir_for(root: PathBuf, configured: Option<&str>) -> PathBuf {
    configured
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or(root)
}

pub fn database_path() -> PathBuf {
    database_dir().join(crate::db::DATABASE_FILENAME)
}

impl Config {
    pub fn config_path() -> PathBuf {
        crate::brand::env("CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| project_root().join(config_filename()))
    }

    pub fn exists() -> bool {
        Self::config_path().exists()
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        Self::load_from(&path)
    }

    fn load_from(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config at {:?}", path))?;
        let mut config = parse_config(&content, &path)?;
        let (admin_access_token, migrated) =
            migrate_admin_access_token(config.admin_access_token.clone())?;
        if migrated {
            config.admin_access_token = admin_access_token;
            config.write_to(&path)?;
        }
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        self.write_to(&path)
    }

    fn write_to(&self, path: &Path) -> Result<()> {
        let mut value = serde_json::to_value(self)?;
        // Keep the file safe even if a caller hands this writer a legacy
        // literal. Normal startup migrates before this point, but enforcing
        // the invariant at the serialization boundary prevents a plaintext
        // administrative credential from being written by any future path.
        if self.admin_access_token.is_some() {
            let (admin_access_token, _) =
                migrate_admin_access_token(self.admin_access_token.clone())?;
            value["admin_access_token"] = serde_json::to_value(admin_access_token)?;
        }
        if path.exists() {
            let old = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read existing config at {:?}", path))?;
            let old: serde_json::Value = serde_json::from_str(&old)
                .with_context(|| format!("Failed to parse existing config at {:?}", path))?;
            // These fields are now owned by pom.db. Preserve their old values
            // when another setting updates pom.json, so the legacy source stays
            // a usable safety copy until the operator explicitly migrates it.
            // `api_keys` is deliberately absent: older files carried the
            // secrets in plain text, and rewriting the file drops them.
            for field in ["thermal", "huggingface_token"] {
                if let Some(old_value) = old.get(field) {
                    value[field] = old_value.clone();
                }
            }
        }
        let content = serde_json::to_string_pretty(&value)?;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .with_context(|| format!("Failed to write config at {:?}", path))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("Failed to restrict config at {:?}", path))?;
        }
        file.write_all(content.as_bytes())
            .with_context(|| format!("Failed to write config at {:?}", path))?;
        file.flush()
            .with_context(|| format!("Failed to flush config at {:?}", path))
    }

    /// Turn a configured node into the local shell used by first access after
    /// it leaves a network. Keep only preferences that belong to this machine;
    /// every network identity or credential must be supplied again.
    pub fn prepare_for_setup(&mut self) {
        self.setup_pending = true;
        self.profile = crate::profile::NodeProfile::Node;
        self.connection_key.clear();
        self.bootstrap_nodes.clear();
        self.advertise_addr = None;
        self.network = NetworkSettings::default();
        self.api_keys.clear();
        self.blocked_nodes.clear();
        self.admin_exposure = AdminExposure::Localhost;
        self.admin_access_token = None;
        self.tls_required = false;
        self.cluster_logs_enabled = false;
        self.setup_model = None;
    }

    /// Inference port this node should use. Without one configured it follows
    /// the gateway port (8080 -> 8181, as before): nodes that share a host
    /// network, each with its own gateway port, then each bind their own
    /// inference port instead of all taking 8181 - with two of them serving a
    /// model, the second one could not start ("Address already in use").
    pub fn inference_port_or_default(&self) -> u16 {
        self.inference_port.unwrap_or_else(|| {
            self.gateway_port
                .checked_add(DEFAULT_INFERENCE_PORT - default_gateway_port())
                .unwrap_or(DEFAULT_INFERENCE_PORT)
        })
    }

    pub fn active_api_key(&self) -> Option<&ApiKey> {
        self.api_keys.iter().find(|k| k.active)
    }

    /// Directories to scan for whole models, falling back to what LM Studio and
    /// Ollama put on this machine while the operator has not chosen.
    ///
    /// `POM_MODEL_DIRS` (derivada da marca) vence o `pom.json`; ver
    /// `model_dirs_from_env`.
    pub fn model_dir_paths(&self) -> Vec<PathBuf> {
        if let Some(dirs) = model_dirs_from_env() {
            return dirs;
        }
        match &self.model_dirs {
            Some(dirs) => dirs.iter().map(PathBuf::from).collect(),
            None => crate::catalog::default_model_dirs(),
        }
    }

    /// True while the whole-model list is still the auto-detected one — the UI
    /// says so instead of pretending the operator picked these paths.
    pub fn model_dirs_are_default(&self) -> bool {
        self.model_dirs.is_none() && model_dirs_from_env().is_none()
    }

    /// Destination of local slicing, and the only directory a peer may pull
    /// from (see `transfer::safe_model_path`).
    ///
    /// `POM_SHARD_DIR` (derivada da marca) vence o `pom.json`; ver
    /// `shard_dir_from_env`.
    pub fn shard_dir_path(&self) -> PathBuf {
        if let Some(dir) = shard_dir_from_env() {
            return dir;
        }
        match self.shard_dir.as_deref().map(str::trim) {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => default_shard_dir(),
        }
    }

    /// Shared root for user projects and dynamic plugin state. Relative values
    /// from a hand-edited legacy config are resolved under the project root so
    /// the host contract always receives an absolute path.
    /// Creates the storage folders that are POM's own defaults when they are
    /// the effective ones: `~/.pom/models` (when it is among the model
    /// folders), `~/.pom/models_sliced` and `~/pom_workspace`.
    ///
    /// Setup and the local-network bootstrap save these paths without
    /// creating them, so a fresh install could not receive a file: the
    /// destination did not exist. A folder the operator typed is never
    /// created here: it may sit on a drive that is not mounted right now, and
    /// creating it would write onto the wrong disk. Returns the folders that
    /// could not be created, for the caller to report.
    pub fn create_default_storage_dirs(&self) -> Vec<(PathBuf, std::io::Error)> {
        self.create_storage_dirs_for_defaults(
            &default_model_dir(),
            &default_shard_dir(),
            &default_workspace_root(),
        )
    }

    /// `create_default_storage_dirs` with the defaults given, so a test can
    /// point them at a temporary folder instead of the user's home.
    fn create_storage_dirs_for_defaults(
        &self,
        default_models: &Path,
        default_shards: &Path,
        default_workspace: &Path,
    ) -> Vec<(PathBuf, std::io::Error)> {
        let mut wanted = Vec::new();
        if self.model_dir_paths().iter().any(|dir| dir == default_models) {
            wanted.push(default_models.to_path_buf());
        }
        let shard_dir = self.shard_dir_path();
        if shard_dir == default_shards {
            wanted.push(shard_dir);
        }
        let workspace = self.workspace_root_path();
        if workspace == default_workspace {
            wanted.push(workspace);
        }
        wanted
            .into_iter()
            .filter(|dir| !dir.is_dir())
            .filter_map(|dir| std::fs::create_dir_all(&dir).err().map(|error| (dir, error)))
            .collect()
    }

    pub fn workspace_root_path(&self) -> PathBuf {
        let path = match self.workspace_root.as_deref().map(str::trim) {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => default_workspace_root(),
        };
        if path.is_absolute() {
            path
        } else {
            project_root().join(path)
        }
    }

    pub fn shard_max_gb(&self) -> f64 {
        if self.shard_max_gb.is_finite() && self.shard_max_gb > 0.0 {
            self.shard_max_gb
        } else {
            default_shard_max_gb()
        }
    }

    pub fn shard_max_bytes(&self) -> u64 {
        (self.shard_max_gb() * 1_073_741_824.0).round() as u64
    }

    /// Effective KV cache snapshot directory, or `None` when the feature is
    /// off. Precedence: the `POM_SLOT_SAVE_PATH` environment variable (derived
    /// from the brand, `brand::env("SLOT_SAVE_PATH")`) beats the `pom.json`
    /// field, and with neither set the default is `~/.pom/kv-slots`
    /// (`default_slot_save_dir`). An empty value in the variable or the field
    /// turns the feature off. Since the env var is per process, it is **not**
    /// persisted: `save()` keeps writing only the file's field.
    pub fn slot_save_path_effective(&self) -> Option<String> {
        match crate::brand::env("SLOT_SAVE_PATH")
            .ok()
            .or_else(|| self.slot_save_path.clone())
        {
            Some(value) => Some(value.trim().to_string()).filter(|value| !value.is_empty()),
            None => Some(default_slot_save_dir().to_string_lossy().into_owned()),
        }
    }

    pub fn slot_save_dir(&self) -> Option<PathBuf> {
        self.slot_save_path_effective().map(PathBuf::from)
    }

    /// Persistência automática do cache de prefixo ligada? A env var
    /// `POM_SLOT_PERSIST_PREFIX_CACHE` (`1`/`true`/`yes`/`on`) vence o campo do
    /// `pom.json`.
    pub fn slot_persist_prefix_cache_enabled(&self) -> bool {
        match crate::brand::env("SLOT_PERSIST_PREFIX_CACHE")
            .ok()
            .as_deref()
        {
            Some(v) => matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ),
            None => self.slot_persist_prefix_cache,
        }
    }

    /// Caminho do índice de snapshots automáticos de prefixo (fica ao lado dos
    /// `.bin`, no `slot_save_path` deste nó). `None` se `slot_save_path` não
    /// está configurado.
    pub fn prefix_auto_index_path(&self) -> Option<PathBuf> {
        self.slot_save_dir()
            .map(|dir| dir.join("prefix-auto-index.json"))
    }

    pub fn slot_auto_max_bytes(&self) -> u64 {
        self.slot_auto_max_bytes
            .filter(|n| *n > 0)
            .unwrap_or(crate::prefix_cache::DEFAULT_SLOT_AUTO_MAX_BYTES)
    }

    /// Host-memory KV cache setting in MiB (`--cache-ram`); `POM_CACHE_RAM`
    /// beats the `pom.json` field. `None` means neither source set it.
    pub fn cache_ram_mib_effective(&self) -> Option<i64> {
        crate::brand::env("CACHE_RAM")
            .ok()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .or(self.cache_ram_mib)
    }

    /// TTL efetivo dos snapshots automáticos de prefixo, em segundos.
    /// `POM_SLOT_AUTO_TTL_SECS` vence o campo do `pom.json`.
    pub fn slot_auto_ttl_secs(&self) -> i64 {
        crate::brand::env("SLOT_AUTO_TTL_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .or_else(|| self.slot_auto_ttl_secs.map(|n| n as i64))
            .filter(|n| *n > 0)
            .unwrap_or(crate::prefix_cache::DEFAULT_SLOT_AUTO_TTL_SECS)
    }
}

/// Stable geo-hierarchical metadata used to narrow public-network discovery.
/// It deliberately never becomes the pkarr key: the key remains the signed
/// node identity, while this label is an indexable field in its record.
pub fn geo_network_label(country_code: &str, city: &str, node_id: &str) -> String {
    let country = country_code.trim().to_ascii_lowercase();
    let city = geo_slug(city);
    let node = geo_slug(node_id);
    if country.is_empty() || city.is_empty() || node.is_empty() {
        return String::new();
    }
    format!("com.{country}.{city}.{node}")
}

fn geo_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in value.trim().chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator {
                slug.push('-');
            }
            slug.push(character);
            separator = false;
        } else if (character.is_whitespace() || character == '-' || character == '_')
            && !slug.is_empty()
        {
            separator = true;
        }
    }
    slug
}

/// `<project_root>/models` - where every slice has always landed.
pub fn default_shard_dir() -> PathBuf {
    crate::catalog::home_dir()
        .unwrap_or_else(project_root)
        .join(".pom")
        .join("models_sliced")
}

/// Persistent root for user projects and dynamic plugin state. Model weights
/// and runtime/install files deliberately live elsewhere. The first-access
/// default follows the user's home rather than the project root so the setup
/// wizard can remain focused on node identity and model storage.
pub fn default_workspace_root() -> PathBuf {
    crate::catalog::home_dir()
        .unwrap_or_else(project_root)
        .join("pom_workspace")
}

/// `~/.pom/kv-slots` is where KV cache snapshots (manual and the persistent
/// prefix cache) go when `slot_save_path` is not configured.
pub fn default_slot_save_dir() -> PathBuf {
    crate::catalog::home_dir()
        .unwrap_or_else(project_root)
        .join(".pom")
        .join("kv-slots")
}

fn default_slot_persist_prefix_cache() -> bool {
    true
}

/// `~/.pom/models` is the first-access destination for whole models.
pub fn default_model_dir() -> PathBuf {
    crate::catalog::home_dir()
        .unwrap_or_else(project_root)
        .join(".pom")
        .join("models")
}

/// Where the built web interface (`apps/frontend/dist`) is read from at
/// runtime. `$POM_UI_DIST` overrides it — the runtime Docker image sets
/// it explicitly rather than relying on `project_root()` detection, and a dev
/// setup that keeps the bundle elsewhere can point straight at it.
pub fn ui_dist_dir() -> PathBuf {
    if let Ok(dir) = crate::brand::env("UI_DIST") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = bundled_ui_dist() {
        return dir;
    }
    project_root().join("apps/frontend/dist")
}

/// A interface dentro de um bundle `.app` do macOS.
///
/// O bundle é o único jeito de um executável ter ícone no macOS
/// (`scripts/make-macos-app.sh`), e nele o binário vive em
/// `POM.app/Contents/MacOS/`. Dali, `project_root()` cai no cwd de quem
/// abriu - o Finder abre em `/`, e a tela responderia 500. A interface viaja
/// em `Contents/Resources/apps/frontend/dist`, que é o que esta função acha.
///
/// Vale em qualquer plataforma porque é só um teste de caminho; fora de um
/// bundle o diretório não existe e a função devolve `None`.
fn bundled_ui_dist() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let macos_dir = exe.parent()?;
    if macos_dir.file_name()? != "MacOS" {
        return None;
    }
    let dir = macos_dir
        .parent()?
        .join("Resources")
        .join("apps/frontend/dist");
    dir.is_dir().then_some(dir)
}

fn parse_config(content: &str, path: &Path) -> Result<Config> {
    serde_json::from_str(content).map_err(|error| {
        let role_hint = serde_json::from_str::<serde_json::Value>(content)
            .ok()
            .filter(|value| value.get("role").is_some())
            .map(|_| {
                ". O campo `role` foi removido da configuração; remova-o e use `profile` (`admin`, `node` ou `infer`)"
            })
            .unwrap_or("");
        anyhow::anyhow!("Failed to parse config at {:?}: {error}{role_hint}", path)
    })
}

/// Convert the pre-password credential on first load. Older configurations
/// stored the literal administrative token in this field; current
/// configurations store only an Argon2id PHC string. A missing value remains
/// missing, so a fresh local-only setup still has no password.
fn migrate_admin_access_token(value: Option<String>) -> Result<(Option<String>, bool)> {
    let Some(value) = value else {
        return Ok((None, false));
    };
    if value.is_empty() {
        return Ok((None, true));
    }
    if let Ok(parsed) = PasswordHash::new(&value) {
        if parsed.algorithm.as_str() == "argon2id"
            && parsed.version.is_some()
            && !parsed.params.is_empty()
            && parsed.salt.is_some()
            && parsed.hash.is_some()
        {
            return Ok((Some(value), false));
        }
    }
    crate::auth::hash_admin_password(&value).map(|hash| (Some(hash), true))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_default_inference_port_follows_the_gateway_port() {
        let mut config: Config =
            serde_json::from_value(serde_json::json!({"profile": "admin", "connection_key": "k", "gateway_port": 8080})).unwrap();
        assert_eq!(config.inference_port_or_default(), 8181, "the single-node default is unchanged");
        config.gateway_port = 8082;
        assert_eq!(config.inference_port_or_default(), 8183);
        config.inference_port = Some(9000);
        assert_eq!(config.inference_port_or_default(), 9000, "a configured port wins");
    }

    /// A peer address with no port is what produces `invalid port value` — the
    /// resolver splits on the last colon and finds hex where a port belongs.
    /// Measured, not assumed: an unbracketed IPv6 *with* a port resolves fine.
    #[test]
    fn peer_addr_supplies_the_missing_port() {
        assert_eq!(
            normalize_peer_addr("2804:d55:5579:1000:1c20:e551:6a95:9a0d:42777"),
            "[2804:d55:5579:1000:1c20:e551:6a95:9a0d]:42777"
        );
        // Already correct: untouched.
        assert_eq!(
            normalize_peer_addr(" [2804:d55::1]:42777 "),
            "[2804:d55::1]:42777"
        );
        // No port at all: the default P2P port is the only one it could mean.
        assert_eq!(normalize_peer_addr("2804:d55::1"), "[2804:d55::1]:42777");
        assert_eq!(normalize_peer_addr("192.168.1.5"), "192.168.1.5:42777");
        // IPv4 with port, and a hostname, both pass through.
        assert_eq!(
            normalize_peer_addr("192.168.1.5:42777"),
            "192.168.1.5:42777"
        );
        assert_eq!(normalize_peer_addr("ksia:42777"), "ksia:42777");
        assert_eq!(normalize_peer_addr("192.168.1.5:0"), "");
        assert_eq!(normalize_peer_addr("peer.example:0"), "");
        assert_eq!(normalize_peer_addr("  "), "");
    }

    use super::*;

    #[test]
    fn migrates_the_legacy_admin_token_into_a_password_hash() {
        let (migrated, changed) = migrate_admin_access_token(Some("legacy-admin-token".into()))
            .expect("migrar o token legado");

        let migrated = migrated.expect("hash migrado");
        assert!(changed);
        assert!(migrated.starts_with("$argon2id$"));
        assert!(crate::auth::verify_admin_password(
            &migrated,
            "legacy-admin-token"
        ));
        assert!(!migrated.contains("legacy-admin-token"));
    }

    #[test]
    fn invalid_argon2id_prefix_is_migrated_as_a_legacy_token() {
        let legacy = "$argon2id$not-a-complete-phc";
        let (migrated, changed) = migrate_admin_access_token(Some(legacy.into()))
            .expect("migrar o token legado com prefixo ambíguo");

        let migrated = migrated.expect("hash migrado");
        assert!(changed);
        assert!(crate::auth::verify_admin_password(&migrated, legacy));
        assert!(!migrated.contains(legacy));
    }

    #[test]
    fn config_load_migrates_legacy_admin_token_on_disk() {
        let legacy = "legacy-admin-token-loaded-from-disk";
        let path = std::env::temp_dir().join(format!(
            "pom-config-admin-migration-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("relógio do sistema")
                .as_nanos()
        ));
        std::fs::write(
            &path,
            serde_json::json!({
                "profile": "admin",
                "admin_access_token": legacy,
            })
            .to_string(),
        )
        .expect("escrever configuração legada");

        let config = Config::load_from(&path).expect("carregar configuração legada");
        let hash = config
            .admin_access_token
            .as_deref()
            .expect("token legado migrado");
        assert!(crate::auth::verify_admin_password(hash, legacy));
        assert!(hash.starts_with("$argon2id$"));

        let persisted = std::fs::read_to_string(&path).expect("ler configuração migrada");
        assert!(!persisted.contains(legacy));
        assert!(persisted.contains("$argon2id$"));

        std::fs::remove_file(path).expect("remover configuração temporária");
    }

    #[test]
    fn absent_admin_token_stays_without_a_password() {
        let (migrated, changed) = migrate_admin_access_token(None).expect("ler ausência");
        assert!(migrated.is_none());
        assert!(!changed);
    }

    #[test]
    fn database_directory_uses_volume_override_and_project_fallback() {
        let root = PathBuf::from("/project");
        assert_eq!(
            database_dir_for(root.clone(), Some("/volume/pom")),
            PathBuf::from("/volume/pom")
        );
        assert_eq!(database_dir_for(root.clone(), Some("  ")), root.clone());
        assert_eq!(database_dir_for(root.clone(), None), root);
    }

    #[test]
    fn test_roundtrip_principal_config() {
        let cfg = Config {
            gpus: None,
            thermal: Default::default(),
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            profile: crate::profile::NodeProfile::Admin,
            setup_pending: false,
            local_auto_discovery: false,
            tls_required: false,
            quic_enabled: false,
            cluster_logs_enabled: false,
            network_chat_enabled: false,
            cluster_logs_min_level: Default::default(),
            connection_key: "test-key".into(),
            gateway_port: 8080,
            admin_exposure: AdminExposure::Localhost,
            admin_access_token: None,
            admin_password_default: false,
            p2p_port: 42777,
            inference_port: Some(8181),
            bootstrap_nodes: vec![],
            relay_peers: vec![],
            advertise_addr: None,
            network: Default::default(),
            api_keys: vec![],
            blocked_nodes: vec![],
            model_dirs: None,
            shard_dir: None,
            workspace_root: None,
            shard_max_gb: default_shard_max_gb(),
            resources: Default::default(),
            setup_model: None,
            node_name: None,
            huggingface_token: None,
            slot_save_path: None,
            slot_persist_prefix_cache: false,
            slot_auto_max_bytes: None,
            slot_auto_ttl_secs: None,
            cache_ram_mib: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.profile, crate::profile::NodeProfile::Admin);
        assert_eq!(parsed.connection_key, "test-key");
    }

    /// Configurações legadas não podem reintroduzir uma capacidade absoluta
    /// silenciosamente.
    #[test]
    fn removed_output_capacity_field_is_rejected_explicitly() {
        let legacy_field = ["max", "output", "tokens"].join("_");
        let mut value = serde_json::json!({"profile": "admin", "connection_key": "k"});
        value
            .as_object_mut()
            .unwrap()
            .insert(legacy_field.clone(), serde_json::json!(32000));
        let result: Result<Config, _> = serde_json::from_value(value);
        let error = result.expect_err("campo removido não pode ser aceito silenciosamente");
        assert!(error.to_string().contains(&legacy_field));
    }

    #[test]
    fn reference_config_has_no_absolute_output_capacity_field() {
        // Keep this fixture in the test source. `pom.json` is runtime state and
        // intentionally ignored by Git, so a compile must never depend on it.
        let cfg: Config = serde_json::from_str(
            r#"{"profile":"admin","connection_key":"reference-test","gateway_port":8080,"p2p_port":42777,"bootstrap_nodes":[],"api_keys":[],"blocked_nodes":[]}"#,
        )
        .unwrap();
        let value = serde_json::to_value(cfg).unwrap();
        assert!(!value
            .as_object()
            .unwrap()
            .contains_key(&["max", "output", "tokens"].join("_")));
    }

    #[test]
    fn nivel_minimo_dos_logs_tem_default_info_e_sobrevive_ao_json() {
        let defaulted: Config = serde_json::from_str(r#"{"profile":"node"}"#).unwrap();
        assert_eq!(
            defaulted.cluster_logs_min_level,
            crate::cluster_logs::ClusterLogLevel::Info
        );

        let configured: Config =
            serde_json::from_str(r#"{"profile":"node","cluster_logs_min_level":"debug"}"#).unwrap();
        assert_eq!(
            configured.cluster_logs_min_level,
            crate::cluster_logs::ClusterLogLevel::Debug
        );
        assert_eq!(
            serde_json::to_value(configured)
                .unwrap()
                .get("cluster_logs_min_level")
                .unwrap(),
            "debug"
        );
    }

    #[test]
    fn descoberta_local_fica_desligada_por_padrao() {
        let config: Config = serde_json::from_str(r#"{"profile":"admin"}"#).unwrap();
        assert!(!config.local_auto_discovery);
    }

    #[test]
    fn test_roundtrip_node_config() {
        let cfg = Config {
            gpus: None,
            thermal: Default::default(),
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            profile: crate::profile::NodeProfile::Node,
            setup_pending: false,
            local_auto_discovery: false,
            tls_required: false,
            quic_enabled: false,
            cluster_logs_enabled: false,
            network_chat_enabled: false,
            cluster_logs_min_level: Default::default(),
            connection_key: "node-key".into(),
            gateway_port: 8090,
            admin_exposure: AdminExposure::Localhost,
            admin_access_token: None,
            admin_password_default: false,
            p2p_port: 42777,
            inference_port: Some(8181),
            bootstrap_nodes: vec!["192.168.1.1:42777".into()],
            relay_peers: vec![],
            advertise_addr: None,
            network: Default::default(),
            api_keys: vec![],
            blocked_nodes: vec![],
            model_dirs: None,
            shard_dir: None,
            workspace_root: None,
            shard_max_gb: default_shard_max_gb(),
            resources: Default::default(),
            setup_model: None,
            node_name: None,
            huggingface_token: None,
            slot_save_path: None,
            slot_persist_prefix_cache: false,
            slot_auto_max_bytes: None,
            slot_auto_ttl_secs: None,
            cache_ram_mib: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.profile, crate::profile::NodeProfile::Node);
    }

    /// A config without the official profile is not a supported schema. The
    /// operator recreates it through setup instead of silently guessing.
    #[test]
    fn a_config_without_profile_requires_reinitialization() {
        let old = r#"{"connection_key":"k","gateway_port":8080,
                      "p2p_port":42777,"bootstrap_nodes":[],"api_keys":[],"blocked_nodes":[]}"#;
        assert!(serde_json::from_str::<Config>(old).is_err());
    }

    #[test]
    fn a_legacy_role_config_explains_the_profile_replacement() {
        let error = parse_config(
            r#"{"role":"principal","profile":"admin"}"#,
            Path::new("pom.json"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("campo `role` foi removido"));
        assert!(error.contains("use `profile`"));
    }

    #[test]
    fn model_dirs_from_env_split_like_path_and_ignore_empty_entries() {
        let joined = std::env::join_paths(["/data/models", "/mnt/gguf"]).unwrap();
        assert_eq!(
            parse_model_dirs(&joined),
            Some(vec![
                PathBuf::from("/data/models"),
                PathBuf::from("/mnt/gguf")
            ])
        );
        assert_eq!(parse_model_dirs(std::ffi::OsStr::new("")), None);
        assert_eq!(
            parse_shard_dir(std::ffi::OsStr::new("  /data/shards ")),
            Some(PathBuf::from("/data/shards"))
        );
        assert_eq!(parse_shard_dir(std::ffi::OsStr::new("  ")), None);
    }

    #[test]
    fn relay_peers_are_read_from_the_config_and_empty_stays_out_of_it() {
        let cfg: Config =
            serde_json::from_str(r#"{"profile":"node","relay_peers":["pk:relay"]}"#).unwrap();
        assert_eq!(cfg.relay_peers, vec!["pk:relay"]);

        let cfg: Config = serde_json::from_str(r#"{"profile":"node"}"#).unwrap();
        assert!(cfg.relay_peers.is_empty());
        let saved = serde_json::to_value(&cfg).unwrap();
        assert!(saved.get("relay_peers").is_none());
    }

    #[test]
    fn thermal_policy_is_configurable_and_survives_roundtrip() {
        let mut cfg: Config = serde_json::from_str(r#"{"profile":"node"}"#).unwrap();
        cfg.thermal = crate::thermal::ThermalPolicy {
            limit_c: 82.0,
            hysteresis_c: 7.0,
            cooldown_secs: 45,
        };
        let value = serde_json::to_value(&cfg).unwrap();
        assert_eq!(value["thermal"]["limit_c"], 82.0);
        let parsed: Config = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.thermal.limit_c, 82.0);
        assert_eq!(parsed.thermal.hysteresis_c, 7.0);
        assert_eq!(parsed.thermal.cooldown_secs, 45);
    }

    #[test]
    fn public_network_settings_require_country_and_city() {
        let settings = NetworkSettings {
            public_name: "Rede aberta".into(),
            visibility: NetworkVisibility::Public,
            ..Default::default()
        };

        let error = validate_network_settings(&settings)
            .expect_err("uma rede pública sem localização não deve ser salva");
        assert!(error.to_string().contains("país e cidade"));
    }

    #[test]
    fn accepted_enrollments_default_to_open_and_an_empty_list_closes_the_network() {
        let untouched: NetworkSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(
            untouched.accepted_enrollments.0,
            vec![EnrollmentRole::Node, EnrollmentRole::Infer]
        );

        let closed: NetworkSettings =
            serde_json::from_str(r#"{"accepted_enrollments":[]}"#).unwrap();
        assert!(closed.accepted_enrollments.0.is_empty());

        let only_infer = AcceptedEnrollments(vec![
            EnrollmentRole::Infer,
            EnrollmentRole::Infer,
        ])
        .normalized();
        assert_eq!(
            serde_json::to_value(&only_infer).unwrap(),
            serde_json::json!(["infer"])
        );
    }

    #[test]
    fn cache_types_are_persisted_as_installation_settings() {
        let cfg: Config = serde_json::from_str(
            r#"{"profile":"node","cache_type_k":"q8_0","cache_type_v":"bf16"}"#,
        )
        .unwrap();
        let json = serde_json::to_value(cfg).unwrap();

        assert_eq!(json["cache_type_k"], "q8_0");
        assert_eq!(json["cache_type_v"], "bf16");
    }

    #[test]
    fn invalid_cache_type_is_rejected() {
        let result: Result<Config, _> =
            serde_json::from_str(r#"{"profile":"node","cache_type_k":"not-a-type"}"#);

        assert!(result.is_err());
    }

    #[test]
    fn cache_type_defaults_to_f16_and_preserves_iq4_nl_spelling() {
        let defaulted: Config = serde_json::from_str(r#"{"profile":"node"}"#).unwrap();
        assert_eq!(defaulted.cache_type_k, KvCacheType::F16);
        assert_eq!(defaulted.cache_type_v, KvCacheType::F16);

        let typed: Config =
            serde_json::from_str(r#"{"profile":"node","cache_type_k":"iq4_nl"}"#).unwrap();
        assert_eq!(typed.cache_type_k, KvCacheType::Iq4Nl);
        assert_eq!(typed.cache_type_k.as_str(), "iq4_nl");
    }

    #[cfg(unix)]
    #[test]
    fn huggingface_token_config_is_saved_owner_only_and_debug_is_redacted() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "pom-config-hf-token-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut cfg = Config {
            profile: crate::profile::NodeProfile::Node,
            setup_pending: false,
            local_auto_discovery: false,
            connection_key: String::new(),
            gateway_port: 8080,
            admin_exposure: AdminExposure::Localhost,
            admin_access_token: None,
            admin_password_default: false,
            p2p_port: 42777,
            inference_port: None,
            bootstrap_nodes: vec![],
            relay_peers: vec![],
            advertise_addr: None,
            network: Default::default(),
            api_keys: vec![],
            blocked_nodes: vec![],
            tls_required: false,
            quic_enabled: false,
            cluster_logs_enabled: false,
            network_chat_enabled: false,
            cluster_logs_min_level: Default::default(),
            model_dirs: None,
            shard_dir: None,
            workspace_root: None,
            shard_max_gb: default_shard_max_gb(),
            resources: Default::default(),
            setup_model: None,
            gpus: None,
            thermal: Default::default(),
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            node_name: None,
            huggingface_token: Some("hf_super_secret".into()),
            slot_save_path: None,
            slot_persist_prefix_cache: false,
            slot_auto_max_bytes: None,
            slot_auto_ttl_secs: None,
            cache_ram_mib: None,
        };
        cfg.admin_exposure = AdminExposure::Network;
        let admin_password_hash = crate::auth::hash_admin_password("admin_super_secret").unwrap();
        cfg.admin_access_token = Some(admin_password_hash.clone());

        std::fs::write(
            &path,
            r#"{"api_keys":[],"thermal":{"limit_c":85.0,"hysteresis_c":8.0,"cooldown_secs":30},"huggingface_token":"hf_legacy"}"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        cfg.write_to(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(format!("{cfg:?}").contains("[redacted]"));
        assert!(!format!("{cfg:?}").contains("hf_super_secret"));
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["huggingface_token"], "hf_legacy");
        assert!(!format!("{cfg:?}").contains("admin_super_secret"));
        assert_eq!(saved["admin_access_token"], admin_password_hash);
        let _ = std::fs::remove_file(path);
    }

    /// Saving the list is the operator taking over: an empty list means "look
    /// nowhere", not "go back to guessing".
    #[test]
    fn an_explicit_empty_list_is_not_the_default_list() {
        let cfg: Config = serde_json::from_str(
            r#"{"profile":"node","model_dirs":[],"shard_dir":"/Volumes/sdcard/fatias"}"#,
        )
        .unwrap();
        assert!(!cfg.model_dirs_are_default());
        assert!(cfg.model_dir_paths().is_empty());
        assert_eq!(
            cfg.shard_dir_path(),
            PathBuf::from("/Volumes/sdcard/fatias")
        );
    }

    /// `pom.json` gravado antes do TLS não pode deixar de parsear - e o padrão
    /// tem que ser "sem TLS", senão atualizar o binário derruba o cluster.
    #[test]
    fn config_antiga_parseia_sem_tls_required() {
        let cfg: Config =
            serde_json::from_str(r#"{"profile":"node","connection_key":"k"}"#).unwrap();
        assert!(!cfg.tls_required);
    }

    #[test]
    fn gateway_port_zero_is_rejected_instead_of_using_an_ephemeral_port() {
        assert!(validate_gateway_port(0).is_err());
        assert!(validate_gateway_port(1).is_ok());
        assert!(validate_gateway_port(u16::MAX).is_ok());
    }

    #[test]
    fn tls_required_sobrevive_ao_roundtrip() {
        let cfg: Config =
            serde_json::from_str(r#"{"profile":"admin","tls_required":true}"#).unwrap();
        assert!(cfg.tls_required);
        let texto = serde_json::to_string(&cfg).unwrap();
        let volta: Config = serde_json::from_str(&texto).unwrap();
        assert!(volta.tls_required);
    }

    #[test]
    fn a_blank_shard_dir_falls_back_instead_of_writing_to_the_root() {
        let cfg: Config = serde_json::from_str(r#"{"profile":"node","shard_dir":"   "}"#).unwrap();
        assert_eq!(cfg.shard_dir_path(), default_shard_dir());
    }

    #[test]
    fn the_default_storage_folders_are_created_but_a_typed_one_is_not() {
        let root = std::env::temp_dir().join(format!("pom-storage-{}", uuid::Uuid::new_v4().simple()));
        let models = root.join(".pom").join("models");
        let shards = root.join(".pom").join("models_sliced");
        let workspace = root.join("pom_workspace");
        let typed = root.join("unmounted-drive").join("models");
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "profile": "node",
            "model_dirs": [models, typed],
            "shard_dir": shards,
            "workspace_root": workspace,
        }))
        .unwrap();

        assert!(cfg.create_storage_dirs_for_defaults(&models, &shards, &workspace).is_empty());
        assert!(models.is_dir() && shards.is_dir() && workspace.is_dir());
        assert!(!typed.exists());

        // A folder the operator chose instead of the default is left alone.
        let other = root.join("chosen-slices");
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "profile": "node",
            "shard_dir": other,
        }))
        .unwrap();
        cfg.create_storage_dirs_for_defaults(&models, &shards, &workspace);
        assert!(!other.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_root_defaults_to_the_user_home() {
        let cfg: Config = serde_json::from_str(r#"{"profile":"node"}"#).unwrap();
        assert_eq!(cfg.workspace_root_path(), default_workspace_root());
        assert!(cfg.workspace_root_path().ends_with("pom_workspace"));
        assert_ne!(cfg.workspace_root_path(), cfg.shard_dir_path());
        assert!(!cfg
            .model_dir_paths()
            .iter()
            .any(|path| path == &cfg.workspace_root_path()));
    }

    #[test]
    fn workspace_root_roundtrips_as_a_persisted_absolute_path() {
        let path = "/data/workspace";
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "profile": "node",
            "workspace_root": path,
        }))
        .unwrap();
        assert_eq!(cfg.workspace_root_path(), PathBuf::from(path));
        assert_eq!(
            serde_json::to_value(cfg)
                .unwrap()
                .get("workspace_root")
                .and_then(serde_json::Value::as_str),
            Some(path)
        );
    }

    #[test]
    fn setup_defaults_keep_models_and_slices_in_the_pom_home() {
        let model_dir = default_model_dir();
        let shard_dir = default_shard_dir();
        assert!(model_dir.ends_with(Path::new(".pom").join("models")));
        assert!(shard_dir.ends_with(Path::new(".pom").join("models_sliced")));
    }

    /// KV snapshots and the persistent prefix cache are on by default, in
    /// `~/.pom/kv-slots`; an empty `slot_save_path` or `false` turns them off.
    #[test]
    fn kv_slots_are_on_by_default_in_the_pom_home() {
        if crate::brand::env("SLOT_SAVE_PATH").is_ok()
            || crate::brand::env("SLOT_PERSIST_PREFIX_CACHE").is_ok()
        {
            return;
        }
        let defaulted: Config = serde_json::from_str(r#"{"profile":"node"}"#).unwrap();
        assert!(defaulted.slot_persist_prefix_cache_enabled());
        assert!(defaulted
            .slot_save_dir()
            .unwrap()
            .ends_with(Path::new(".pom").join("kv-slots")));

        let off: Config = serde_json::from_str(
            r#"{"profile":"node","slot_save_path":"","slot_persist_prefix_cache":false}"#,
        )
        .unwrap();
        assert_eq!(off.slot_save_dir(), None);
        assert!(!off.slot_persist_prefix_cache_enabled());
    }

    #[test]
    fn test_active_api_key() {
        let cfg = Config {
            gpus: None,
            thermal: Default::default(),
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            profile: crate::profile::NodeProfile::Admin,
            setup_pending: false,
            local_auto_discovery: false,
            tls_required: false,
            quic_enabled: false,
            cluster_logs_enabled: false,
            network_chat_enabled: false,
            cluster_logs_min_level: Default::default(),
            connection_key: "k".into(),
            gateway_port: 8080,
            admin_exposure: AdminExposure::Localhost,
            admin_access_token: None,
            admin_password_default: false,
            p2p_port: 42777,
            inference_port: None,
            bootstrap_nodes: vec![],
            relay_peers: vec![],
            advertise_addr: None,
            network: Default::default(),
            api_keys: vec![
                ApiKey {
                    id: "old-id".into(),
                    key_hash: "hash-old".into(),
                    key_hint: String::new(),
                    label: "old".into(),
                    node_id: "n1".into(),
                    origin_node_id: "n1".into(),
                    active: false,
                    created_at: "2026-01-01T00:00:00Z".into(),
                },
                ApiKey {
                    id: "active-id".into(),
                    key_hash: "hash-active".into(),
                    key_hint: String::new(),
                    label: "active".into(),
                    node_id: "n1".into(),
                    origin_node_id: "n1".into(),
                    active: true,
                    created_at: "2026-02-01T00:00:00Z".into(),
                },
            ],
            blocked_nodes: vec![],
            model_dirs: None,
            shard_dir: None,
            workspace_root: None,
            shard_max_gb: default_shard_max_gb(),
            resources: Default::default(),
            setup_model: None,
            node_name: None,
            huggingface_token: None,
            slot_save_path: None,
            slot_persist_prefix_cache: false,
            slot_auto_max_bytes: None,
            slot_auto_ttl_secs: None,
            cache_ram_mib: None,
        };
        assert_eq!(cfg.active_api_key().unwrap().id, "active-id");
    }

    #[test]
    fn sair_da_rede_preserva_preferencias_locais_e_limpa_credenciais() {
        let mut cfg: Config = serde_json::from_value(serde_json::json!({
            "profile": "admin",
            "connection_key": "segredo-da-rede",
            "bootstrap_nodes": ["[2001:db8::2]:42777"],
            "advertise_addr": "[2001:db8::1]:42777",
            "network": {
                "public_name": "Rede antiga",
                "organization": "Equipe antiga",
                "visibility": "private"
            },
            "api_keys": [{
                "id": "key-id",
                "key": "sk-old",
                "label": "Aplicativo",
                "node_id": "node",
                "origin_node_id": "node",
                "active": true,
                "created_at": "2026-09-09T00:00:00Z"
            }],
            "admin_exposure": "network",
            "admin_access_token": "admin-token",
            "tls_required": true,
            "model_dirs": ["/Volumes/models"],
            "shard_dir": "/Volumes/slices",
            "shard_max_gb": 120,
            "resources": {"ram_reserve_pct": 31, "cpu_reserve_pct": 42},
            "thermal": {"limit_c": 82, "hysteresis_c": 5, "cooldown_secs": 30},
            "node_name": "Mesa do Jair"
        }))
        .unwrap();

        cfg.prepare_for_setup();

        assert!(cfg.setup_pending);
        assert!(cfg.connection_key.is_empty());
        assert!(cfg.bootstrap_nodes.is_empty());
        assert!(cfg.advertise_addr.is_none());
        assert_eq!(cfg.network, NetworkSettings::default());
        assert!(cfg.api_keys.is_empty());
        assert!(cfg.admin_access_token.is_none());
        assert_eq!(cfg.admin_exposure, AdminExposure::Localhost);
        assert!(!cfg.tls_required);
        assert_eq!(cfg.model_dirs, Some(vec!["/Volumes/models".into()]));
        assert_eq!(cfg.shard_dir.as_deref(), Some("/Volumes/slices"));
        assert_eq!(cfg.shard_max_gb, 120.0);
        assert_eq!(cfg.resources.ram_reserve_pct, 31.0);
        assert_eq!(cfg.resources.cpu_reserve_pct, 42.0);
        assert_eq!(cfg.thermal.limit_c, 82.0);
        assert_eq!(cfg.node_name.as_deref(), Some("Mesa do Jair"));
    }

    #[test]
    fn rotulo_geo_separa_pais_cidade_e_identidade_do_no() {
        assert_eq!(
            geo_network_label("BR", "Curitiba", "abc123"),
            "com.br.curitiba.abc123"
        );
        assert_eq!(
            geo_network_label("AU", "Cidade Manual", "node-id"),
            "com.au.cidade-manual.node-id"
        );
    }
}
