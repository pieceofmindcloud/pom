// src/config.rs
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

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
    pub fn requires_token(self) -> bool {
        matches!(self, Self::Network)
    }
}

/// Tipos de dados aceitos pelo cache KV do llama.cpp desta instalação.
///
/// A definição mudou para `llama-engine` (`kv_cache.rs`) porque o lado FFI
/// precisa dela; aqui é só um campo de `Config`. Re-exportada para que
/// `crate::config::KvCacheType` continue valendo.
pub use llama_engine::KvCacheType;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    /// Stable public identity used by the audit trail. Never a credential.
    #[serde(default)]
    pub id: String,
    pub key: String,
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
    #[serde(default)]
    pub connection_key: String,
    #[serde(default = "default_gateway_port")]
    pub gateway_port: u16,
    /// Onde as rotas administrativas podem ser usadas. Ausente significa
    /// localhost, inclusive em configurações gravadas antes deste campo.
    #[serde(default)]
    pub admin_exposure: AdminExposure,
    /// Credencial exclusiva da pessoa que abre a superfície administrativa.
    /// Não é a `connection_key`, que continua autenticando nós e pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_access_token: Option<String>,
    #[serde(default = "default_p2p_port")]
    pub p2p_port: u16,
    #[serde(default)]
    pub inference_port: Option<u16>,
    #[serde(default)]
    pub bootstrap_nodes: Vec<String>,
    /// Address other nodes should keep for this one, e.g.
    /// `"[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777"` or just the bare IPv6.
    ///
    /// Empty means "let each peer read it off the packet", which is right on a
    /// LAN. On the public internet it is not: with privacy extensions the source
    /// address is a temporary IPv6 that rotates, so peers end up holding an
    /// address that has stopped answering. See `network::advertised_addr`.
    #[serde(default)]
    pub advertise_addr: Option<String>,
    #[serde(default)]
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
    /// Coleta de logs de processo para o nó principal. A decisão é do
    /// principal e os demais nós só adotam a resposta autenticada da catraca.
    #[serde(default)]
    pub cluster_logs_enabled: bool,
    /// Canal global de conversa, decidido pelo principal e confirmado pelos
    /// demais nós via leitura autenticada.
    #[serde(default)]
    pub network_chat_enabled: bool,
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
    /// Maximum disk space reserved for generated model slices, in GB.
    #[serde(default = "default_shard_max_gb")]
    pub shard_max_gb: f64,
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
    /// Diretório onde os snapshots de KV cache são gravados e lidos, como o
    /// `--slot-save-path` do llama-server. Propriedade da máquina, não da
    /// implantação: cada peer grava a fatia dele aqui. A variável de ambiente
    /// `POM_SLOT_SAVE_PATH` e a flag `--slot-save-path` têm precedência sobre
    /// este campo - use `slot_save_path_effective()`, nunca o campo cru.
    /// Ausente/vazio em todas as fontes desliga o recurso e as rotas de
    /// snapshot respondem "não configurado".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_save_path: Option<String>,
    /// Liga a persistência automática do cache de prefixo: ao fim de cada turno
    /// o gateway grava o KV retido em `slot_save_path` e, num miss após
    /// reinício, restaura em vez de refazer o prefill a frio. Exige
    /// `slot_save_path` em todos os hops. Desligado por padrão até ter prova de
    /// campo; a env var `POM_SLOT_PERSIST_PREFIX_CACHE=1` também liga. Use
    /// `slot_persist_prefix_cache_enabled()`.
    #[serde(default)]
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
            .field("api_key_count", &self.api_keys.len())
            .field("blocked_nodes", &self.blocked_nodes)
            .field("tls_required", &self.tls_required)
            .field("cluster_logs_enabled", &self.cluster_logs_enabled)
            .field("network_chat_enabled", &self.network_chat_enabled)
            .field("model_dirs", &self.model_dirs)
            .field("shard_dir", &self.shard_dir)
            .field("shard_max_gb", &self.shard_max_gb)
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

/// Port the local inference server listens on when the config does not name one.
///
/// A node configured through the web UI never passes `--inference-port`, and
/// without a port `apply` refused every model with "este nó subiu sem
/// --inference-port" — the UI could configure nothing at all. Advertising it
/// up front matches what `run-p1.sh`/`run-p2.sh` already do: the port is
/// announced before the Python server exists, and the server is spawned when a
/// slice is actually assigned.
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
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config at {:?}", path))?;
        parse_config(&content, &path)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        self.write_to(&path)
    }

    fn write_to(&self, path: &Path) -> Result<()> {
        let mut value = serde_json::to_value(self)?;
        if path.exists() {
            let old = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read existing config at {:?}", path))?;
            let old: serde_json::Value = serde_json::from_str(&old)
                .with_context(|| format!("Failed to parse existing config at {:?}", path))?;
            // These fields are now owned by pom.db. Preserve their old values
            // when another setting updates pom.json, so the legacy source stays
            // a usable safety copy until the operator explicitly migrates it.
            for field in ["api_keys", "thermal", "huggingface_token"] {
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

    /// Inference port this node should use, falling back to the default.
    pub fn inference_port_or_default(&self) -> u16 {
        self.inference_port.unwrap_or(DEFAULT_INFERENCE_PORT)
    }

    pub fn active_api_key(&self) -> Option<&ApiKey> {
        self.api_keys.iter().find(|k| k.active)
    }

    /// Directories to scan for whole models, falling back to what LM Studio and
    /// Ollama put on this machine while the operator has not chosen.
    pub fn model_dir_paths(&self) -> Vec<PathBuf> {
        match &self.model_dirs {
            Some(dirs) => dirs.iter().map(PathBuf::from).collect(),
            None => crate::catalog::default_model_dirs(),
        }
    }

    /// True while the whole-model list is still the auto-detected one — the UI
    /// says so instead of pretending the operator picked these paths.
    pub fn model_dirs_are_default(&self) -> bool {
        self.model_dirs.is_none()
    }

    /// Destination of local slicing, and the only directory a peer may pull
    /// from (see `transfer::safe_model_path`).
    pub fn shard_dir_path(&self) -> PathBuf {
        match self.shard_dir.as_deref().map(str::trim) {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => default_shard_dir(),
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

    /// Diretório efetivo de snapshots de KV cache, ou `None` quando o recurso
    /// está desligado. Precedência: a variável de ambiente `POM_SLOT_SAVE_PATH`
    /// (derivada da marca, `brand::env("SLOT_SAVE_PATH")`) vence o campo do
    /// `pom.json`. Vazio em qualquer uma conta como desligado, igual à regra de
    /// `shard_dir`. Como a env var é por processo, ela **não** é persistida:
    /// `save()` continua gravando só o campo do arquivo.
    pub fn slot_save_path_effective(&self) -> Option<String> {
        crate::brand::env("SLOT_SAVE_PATH")
            .ok()
            .or_else(|| self.slot_save_path.clone())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
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

/// `<project_root>/models` — where every slice has always landed.
pub fn default_shard_dir() -> PathBuf {
    project_root().join("models")
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

#[cfg(test)]
mod tests {
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
            tls_required: false,
            cluster_logs_enabled: false,
            network_chat_enabled: false,
            connection_key: "test-key".into(),
            gateway_port: 8080,
            admin_exposure: AdminExposure::Localhost,
            admin_access_token: None,
            p2p_port: 42777,
            inference_port: Some(8181),
            bootstrap_nodes: vec![],
            advertise_addr: None,
            api_keys: vec![],
            blocked_nodes: vec![],
            model_dirs: None,
            shard_dir: None,
            shard_max_gb: default_shard_max_gb(),
            node_name: None,
            huggingface_token: None,
            slot_save_path: None,
            slot_persist_prefix_cache: false,
            slot_auto_max_bytes: None,
            slot_auto_ttl_secs: None,
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
    fn test_roundtrip_node_config() {
        let cfg = Config {
            gpus: None,
            thermal: Default::default(),
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            profile: crate::profile::NodeProfile::Node,
            tls_required: false,
            cluster_logs_enabled: false,
            network_chat_enabled: false,
            connection_key: "node-key".into(),
            gateway_port: 8090,
            admin_exposure: AdminExposure::Localhost,
            admin_access_token: None,
            p2p_port: 42777,
            inference_port: Some(8181),
            bootstrap_nodes: vec!["192.168.1.1:42777".into()],
            advertise_addr: None,
            api_keys: vec![],
            blocked_nodes: vec![],
            model_dirs: None,
            shard_dir: None,
            shard_max_gb: default_shard_max_gb(),
            node_name: None,
            huggingface_token: None,
            slot_save_path: None,
            slot_persist_prefix_cache: false,
            slot_auto_max_bytes: None,
            slot_auto_ttl_secs: None,
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
    fn cache_types_are_persisted_as_installation_settings() {
        let cfg: Config =
            serde_json::from_str(r#"{"profile":"node","cache_type_k":"q8_0","cache_type_v":"bf16"}"#)
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
            connection_key: String::new(),
            gateway_port: 8080,
            admin_exposure: AdminExposure::Localhost,
            admin_access_token: None,
            p2p_port: 42777,
            inference_port: None,
            bootstrap_nodes: vec![],
            advertise_addr: None,
            api_keys: vec![],
            blocked_nodes: vec![],
            tls_required: false,
            cluster_logs_enabled: false,
            network_chat_enabled: false,
            model_dirs: None,
            shard_dir: None,
            shard_max_gb: default_shard_max_gb(),
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
        };
        cfg.admin_exposure = AdminExposure::Network;
        cfg.admin_access_token = Some("admin_super_secret".into());

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
        assert_eq!(saved["admin_access_token"], "admin_super_secret");
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
        let cfg: Config = serde_json::from_str(r#"{"profile":"node","connection_key":"k"}"#).unwrap();
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
    fn test_active_api_key() {
        let cfg = Config {
            gpus: None,
            thermal: Default::default(),
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            profile: crate::profile::NodeProfile::Admin,
            tls_required: false,
            cluster_logs_enabled: false,
            network_chat_enabled: false,
            connection_key: "k".into(),
            gateway_port: 8080,
            admin_exposure: AdminExposure::Localhost,
            admin_access_token: None,
            p2p_port: 42777,
            inference_port: None,
            bootstrap_nodes: vec![],
            advertise_addr: None,
            api_keys: vec![
                ApiKey {
                    id: "old-id".into(),
                    key: "sk-old".into(),
                    label: "old".into(),
                    node_id: "n1".into(),
                    origin_node_id: "n1".into(),
                    active: false,
                    created_at: "2026-01-01T00:00:00Z".into(),
                },
                ApiKey {
                    id: "active-id".into(),
                    key: "sk-active".into(),
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
            shard_max_gb: default_shard_max_gb(),
            node_name: None,
            huggingface_token: None,
            slot_save_path: None,
            slot_persist_prefix_cache: false,
            slot_auto_max_bytes: None,
            slot_auto_ttl_secs: None,
        };
        assert_eq!(cfg.active_api_key().unwrap().key, "sk-active");
    }
}
