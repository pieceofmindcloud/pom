#![cfg_attr(windows, windows_subsystem = "windows")]

/// Build-time commit fallback, stamped by `build.rs` for non-container builds.
pub const BUILD_COMMIT: &str = env!("POM_BUILD_COMMIT");

/// Runtime image metadata wins over the compiled fallback. Docker sets this in
/// the final stage so changing commits does not invalidate the Cargo build.
pub fn build_commit() -> String {
    build_commit_from(
        std::env::var("POM_BUILD_COMMIT").ok().as_deref(),
        BUILD_COMMIT,
    )
}

fn build_commit_from(runtime: Option<&str>, compiled: &str) -> String {
    runtime
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(compiled)
        .to_owned()
}

/// Development-only controls are enabled only by an explicit process setting.
pub(crate) const POM_DESENV_ENV: &str = "POM_DESENV";
static POM_DESENV_ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

pub(crate) fn pom_desenv_enabled_from(value: Option<&str>) -> bool {
    value == Some("1")
}

pub(crate) fn initialize_pom_desenv() -> bool {
    *POM_DESENV_ENABLED
        .get_or_init(|| pom_desenv_enabled_from(std::env::var(POM_DESENV_ENV).ok().as_deref()))
}

pub(crate) fn pom_desenv_enabled() -> bool {
    initialize_pom_desenv()
}

#[path = "network/addrs.rs"]
pub mod addrs;
#[path = "backend/admin.rs"]
pub(crate) mod admin;
#[path = "backend/auth.rs"]
mod auth;
// `brand` e o cluster llama.cpp saíram para crates próprios (`crates/brand` e
// `crates/llama-engine`) para que editar backend/rede/roteamento não os
// reconstrua. Re-exportados abaixo para que `crate::brand::…`, `crate::llm::…`
// etc. continuem resolvendo em todo o `node` sem tocar os ~40 arquivos que os
// usam. Ver `docs/subsystems/build-execucao.md`.
use ::brand;
pub use llama_engine::{collective, gguf, kv_budget, llm, moe, mtp, multipart, sysmon, wire};
#[path = "admin_trust.rs"]
mod admin_trust;
#[path = "backend/capabilities.rs"]
mod capabilities;
#[path = "models/catalog.rs"]
mod catalog;
#[path = "network/cluster_logs.rs"]
mod cluster_logs;
#[path = "network/cluster_logs_policy.rs"]
mod cluster_logs_policy;
#[path = "utils/config.rs"]
mod config;
#[cfg(test)]
#[path = "backend/continuation_lab.rs"]
mod continuation_lab;
#[path = "core_update.rs"]
mod core_update;
#[path = "inference/cycle.rs"]
mod cycle;
#[path = "backend/db.rs"]
mod db;
#[path = "backend/deploy/mod.rs"]
mod deploy;
#[path = "models/deployment.rs"]
mod deployment;
#[path = "backend/devupdate.rs"]
mod devupdate;
#[path = "inference/direct.rs"]
mod direct;
#[path = "inference/direct_lab.rs"]
mod direct_lab;
#[path = "network/discovery.rs"]
mod discovery;
#[path = "enterprise_p2p.rs"]
mod enterprise_p2p;
#[path = "inference/executor.rs"]
mod executor;
#[path = "backend/gateway/mod.rs"]
mod gateway;
#[path = "models/huggingface.rs"]
mod huggingface;
#[path = "network/identity.rs"]
mod identity;
#[path = "inference/infer_server/mod.rs"]
mod infer_server;
#[path = "network/infer_tunnel.rs"]
mod infer_tunnel;
#[path = "models/lastslice.rs"]
mod lastslice;
#[path = "inference/link.rs"]
mod link;
#[cfg(test)]
#[path = "inference/link_lab.rs"]
mod link_lab;
#[path = "network/local_discovery.rs"]
mod local_discovery;
#[cfg(test)]
#[path = "network/local_discovery_tests.rs"]
mod local_discovery_tests;
#[cfg(target_os = "macos")]
mod macos_status;
#[path = "backend/messages.rs"]
mod messages;
#[path = "models/model.rs"]
mod model;
#[path = "models/provider.rs"]
mod model_provider;
#[path = "backend/model_provider.rs"]
mod model_provider_backend;
#[path = "network/network.rs"]
mod network;
#[path = "network/network_chat.rs"]
mod network_chat;
#[path = "network/network_chat_policy.rs"]
mod network_chat_policy;
#[cfg(test)]
#[path = "network/network_chat_tests.rs"]
mod network_chat_tests;
#[path = "models/ollama.rs"]
mod ollama;
#[path = "backend/openai.rs"]
mod openai;
#[path = "network/peer_client.rs"]
pub mod peer_client;
#[path = "routing/pipeline.rs"]
mod pipeline;
#[path = "network/pkarr_node.rs"]
mod pkarr_node;
#[path = "models/plan.rs"]
mod plan;
mod plugin_github;
#[path = "plugin_host.rs"]
mod plugin_host;
mod plugin_proxy;
mod plugin_ui;
#[path = "plugins.rs"]
mod plugins;
#[path = "models/pom.rs"]
mod pom;
#[path = "pom_core_update.rs"]
mod pom_core_update;
#[path = "inference/prefill/mod.rs"]
mod prefill;
#[path = "inference/prefix_cache.rs"]
mod prefix_cache;
#[cfg(test)]
#[path = "inference/prefix_cache_lab.rs"]
mod prefix_cache_lab;
#[cfg(test)]
#[path = "inference/prefix_reuse_lab.rs"]
mod prefix_reuse_lab;
#[path = "profile.rs"]
pub(crate) mod profile;
#[cfg(test)]
#[path = "profile_tests.rs"]
mod profile_tests;
#[path = "network/public_directory.rs"]
mod public_directory;
#[path = "network/quic.rs"]
pub mod quic;
#[path = "network/quic_policy.rs"]
pub mod quic_policy;
#[path = "inference/relay.rs"]
mod relay;
#[path = "backend/reqlog.rs"]
mod reqlog;
#[path = "utils/respawn.rs"]
mod respawn;
#[path = "network/route.rs"]
pub mod route;
#[path = "inference/runtime.rs"]
mod runtime;
#[path = "inference/scheduler.rs"]
mod scheduler;
#[path = "backend/serve.rs"]
mod serve;
#[path = "backend/session.rs"]
mod session;
#[path = "inference/spec_lab.rs"]
mod spec_lab;
#[path = "models/splitter.rs"]
mod splitter;
#[path = "models/storage.rs"]
mod storage;
#[path = "telemetry.rs"]
pub(crate) mod telemetry;
#[path = "utils/thermal.rs"]
mod thermal;
#[path = "network/tls.rs"]
pub mod tls;
#[path = "network/tls_policy.rs"]
pub mod tls_policy;
#[path = "backend/tool_call.rs"]
mod tool_call;
#[path = "inference/tp.rs"]
mod tp;
#[path = "backend/transfer.rs"]
mod transfer;
#[path = "backend/webapp.rs"]
mod webapp;
#[path = "backend/webui/mod.rs"]
mod webui;
pub(crate) mod windows_firewall;
#[cfg(test)]
#[path = "windows_firewall_tests.rs"]
mod windows_firewall_tests;
#[cfg(any(windows, test))]
mod windows_status;

use anyhow::{bail, Context, Result};
use clap::Parser;
use clap::Subcommand;
use discovery::Discovery;
use gateway::GatewayState;
use network::Network;
use ollama::OllamaClient;
#[cfg(windows)]
use std::io::Write;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, Duration};
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Subcommand, Debug)]
enum Command {
    /// Block automatic model restoration until the next explicit deployment.
    #[command(name = "undeploy")]
    Undeploy,

    /// Create or install the operator-anchored administrator trust file.
    #[command(name = "admin-anchor")]
    AdminAnchor {
        /// Stable operator-chosen identifier for this cluster. Required when
        /// creating the root locally, optional when importing a root document.
        #[arg(long)]
        cluster_id: Option<String>,

        /// SHA-256 fingerprint printed by the first invocation. It is required
        /// together with --confirm so the operator performs the manual check.
        #[arg(long)]
        fingerprint: Option<String>,

        /// Existing signed trust document transported out of band from the
        /// anchored primary to another node.
        #[arg(long = "from", value_name = "FILE")]
        from_file: Option<std::path::PathBuf>,

        /// Confirm that the displayed fingerprint was checked by the operator.
        #[arg(long)]
        confirm: bool,

        /// Replace an existing local trust file after the same explicit check.
        #[arg(long)]
        replace: bool,
    },
}

#[derive(Parser, Debug)]
#[command(
    name = "pom",
    about = "Inferência distribuída de LLM entre máquinas, sem servidor central",
    long_about = "Runs a node in a distributed LLM inference network.\n\
                  Models are deployed from the web UI served on --gateway-port;\n\
                  each node loads its slice and coordinates with peers to cover\n\
                  the full model."
)]
struct Args {
    /// Internal elevated child used by the Windows firewall permission flow.
    #[cfg(windows)]
    #[arg(long, hide = true)]
    windows_firewall_helper: Option<u16>,

    #[command(subcommand)]
    command: Option<Command>,

    /// UDP port for P2P communication.
    /// Use different ports to run multiple nodes on the same machine.
    #[arg(long, short = 'p', default_value = "42777")]
    port: u16,

    /// Bootstrap node address(es), comma-separated (e.g. 127.0.0.1:42777).
    /// Required when running multiple instances on the same machine.
    #[arg(long, short = 'b', default_value = "")]
    bootstrap: String,

    /// Address peers should keep for this node, e.g. `[2804:d55::1]:42777` or a
    /// bare IPv6. Overrides `advertise_addr` in pom.json. Without it each
    /// peer reads the address off the packet, which on the public internet means
    /// a temporary IPv6 that rotates.
    #[arg(long, default_value = "")]
    advertise_addr: String,

    /// Ollama base URL (model resolution, /health, e a porta anunciada aos peers)
    #[arg(long, default_value = "http://localhost:11434")]
    ollama_url: String,

    /// HTTP port for the gateway/API server (served by every node)
    #[arg(long, default_value = "8080")]
    gateway_port: u16,

    /// IP the setup page listens on while the node has no pom.json. Defaults to
    /// every interface so a fresh node (including one in a container with a
    /// published port) is reachable for first access; hardening happens in the
    /// setup itself. Pass 127.0.0.1 to keep the unauthenticated setup local.
    /// The configured gateway always listens on every interface and ignores
    /// this flag.
    #[arg(long, default_value = "0.0.0.0")]
    setup_bind: std::net::IpAddr,

    /// Enable the conservative cycle detector for this node (off by default).
    #[arg(long)]
    cycle_detection: bool,

    /// Pkarr public keys of known peers to dial on startup (comma-separated, format: pk:zbase32...).
    #[arg(long, default_value = "")]
    pkarr_peers: String,

    /// External address to publish to the pkarr DHT and to advertise to
    /// peers as this node's own address (HOST:PORT) — same effect as
    /// `--advertise-addr`, which wins if both are set. PORT here is the UDP
    /// port; libp2p TCP port will be UDP+10000. Without either, peers reached
    /// over the internet fall back to a public address observed via libp2p
    /// `identify`, if any peer has reported one; failing that, this node's
    /// own local network interfaces (usually wrong for a NAT'd node).
    #[arg(long, default_value = "")]
    public_addr: String,

    /// Pkarr keys of relay nodes to use for NAT traversal (comma-separated, format: pk:zbase32...).
    /// Added to `relay_peers` in pom.json.
    #[arg(long, default_value = "")]
    relay_peers: String,

    /// Run a relay server that accepts circuit connections from other peers (requires public IP).
    #[arg(long)]
    serve_relay: bool,

    /// Disable the built-in public IPFS relay nodes.
    #[arg(long)]
    no_default_relays: bool,

    /// Start off the network: no announce, no LAN broadcast, no bootstrap.
    ///
    /// The discovery broadcast goes to `255.255.255.255:42777` whatever `--port`
    /// says, so a node started to try the interface out joins whatever cluster
    /// is on the wire. This is how you run one next to a live cluster without
    /// showing up in it. The node still serves its own UI and API, and
    /// Configurações › Rede brings it into the network when you want it there.
    #[arg(long)]
    no_discovery: bool,

    /// `--port` veio da variável de ambiente, não da linha de comando.
    /// Preenchido por `parse_args`; ver `apply_env_overrides`.
    #[arg(skip)]
    port_from_env: bool,

    /// `--gateway-port` veio da variável de ambiente. Ver `port_from_env`.
    #[arg(skip)]
    gateway_port_from_env: bool,
}

/// Variável de ambiente de cada opção da linha de comando, pelo id do clap.
///
/// Toda opção tem a sua: a linha de comando vence a variável, que vence o
/// padrão (a ordem do clap). O nome sai de `brand::env_var`, por isso não pode
/// ir no atributo `env` do derive, que exige literal.
const ARG_ENV: &[(&str, &str)] = &[
    ("port", "PORT"),
    ("bootstrap", "BOOTSTRAP"),
    ("advertise_addr", "ADVERTISE_ADDR"),
    ("ollama_url", "OLLAMA_URL"),
    ("gateway_port", "GATEWAY_PORT"),
    ("setup_bind", "SETUP_BIND"),
    ("cycle_detection", "CYCLE_DETECTION"),
    ("pkarr_peers", "PKARR_PEERS"),
    ("public_addr", "PUBLIC_ADDR"),
    ("relay_peers", "RELAY_PEERS"),
    ("serve_relay", "SERVE_RELAY"),
    ("no_default_relays", "NO_DEFAULT_RELAYS"),
    ("no_discovery", "NO_DISCOVERY"),
];

/// A linha de comando com as variáveis de `ARG_ENV`. As flags booleanas leem a
/// variável como `1/0`, `true/false`, `yes/no` ou `on/off`, como as outras
/// variáveis booleanas do projeto.
fn cli_command() -> clap::Command {
    let mut command = <Args as clap::CommandFactory>::command();
    for (id, suffix) in ARG_ENV {
        command = command.mut_arg(*id, |arg| {
            let arg = arg.env(brand::env_var(suffix));
            if matches!(arg.get_action(), clap::ArgAction::SetTrue) {
                arg.value_parser(clap::builder::BoolishValueParser::new())
            } else {
                arg
            }
        });
    }
    command
}

fn parse_args() -> Args {
    let matches = cli_command().get_matches();
    let from_env =
        |id: &str| matches.value_source(id) == Some(clap::parser::ValueSource::EnvVariable);
    let mut args = <Args as clap::FromArgMatches>::from_arg_matches(&matches)
        .unwrap_or_else(|error| error.exit());
    args.port_from_env = from_env("port");
    args.gateway_port_from_env = from_env("gateway_port");
    args
}

/// A variável de ambiente vence o `pom.json`.
///
/// Nas outras opções isso já sai da precedência normal, porque a linha de
/// comando vence ou se soma à config. `--port` e `--gateway-port` são a
/// exceção: na linha de comando elas só valem para o modo setup, e a porta
/// gravada vence - é o que mantém uma troca de porta feita pela interface
/// depois do respawn, que repete o argv. Vindas do ambiente, elas vencem a
/// config.
fn apply_env_overrides(config: &mut config::Config, args: &Args) {
    if args.port_from_env {
        config.p2p_port = args.port;
    }
    if args.gateway_port_from_env {
        config.gateway_port = args.gateway_port;
    }
}

/// O nó é um serviço Tokio, e a thread principal é dele em toda plataforma
/// menos uma.
///
/// No macOS a thread principal pertence ao AppKit: `NSApplication` só aceita
/// ser conduzida a partir dela, e é ela que atende o item da barra de status.
/// No Windows, o Tao e a bandeja têm a mesma exigência de thread de UI. Por
/// isso o serviço é movido para uma thread de fundo com o seu próprio runtime,
/// e a principal fica no laço de eventos nativo. Nas outras plataformas nada
/// disso existe e o runtime é construído aqui mesmo, de modo que quem corre o
/// binário num servidor Linux não ganha dependência de interface gráfica.
fn main() -> Result<()> {
    respawn::remember_argv(std::env::args().collect());
    let (log_capture, log_receiver) =
        cluster_logs::LogCapture::new(cluster_logs::LOG_CAPTURE_QUEUE_CAPACITY);

    #[cfg(windows)]
    {
        let writer = windows_diagnostic_log_writer();
        tracing_subscriber::registry()
            .with(application_log_filter())
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(false)
                    .with_writer(writer),
            )
            .with(cluster_logs::CaptureLayer::new(log_capture.clone()))
            .init();
    }

    #[cfg(not(windows))]
    tracing_subscriber::registry()
        .with(application_log_filter())
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(cluster_logs::CaptureLayer::new(log_capture.clone()))
        .init();

    let pom_desenv = initialize_pom_desenv();
    info!("POM_DESENV habilitado: {pom_desenv}");

    // O `llama-engine` não conhece o layout do projeto nem a rede: injeta a raiz
    // que o `sysmon` mede e as funções de estado da coletiva TP que o `llm`
    // consulta de dentro do decode (`tp.rs` fica em `node`, ver `tp_probe`).
    sysmon::set_disk_root(config::project_root());
    tp::install_probe();

    let args = parse_args();
    #[cfg(windows)]
    if let Some(port) = args.windows_firewall_helper {
        return windows_firewall::run_elevated_helper(port);
    }
    if let Some(command) = args.command.as_ref() {
        return run_command(command);
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let gateway_port = configured_gateway_port(&args);

    let service = move |gateway_ready: Option<gateway::GatewayReadySender>| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async_main(args, log_capture, log_receiver, gateway_ready))
    };

    #[cfg(target_os = "macos")]
    {
        return macos_status::run(gateway_port, move || service(None));
    }

    #[cfg(target_os = "windows")]
    {
        let (gateway_ready, gateway_ready_rx) = std::sync::mpsc::sync_channel(1);
        return windows_status::run(
            gateway_port,
            move || service(Some(gateway_ready)),
            gateway_ready_rx,
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    service(None)
}

fn run_command(command: &Command) -> Result<()> {
    match command {
        Command::Undeploy => {
            lastslice::force_disable()?;
            info!("retomada automática de implantação bloqueada");
            Ok(())
        }
        Command::AdminAnchor {
            cluster_id,
            fingerprint,
            from_file,
            confirm,
            replace,
        } => anchor_admin(
            cluster_id.as_deref(),
            fingerprint.as_deref(),
            from_file.as_deref(),
            *confirm,
            *replace,
        ),
    }
}

fn anchor_admin(
    cluster_id: Option<&str>,
    fingerprint: Option<&str>,
    from_file: Option<&std::path::Path>,
    confirm: bool,
    replace: bool,
) -> Result<()> {
    let destination = config::admin_trust_path();
    if destination.exists() && !replace {
        bail!(
            "a cadeia administrativa já existe em {}; use --replace somente para uma reancoragem manual explícita",
            destination.display()
        );
    }

    let (document, local_root) = if let Some(source) = from_file {
        let source_store = crate::admin_trust::AdminTrust::load(source).with_context(|| {
            format!(
                "não foi possível ler a cadeia transportada em {}",
                source.display()
            )
        })?;
        let document = source_store.document();
        if let Some(expected_cluster) = cluster_id {
            if expected_cluster != document.anchor.cluster_id {
                bail!(
                    "o cluster da cadeia transportada é {}, não {}",
                    document.anchor.cluster_id,
                    expected_cluster
                );
            }
        }
        (document, false)
    } else {
        let cluster_id = cluster_id
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("--cluster-id é obrigatório ao criar a raiz local"))?;
        let (_signing_key, principal) = crate::admin_trust::local_authority_identity()?;
        let anchor =
            crate::admin_trust::TrustAnchor::for_principal(cluster_id.to_string(), principal);
        let root_principal = anchor.principal.clone();
        let document = crate::admin_trust::AdminTrustDocument {
            version: crate::admin_trust::TRUST_VERSION,
            anchor,
            authority_epoch: 0,
            promotion_seq: 0,
            primary: root_principal.clone(),
            primary_history: vec![root_principal],
            grants: Vec::new(),
            demotions: Vec::new(),
            pending_transfer: None,
            completed_transfers: Vec::new(),
            replay: Vec::new(),
            setup: None,
            setup_signature: String::new(),
        };
        (document, true)
    };

    let expected = crate::admin_trust::anchor_fingerprint(&document);
    println!("impressão digital da autoridade: {expected}");
    // A mesma conferência que a interface faz. Duas seriam duas políticas.
    if !fingerprint.is_some_and(|typed| crate::admin_trust::fingerprint_matches(typed, &expected)) {
        bail!(
            "a impressão digital não foi confirmada; confira a linha acima e execute novamente com --fingerprint {expected} --confirm"
        );
    }
    if !confirm {
        bail!(
            "a impressão digital precisa de confirmação explícita; execute novamente com --confirm"
        );
    }

    if local_root {
        let (signing_key, principal) = crate::admin_trust::local_authority_identity()?;
        let anchor = crate::admin_trust::TrustAnchor::manual(
            document.anchor.cluster_id.clone(),
            principal,
            expected,
        )?;
        crate::admin_trust::AdminTrust::bootstrap_primary_with_anchor(
            &destination,
            anchor,
            &signing_key,
        )?;
        if config::Config::exists() {
            let mut config = config::Config::load()?;
            if config.profile != crate::profile::NodeProfile::Admin {
                config.profile = crate::profile::NodeProfile::Admin;
                config.save()?;
            }
        }
    } else {
        crate::admin_trust::AdminTrust::install_document(&destination, document)?;
    }
    println!(
        "âncora administrativa instalada em {}",
        destination.display()
    );
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn gateway_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/admin-ui/chat")
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn select_gateway_port(config_port: Option<u16>, cli_port: u16) -> u16 {
    config_port.unwrap_or(cli_port)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn configured_gateway_port(args: &Args) -> u16 {
    if args.gateway_port_from_env {
        return args.gateway_port;
    }
    let persisted = config::Config::load()
        .ok()
        .map(|config| config.gateway_port);
    select_gateway_port(persisted, args.gateway_port)
}

async fn async_main(
    args: Args,
    log_capture: Arc<cluster_logs::LogCapture>,
    log_receiver: tokio::sync::mpsc::Receiver<cluster_logs::LogEntry>,
    gateway_ready: Option<gateway::GatewayReadySender>,
) -> Result<()> {
    info!(
        "Project root: {} (config: {})",
        config::project_root().display(),
        config::Config::config_path().display()
    );

    let database = Arc::new(db::PomDb::open(config::database_path())?);
    info!("Local database: {}", database.path().display());

    // ── Setup flow: a fresh node and a node that just left a network both
    // serve only the setup page. The latter keeps pom.json as a local
    // preference shell, so checking only for file existence would boot the old
    // network again.
    let mut existing_config = if config::Config::exists() {
        Some(config::Config::load()?)
    } else {
        None
    };
    if let Some(config) = existing_config.as_mut() {
        apply_env_overrides(config, &args);
    }
    if existing_config
        .as_ref()
        .is_none_or(|config| config.setup_pending)
    {
        if existing_config.is_some() {
            info!("Setup pending — starting in setup mode");
        } else {
            info!("No config found — starting in setup mode");
        }
        let gateway_port = existing_config
            .as_ref()
            .map_or(args.gateway_port, |config| config.gateway_port);
        start_setup_mode(
            args.setup_bind,
            gateway_port,
            args.port,
            existing_config,
            database.clone(),
            gateway_ready,
        )
        .await?;
        return Ok(());
    }

    // ── Load existing config ─────────────────────────────────────────────────
    let mut app_config = existing_config.expect("config já carregada antes do modo completo");
    if app_config.connection_key.trim().is_empty() {
        app_config.connection_key = crate::webui::fresh_connection_key();
        app_config.save()?;
        info!("Generated a local admin credential for the configured node");
    }
    log_capture.set_min_level(app_config.cluster_logs_min_level);
    let cluster_logs = cluster_logs::ClusterLogs::new(log_capture, log_receiver, database.clone());
    let audit = reqlog::AuditLog::new(database.clone());
    app_config.api_keys = database.api_keys()?;
    if let Err(error) = database.save_network_settings(&app_config.network) {
        warn!("não foi possível guardar os detalhes da rede no banco: {error:#}");
    }
    app_config.thermal = database
        .thermal_policy()?
        .unwrap_or_else(crate::thermal::ThermalPolicy::default);
    app_config
        .thermal
        .validate()
        .context("validar política térmica persistida")?;
    app_config.huggingface_token = database.huggingface_token()?;
    config::validate_gateway_port(app_config.gateway_port)?;
    info!("Profile: {}", app_config.profile);

    // Antes de qualquer coisa que possa carregar modelo: os backends do ggml
    // enumeram os dispositivos ao serem registrados, e depois disso a lista está
    // congelada. Aplicar a escolha mais tarde não tiraria nem devolveria placa.
    {
        let detected = sysmon::sample().await.gpus;
        match sysmon::apply_gpu_selection(app_config.gpus.as_deref(), &detected) {
            _ if app_config.gpus.is_none() => {
                info!(
                    "GPUs: todas as {} disponíveis (pom.json não restringe)",
                    detected.len()
                );
            }
            escolhidas if escolhidas.is_empty() => {
                info!("GPUs: nenhuma - inferência em CPU (pom.json traz uma lista vazia)");
            }
            escolhidas => info!("GPUs: {}", escolhidas.join(", ")),
        }
    }
    let gateway_port = app_config.gateway_port;

    crate::gateway::ensure_gateway_port_available(app_config.gateway_port, None).await?;

    // O nó sobe sempre sem modelo: o que ele serve vem da implantação feita pela
    // interface, restaurada do journal da fatia (`lastslice`) num reinício.
    start_node(
        gateway_port,
        database.clone(),
        app_config.clone(),
        args.ollama_url,
        args.no_discovery,
        args.no_default_relays,
        merged_bootstrap_nodes(&app_config.bootstrap_nodes, &args.bootstrap),
        merged_relay_peers(&app_config.relay_peers, &args.relay_peers),
        comma_list(&args.pkarr_peers),
        args.serve_relay,
        args.advertise_addr.clone(),
        args.public_addr.clone(),
        args.cycle_detection,
        cluster_logs,
        audit,
        gateway_ready,
    )
    .await
}

/// A porta de Ollama que este nó anuncia aos peers.
///
/// Não depende de flag nenhuma: todo nó serve a API, e um nó que servisse o
/// Ollama local sem anunciar a porta ficaria invisível para o roteamento.
fn advertised_ollama_port(ollama_url: &str) -> Option<u16> {
    parse_ollama_port(ollama_url)
}

/// Extract the port from an Ollama URL like "http://localhost:11434".
fn parse_ollama_port(url: &str) -> Option<u16> {
    url.trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .nth(1)
        .and_then(|s| s.split('/').next())
        .and_then(|s| s.parse::<u16>().ok())
}

async fn start_setup_mode(
    bind_ip: std::net::IpAddr,
    gateway_port: u16,
    p2p_port: u16,
    preserved: Option<config::Config>,
    database: Arc<db::PomDb>,
    gateway_ready: Option<gateway::GatewayReadySender>,
) -> Result<()> {
    use crate::config::Config;
    use crate::webui;
    use axum::{routing::get, Router};
    use std::net::SocketAddr;

    config::validate_gateway_port(gateway_port)?;
    let setup_p2p_port = preserved
        .as_ref()
        .map_or(p2p_port, |config| config.p2p_port);
    let config = Arc::new(RwLock::new(preserved.unwrap_or_else(|| Config {
        profile: crate::profile::NodeProfile::Admin,
        setup_pending: true,
        local_auto_discovery: false,
        tls_required: false,
        quic_enabled: false,
        cluster_logs_enabled: false,
        network_chat_enabled: false,
        cluster_logs_min_level: Default::default(),
        // Modo setup nao carrega modelo: a escolha de GPU so passa a valer
        // depois que a `pom.json` existir.
        gpus: None,
        thermal: Default::default(),
        cache_type_k: config::KvCacheType::F16,
        cache_type_v: config::KvCacheType::F16,
        connection_key: String::new(),
        gateway_port,
        admin_exposure: config::AdminExposure::Localhost,
        admin_access_token: None,
        p2p_port: setup_p2p_port,
        inference_port: None,
        bootstrap_nodes: vec![],
        relay_peers: vec![],
        advertise_addr: None,
        network: Default::default(),
        api_keys: vec![],
        blocked_nodes: vec![],
        // Um nó recém-configurado sobe com as bibliotecas detectadas; a tela de
        // Modelos é onde essa lista deixa de ser um palpite.
        model_dirs: None,
        shard_dir: None,
        workspace_root: None,
        shard_max_gb: crate::storage::DEFAULT_SHARD_MAX_GB,
        resources: Default::default(),
        setup_model: None,
        node_name: None,
        huggingface_token: None,
        slot_save_path: None,
        slot_persist_prefix_cache: false,
        slot_auto_max_bytes: None,
        slot_auto_ttl_secs: None,
    })));

    let setup_p2p_port = config.read().await.p2p_port;
    let setup_socket =
        tokio::net::UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], setup_p2p_port)))
            .await
            .with_context(|| {
                format!("não foi possível abrir a descoberta local na porta {setup_p2p_port}")
            })?;
    setup_socket.set_broadcast(true)?;
    let local_discovery = crate::local_discovery::LocalDiscoveryRuntime::new(
        Arc::new(setup_socket),
        SocketAddr::from(([255, 255, 255, 255], setup_p2p_port)),
    );
    {
        let discovery = local_discovery.clone();
        tokio::spawn(async move {
            discovery.run_receive_loop().await;
        });
    }

    // The same interface the configured node serves, so setup is not a second
    // look-and-feel to maintain. The app notices it is in setup mode because
    // `/api/ui/node` is not among the routes here.
    let app = Router::new()
        .route("/api/setup/hardware", get(webui::get_setup_hardware))
        .route("/api/setup/models", get(webui::get_setup_models))
        .route(
            "/api/setup/geo-reference",
            get(webui::get_setup_geo_reference),
        )
        .route(
            "/api/setup/admin-anchor/preview",
            axum::routing::post(webui::preview_setup_anchor),
        )
        .route(
            "/api/setup/trust-anchor",
            axum::routing::post(webui::install_setup_trust_anchor),
        )
        .route(
            "/api/setup/health",
            axum::routing::post(webui::get_setup_health),
        )
        .route(
            "/api/setup/auto-discovery/status",
            get(webui::get_setup_auto_discovery_status),
        )
        .route(
            "/api/setup/auto-discovery/start",
            axum::routing::post(webui::start_setup_auto_discovery),
        )
        .route(
            "/api/setup/auto-discovery/search",
            axum::routing::post(webui::search_setup_auto_discovery),
        )
        .route("/api/setup/save", axum::routing::post(webui::save_setup))
        .merge(crate::webapp::routes())
        .with_state(webui::SetupState {
            config,
            database,
            local_discovery,
        })
        .layer(tower_http::compression::CompressionLayer::new());

    let addr = SocketAddr::new(bind_ip, gateway_port);
    if bind_ip.is_loopback() {
        info!("Setup page available at http://localhost:{}", gateway_port);
    } else {
        warn!("Setup page listening on {addr} without authentication until setup is saved");
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    if let Some(ready) = gateway_ready {
        let _ = ready.send(());
    }
    axum::serve(listener, app).await?;
    Ok(())
}

/// `--advertise-addr` wins, else `--public-addr`, else the persisted config
/// value. `--public-addr` used to only reach the pkarr
/// DHT record; folding it in here is what makes it also reach the `Announce`
/// peers actually dial.
fn resolve_advertise_addr(
    cli_advertise: &str,
    cli_public: &str,
    config_val: Option<&str>,
) -> Option<String> {
    if !cli_advertise.trim().is_empty() {
        Some(cli_advertise.to_string())
    } else if !cli_public.trim().is_empty() {
        Some(cli_public.to_string())
    } else {
        config_val.map(|s| s.to_string())
    }
}

/// Convert the operator-facing advertised endpoint into the libp2p address
/// stored in the pkarr record. IPv6 endpoints must use `/ip6` and preserve
/// their configured port, including the bracketed `[addr]:port` form used by
/// the setup wizard.
fn pkarr_multiaddr_for_advertise(
    configured: Option<&str>,
    fallback_ip: Option<String>,
    default_udp_port: u16,
) -> String {
    let raw = configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or(fallback_ip)
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let (host, udp_port) = if let Some(rest) = raw.strip_prefix('[') {
        rest.split_once(']')
            .map(|(host, suffix)| {
                let port = suffix
                    .strip_prefix(':')
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(default_udp_port);
                (host.to_string(), port)
            })
            .unwrap_or((rest.trim_end_matches(']').to_string(), default_udp_port))
    } else if raw.parse::<std::net::Ipv6Addr>().is_ok() {
        (raw, default_udp_port)
    } else if let Some((host, port)) = raw.rsplit_once(':') {
        (host.to_string(), port.parse().unwrap_or(default_udp_port))
    } else {
        (raw, default_udp_port)
    };
    let protocol = if host.parse::<std::net::Ipv6Addr>().is_ok() {
        "ip6"
    } else {
        "ip4"
    };
    format!("/{protocol}/{host}/tcp/{}", udp_port.saturating_add(10000))
}

/// Everything `P2pStack` needs to bring up pkarr, a stable libp2p identity,
/// `Network`, and (best-effort) `Discovery`, kept in one function so the
/// ordering constraint below can't be violated from outside it: the libp2p keypair must be derived
/// *before* `Network::new()`'s builder chain (`.with_libp2p_peer_id(...)`
/// needs the derived `PeerId` string), and `Network` becomes immutable the
/// moment it's wrapped in `Arc`, right after that chain.
struct P2pStackArgs {
    node_id: String,
    pubkey: String,
    p2p_port: u16,
    bootstrap_nodes: Vec<String>,
    ollama_port: Option<u16>,
    inference_port: Option<u16>,
    inference_quic_port: Option<u16>,
    gateway_port: u16,
    gateway_quic_port: Option<u16>,
    profile: crate::profile::NodeProfile,
    node_display_name: String,
    advertise_addr: Option<String>,
    /// Política inicial, aplicada antes da task de sondagem da `Network`.
    /// Sem isso a URL já nascia em HTTPS pelo cliente global, mas a sonda
    /// começava em HTTP e todos os RTTs remotos ficavam ausentes.
    tls_required: bool,
    cluster_logs: Arc<cluster_logs::ClusterLogs>,
    audit: Arc<reqlog::AuditLog>,
    plugins: Arc<plugins::PluginManager>,
    primary_authority_pubkey: String,
    cluster_logs_enabled: bool,
    network_chat_enabled: bool,
    local_auto_discovery: bool,
    auth: Arc<RwLock<config::Config>>,
    admin_trust: Arc<crate::admin_trust::AdminTrust>,
    database: Arc<db::PomDb>,
    /// Raw pkarr public keys (`--relay-peers`), resolved to multiaddrs
    /// *inside* this function, after it loads its own `pkarr_node` — not
    /// pre-resolved by the caller. Resolution needs an already-loaded pkarr
    /// identity, and that identity is loaded here, not by the caller; asking
    /// the caller to resolve first would mean loading pkarr twice (once to
    /// resolve, once inside this function) or restructuring who loads it.
    relay_peer_keys: Vec<String>,
    serve_relay: bool,
    use_default_relays: bool,
    no_discovery: bool,
}

/// What a caller needs after `start_p2p_stack` returns, to keep going with
/// whatever is specific to its own path (layer assignment, `GatewayState`,
/// pkarr dial/publish — none of which belong inside this shared helper).
struct P2pStack {
    net: Arc<Network>,
    authority_signing_key: ed25519_dalek::SigningKey,
    libp2p_peer_id: String,
    cluster_logs: Arc<cluster_logs::ClusterLogs>,
    network_chat: Arc<network_chat::NetworkChat>,
    tunnel_tx: mpsc::Sender<infer_tunnel::TunnelOutbound>,
    pkarr_node: Option<pkarr_node::PkarrNode>,
    assignment_shared: Arc<RwLock<Option<model::LayerAssignment>>>,
    /// `Some` only if `Discovery::new` succeeded — lets a caller dial
    /// pkarr-resolved peers by `PeerId` on startup. `None` means libp2p
    /// never came up this run; callers must treat that as "skip it," not
    /// an error (this function itself already logged why).
    disc_dial_tx: Option<mpsc::Sender<libp2p::Multiaddr>>,
}

/// pkarr load → libp2p keypair derivation → tunnel channel → `Network`
/// (with a stable libp2p `PeerId`) → `net.start()` (UDP) → best-effort
/// `Discovery::new` (libp2p swarm) → the 5s periodic gossipsub re-announce.
/// Never fails on anything libp2p-related — only `Network::new()`'s own `?`
/// (UDP bind failure) propagates, exactly like today's CLI path. This is
/// deliberate: every caller of this function needs to always come up and
/// serve its gateway, with or without libp2p.
async fn start_p2p_stack(args: P2pStackArgs) -> Result<P2pStack> {
    let bootstrap_nodes: Vec<String> = args
        .bootstrap_nodes
        .iter()
        .map(|addr| crate::config::normalize_peer_addr(addr))
        .filter(|addr| !addr.is_empty())
        .collect();
    infer_tunnel::set_local_node_id(args.node_id.clone());
    let cluster_logs = args.cluster_logs.clone();
    let primary_authority_pubkey = args.primary_authority_pubkey.clone();
    let plugins = args.plugins.clone();
    cluster_logs.set_identity(args.node_id.clone(), args.node_display_name.clone());
    // Keep the startup behavior of the existing process-log receiver until
    // the authenticated trust document is loaded below; the final receiver
    // state is narrowed to effective admins before the gateway is exposed.
    cluster_logs.set_admin_receiver(args.profile == crate::profile::NodeProfile::Admin);
    cluster_logs.set_enabled(args.cluster_logs_enabled);
    let audit = args.audit.clone();
    audit.set_admin_receiver(args.profile == crate::profile::NodeProfile::Admin);

    let pkarr_node = match pkarr_node::PkarrNode::load_or_create(&identity::pkarr_key_path()) {
        Ok(n) => {
            info!("🔑 Pkarr address: {}", n.public_key_uri());
            Some(n)
        }
        Err(e) => {
            warn!("pkarr init failed (discovery disabled): {}", e);
            None
        }
    };

    // `Network` needs to advertise the resulting PeerId in its own
    // `Announce`, and it's built (then wrapped in `Arc`, no longer mutable
    // in place) a few lines below — so the keypair has to exist before that.
    let authority_signing_key = pkarr_node
        .as_ref()
        .map(|pn| ed25519_dalek::SigningKey::from_bytes(&pn.secret_key_bytes()))
        .unwrap_or_else(|| ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng));
    let mut authority_seed = authority_signing_key.to_bytes();
    let libp2p_keypair = libp2p::identity::Keypair::ed25519_from_bytes(&mut authority_seed[..])
        .expect("a chave Ed25519 da autoridade deve ser uma identidade libp2p válida");
    let libp2p_peer_id = libp2p_keypair.public().to_peer_id().to_string();

    // Resolved *here*, not by the caller — needs the `pkarr_node` this
    // function just loaded (see `P2pStackArgs::relay_peer_keys`'s doc comment).
    let resolved_relay_addrs: Vec<String> = if let Some(ref pn) = pkarr_node {
        if !args.relay_peer_keys.is_empty() {
            let peers = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                pn.resolve_peers(&args.relay_peer_keys),
            )
            .await
            .unwrap_or_default();
            peers
                .into_iter()
                .filter(|p| p.is_relay)
                .map(|p| p.multiaddr)
                .collect()
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    // The gateway's HTTP server needs a cloneable sender before `Discovery::new`
    // — which owns the matching receiver — is ever called. Registered
    // globally too (`infer_tunnel::set_tunnel_sender`) so `infer_server.rs`'s
    // ring-relay code, which has no `GatewayState` handle, can reach it the
    // same way it already reaches `http_client()`. Sending on this channel
    // before `Discovery::new` succeeds (or if it never does) fails fast with
    // a closed-channel error — the same already-handled outcome
    // `send_frame_via_tunnel` treats as "no tunnel available," not a hang.
    let (tunnel_tx, tunnel_rx) = mpsc::channel::<infer_tunnel::TunnelOutbound>(64);
    infer_tunnel::set_tunnel_sender(tunnel_tx.clone());

    let net = Arc::new(
        Network::new(
            args.node_id,
            args.pubkey,
            args.p2p_port,
            bootstrap_nodes.clone(),
            args.ollama_port,
            args.inference_port,
            Some(args.gateway_port),
        )
        .await?
        .with_identity(args.profile, args.node_display_name)
        .with_advertise_addr(args.advertise_addr)
        .with_quic_ports(args.inference_quic_port, args.gateway_quic_port)
        .with_authority_pubkey(hex::encode(
            authority_signing_key.verifying_key().as_bytes(),
        ))
        .with_libp2p_peer_id(Some(libp2p_peer_id.clone()))
        .with_admission(args.auth.clone(), args.admin_trust.clone()),
    );
    net.set_policy_principal_pubkey(primary_authority_pubkey);
    net.set_tls(args.tls_required);
    net.set_cluster_logs_enabled(args.cluster_logs_enabled);
    net.set_network_chat_enabled(args.network_chat_enabled);
    if args.local_auto_discovery {
        if let Err(error) = net.set_local_auto_discovery(true).await {
            warn!("descoberta local não pôde ser habilitada na inicialização: {error:#}");
        }
    }

    if args.no_discovery {
        net.detach().await;
        info!("🚫 --no-discovery: nó começa fora da rede, sem anunciar nem procurar peers");
    }

    let assignment_shared: Arc<RwLock<Option<model::LayerAssignment>>> =
        Arc::new(RwLock::new(None));
    net.clone().start(assignment_shared.clone()).await?;
    let (log_tx, log_rx) = mpsc::channel(cluster_logs::LOG_TRANSPORT_QUEUE_CAPACITY);
    cluster_logs.start(net.clone(), assignment_shared.clone(), log_tx);
    let (network_chat, chat_rx) = network_chat::NetworkChat::new(
        args.database.clone(),
        args.auth.clone(),
        args.network_chat_enabled,
    );
    let (audit_tx, audit_rx) = mpsc::channel(reqlog::AUDIT_TRANSPORT_QUEUE_CAPACITY);
    audit.start_replication(net.clone(), audit_tx);

    // libp2p TCP port = UDP port + 10 000 (e.g. 42777 → 52777). Announce
    // messages are published to gossipsub every 5 s so internet peers learn
    // our layer assignment without needing a shared signalling server.
    // `Network::new` may have received port 0 and selected an ephemeral UDP
    // port. Discovery must derive its TCP port from the assigned value, not
    // from the unresolved configuration value.
    let (disc_announce_tx, disc_dial_tx) = match Discovery::new(
        net.bind_addr.port(),
        &bootstrap_nodes,
        &resolved_relay_addrs,
        args.serve_relay,
        args.use_default_relays,
        net.clone(),
        libp2p_keypair,
        tunnel_rx,
        log_rx,
        cluster_logs.clone(),
        chat_rx,
        network_chat.clone(),
        audit_rx,
        audit,
        plugins,
    ) {
        Ok(disc) => {
            let ann_tx = disc.announce_tx.clone();
            let dial_tx = disc.dial_tx.clone();
            net.set_libp2p_tunnel_ready(true);
            tokio::spawn(async move { disc.run().await });
            (Some(ann_tx), Some(dial_tx))
        }
        Err(e) => {
            warn!("libp2p discovery disabled ({}). UDP-only mode.", e);
            (None, None)
        }
    };

    if let Some(tx) = disc_announce_tx {
        let net_disc = net.clone();
        let asgn_disc = assignment_shared.clone();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(5));
            loop {
                ticker.tick().await;
                // Mesmo construtor do caminho UDP (`Network::self_announce`):
                // capabilities, papel e hostname iguais, e publica **sempre**,
                // com ou sem assignment. Um nó ainda sem modelo precisa ser
                // visto pela internet do mesmo jeito que já era visto na LAN.
                let data = {
                    let asgn = asgn_disc.read().await;
                    net_disc.gossip_announce_bytes(asgn.as_ref())
                };
                if let Some(data) = data {
                    let _ = tx.send(data).await;
                }
            }
        });
    }

    Ok(P2pStack {
        net,
        authority_signing_key,
        libp2p_peer_id,
        cluster_logs,
        network_chat,
        tunnel_tx,
        pkarr_node,
        assignment_shared,
        disc_dial_tx,
    })
}

/// A comma-separated CLI list (`--relay-peers`, `--pkarr-peers`), trimmed and
/// without empty entries.
fn comma_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Relay pkarr keys from `pom.json` (`relay_peers`) plus `--relay-peers`,
/// config first, without duplicates, same as `merged_bootstrap_nodes`.
fn merged_relay_peers(from_config: &[String], cli: &str) -> Vec<String> {
    let mut keys: Vec<String> = from_config
        .iter()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .collect();
    for key in comma_list(cli) {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// Bootstrap nodes from `pom.json` plus the `--bootstrap` extras, without
/// duplicates. Normalised on the way in, config file included: an entry saved
/// before there was any validation is exactly the one that fails at send time.
fn merged_bootstrap_nodes(from_config: &[String], cli: &str) -> Vec<String> {
    let mut nodes: Vec<String> = from_config
        .iter()
        .map(|b| config::normalize_peer_addr(b))
        .filter(|s| !s.is_empty())
        .collect();
    for b in cli
        .split(',')
        .map(config::normalize_peer_addr)
        .filter(|s| !s.is_empty())
    {
        if !nodes.contains(&b) {
            nodes.push(b);
        }
    }
    nodes
}

/// Tarefas de fundo que acompanham todo gateway.
fn spawn_gateway_companions(gw_state: &GatewayState, database: &Arc<db::PomDb>, bind_port: u16) {
    crate::windows_firewall::spawn_startup_check(database.clone(), bind_port);
    crate::webui::spawn_network_settings_ratchet(gw_state.clone());
    crate::plugins::spawn_host_context(gw_state.clone());
    crate::plugins::spawn_serving_watcher(gw_state.clone());
    crate::gateway::spawn_public_directory_republisher(gw_state.clone());
}

/// Sobe o nó configurado: rede, gateway com todas as rotas e, se houver, a
/// fatia anotada por um reinício. O modelo nunca vem da linha de comando: ele é
/// implantado pela interface.
async fn start_node(
    gateway_port: u16,
    database: Arc<db::PomDb>,
    app_config: config::Config,
    ollama_url: String,
    no_discovery: bool,
    no_default_relays: bool,
    bootstrap_nodes: Vec<String>,
    relay_peer_keys: Vec<String>,
    pkarr_peer_keys: Vec<String>,
    serve_relay: bool,
    advertise_addr_cli: String,
    public_addr_cli: String,
    cycle_detection_enabled: bool,
    cluster_logs: Arc<cluster_logs::ClusterLogs>,
    audit: Arc<reqlog::AuditLog>,
    gateway_ready: Option<gateway::GatewayReadySender>,
) -> Result<()> {
    use crate::gateway::GatewayState;
    let identity = identity::Identity::load_or_create(&identity::key_path());
    let node_id = identity.node_id.0.clone();
    let pubkey = identity.public_key_hex();
    let plugins = plugins::PluginManager::runtime_with_database(&node_id, database.clone());

    let node_display_name = match app_config.node_name.clone() {
        Some(name) => name,
        None => sysmon::sample().await.hostname,
    };
    let advertise_addr = resolve_advertise_addr(
        &advertise_addr_cli,
        &public_addr_cli,
        app_config.advertise_addr.as_deref(),
    );
    let ollama_port = advertised_ollama_port(&ollama_url);
    let inference_port = Some(app_config.inference_port_or_default());
    // Portas QUIC derivadas do `p2p_port` deste nó: ver `quic::gateway_quic_port`.
    let inference_quic_port =
        inference_port.and_then(|_| crate::quic::inference_quic_port(app_config.p2p_port));
    let gateway_quic_port = crate::quic::gateway_quic_port(app_config.p2p_port);
    peer_client::set_quic_ativo(app_config.quic_enabled);
    let auth_state: Arc<RwLock<config::Config>> = Arc::new(RwLock::new(app_config.clone()));
    let admin_trust = Arc::new(crate::admin_trust::AdminTrust::load_or_empty(
        config::admin_trust_path(),
    ));
    let stack = start_p2p_stack(P2pStackArgs {
        node_id: node_id.clone(),
        pubkey: pubkey.clone(),
        p2p_port: app_config.p2p_port,
        bootstrap_nodes,
        ollama_port,
        inference_port,
        inference_quic_port,
        gateway_port,
        gateway_quic_port,
        profile: app_config.profile,
        node_display_name,
        advertise_addr: advertise_addr.clone(),
        tls_required: app_config.tls_required,
        cluster_logs: cluster_logs.clone(),
        audit: audit.clone(),
        plugins: plugins.clone(),
        primary_authority_pubkey: admin_trust.current_primary().pubkey.clone(),
        cluster_logs_enabled: app_config.cluster_logs_enabled,
        network_chat_enabled: app_config.network_chat_enabled,
        local_auto_discovery: app_config.local_auto_discovery,
        auth: auth_state.clone(),
        admin_trust: admin_trust.clone(),
        database: database.clone(),
        relay_peer_keys,
        serve_relay,
        use_default_relays: !no_default_relays,
        no_discovery,
    })
    .await?;
    let authority_signing_key = Arc::new(stack.authority_signing_key.clone());
    let authority_principal = crate::admin_trust::AuthorityPrincipal::from_signing_key(
        node_id.clone(),
        stack.libp2p_peer_id.clone(),
        &stack.authority_signing_key,
    );
    if app_config.profile == crate::profile::NodeProfile::Admin
        && admin_trust
            .effective_authority(&authority_principal, app_config.profile)
            .is_none()
    {
        warn!(
            "perfil admin sem concessão administrativa válida; capacidade efetiva permanece node. \
             Para ancorar o primeiro principal nesta máquina: `pom admin-anchor --cluster-id <nome>`, \
             confira a impressão digital exibida e repita com `--fingerprint <valor> --confirm`. \
             Para um nó que já tem principal no cluster, copie {} do admin por um canal seu e rode \
             `pom admin-anchor --from <arquivo> --fingerprint <valor> --confirm`",
            config::admin_trust_filename()
        );
    }
    let net = stack.net;
    net.set_policy_principal_pubkey(admin_trust.current_primary().pubkey);
    let assignment_shared = stack.assignment_shared;
    let tunnel_tx = stack.tunnel_tx;
    let cluster_logs = stack.cluster_logs;
    let network_chat = stack.network_chat;
    let disc_dial_tx = stack.disc_dial_tx;

    // Um admin público ainda precisa do registro pkarr para o diretório de
    // redes públicas encontrá-lo antes de qualquer implantação.
    let pkarr_node = stack.pkarr_node.map(Arc::new);
    let pkarr_multiaddr = pkarr_multiaddr_for_advertise(
        advertise_addr.as_deref(),
        pkarr_node::detect_local_ip(),
        app_config.p2p_port,
    );
    let pkarr_publisher = pkarr_node.as_ref().map(|node| {
        Arc::new(crate::gateway::PkarrPublishHandle::new(
            node.clone(),
            pkarr_multiaddr.clone(),
            Some(gateway_port),
            None,
            String::new(),
            assignment_shared.clone(),
            serve_relay,
            Some(net.clone()),
        ))
    });
    if let Some(publisher) = pkarr_publisher.clone() {
        let geo_label = app_config.network.geo_label.clone();
        tokio::spawn(async move {
            if let Err(error) = publisher.publish_geo_label(&geo_label).await {
                warn!("pkarr initial publish failed: {error}");
            }
        });
    }
    // `--pkarr-peers`: resolvidos uma vez e discados pelo libp2p, em segundo
    // plano para a resolução (até 10 s) não atrasar o gateway.
    if let (Some(node), Some(dial_tx), false) =
        (pkarr_node.clone(), disc_dial_tx, pkarr_peer_keys.is_empty())
    {
        tokio::spawn(async move {
            let peers = tokio::time::timeout(
                Duration::from_secs(10),
                node.resolve_peers(&pkarr_peer_keys),
            )
            .await
            .unwrap_or_default();
            for peer in &peers {
                if let Ok(address) = peer.multiaddr.parse::<libp2p::Multiaddr>() {
                    let _ = dial_tx.send(address).await;
                }
            }
        });
    }
    let local_is_admin = admin_trust
        .effective_authority(&authority_principal, app_config.profile)
        .is_some();
    net.set_policy_principal_pubkey(admin_trust.current_primary().pubkey);
    cluster_logs.set_admin_receiver(local_is_admin);
    audit.set_admin_receiver(local_is_admin);

    // ── TLS entre peers ──────────────────────────────────────────────────────
    // O certificado é derivado da identidade que acabou de ser carregada e
    // nunca vai para o disco (ver `tls.rs`). Ele existe desde o arranque mesmo
    // com a flag desligada, porque é o que permite ligar TLS sem reiniciar: o
    // servidor já sabe falar as duas línguas, e a flag só decide se texto claro
    // remoto continua aceito.
    // Endereços que entram no certificado do navegador: os locais desta
    // máquina, o que o operador fixou, e o hostname. Sem eles o navegador
    // reclamaria do host, e não do emissor.
    let tls_sans = serve::browser_sans(app_config.advertise_addr.as_deref());
    let quic_config = match serve::quic_server_config(&identity) {
        Ok(config) => Some(config),
        Err(error) => {
            warn!("endpoint HTTP/3 indisponível, mantendo fallback TCP: {error}");
            None
        }
    };
    let tls_server =
        match serve::TlsServer::from_identity(&identity, app_config.tls_required, &tls_sans) {
            Ok(t) => Some(t),
            Err(e) => {
                warn!("não deu para derivar o certificado deste nó, TLS indisponível: {e}");
                None
            }
        };

    let hf_token_store =
        huggingface::HfTokenStore::new_from_database(app_config.huggingface_token.clone());
    let node_runtime = runtime::NodeRuntime::new_with_tls_and_quic_and_auth(
        net.clone(),
        assignment_shared.clone(),
        tls_server.clone(),
        quic_config.clone(),
        Some(auth_state.clone()),
    );
    node_runtime.start_thermal_monitor(auth_state.clone()).await;

    // Bind the gateway before restoring a persisted slice. Applying it may
    // schedule a model load, but that load must never hide the control plane.
    let peer_clients = Arc::new(peer_client::PeerClients::new(Duration::from_secs(600)));
    peer_clients.apply_cluster_policy(app_config.tls_required);
    peer_clients.set_quic(app_config.quic_enabled);
    peer_clients.set_auth_state(auth_state.clone());

    tls_policy::spawn_ratchet(
        net.clone(),
        peer_clients.clone(),
        auth_state.clone(),
        tls_server.clone(),
        authority_signing_key.clone(),
        authority_principal.clone(),
        admin_trust.clone(),
    );
    quic_policy::spawn_ratchet(net.clone(), peer_clients.clone(), auth_state.clone());
    cluster_logs_policy::spawn_ratchet(
        net.clone(),
        peer_clients.clone(),
        auth_state.clone(),
        cluster_logs.clone(),
        authority_signing_key.clone(),
        authority_principal.clone(),
        admin_trust.clone(),
    );
    network_chat_policy::spawn_ratchet(
        net.clone(),
        peer_clients.clone(),
        auth_state.clone(),
        network_chat.clone(),
        authority_signing_key.clone(),
        authority_principal.clone(),
        admin_trust.clone(),
    );

    let pom_core_update = pom_core_update::PomCoreUpdater::runtime(plugins.clone());
    let gw_state = GatewayState {
        network: net.clone(),
        ollama: Arc::new(OllamaClient::new(&ollama_url)),
        default_model: String::new(),
        local_assignment: assignment_shared.clone(),
        loads: node_runtime.load_tracker(),
        prefix_cache: crate::prefix_cache::PrefixCache::default_arc(),
        prefix_disk: crate::gateway::build_prefix_disk_index(&app_config),
        adaptive_chunk_tuner: crate::gateway::AdaptiveChunkTuner::default_arc(),
        peer_http: peer_clients.clone(),
        admission_health: Default::default(),
        runtime: node_runtime.clone(),
        cycle_config: crate::cycle::CycleConfig {
            enabled: cycle_detection_enabled,
            ..crate::cycle::CycleConfig::default()
        },
        audit,
        plugins: plugins.clone(),
        pom_core_update: pom_core_update.clone(),
        database: database.clone(),
        gateway_port,
        auth: auth_state,
        admin_trust,
        authority_signing_key,
        authority_principal,
        admin_sessions: Arc::new(tokio::sync::Mutex::new(Default::default())),
        transfers: Default::default(),
        reply_inbox: crate::direct::ReplyInbox::new(),
        // `start_p2p_stack` always hands back a working sender, even when
        // `Discovery::new` itself failed - sending on it then just fails
        // fast with a closed-channel error, the same already-handled
        // "no tunnel available" outcome `send_frame_via_tunnel` treats as.
        tunnel_tx: Some(tunnel_tx),
        tls_server: tls_server.clone(),
        quic_server: quic_config.clone(),
        quic_port: net.gateway_quic_port,
        cluster_logs,
        network_chat,
        cluster_cache: Arc::new(tokio::sync::Mutex::new(Default::default())),
        model_service: Arc::new(model_provider_backend::ModelService::with_token_store(
            Arc::new(huggingface::HuggingFaceProvider::new_with_token_store(
                hf_token_store.clone(),
            )),
            hf_token_store,
        )),
        pkarr_publisher,
    };

    info!("Gateway running at http://localhost:{}", gateway_port);
    spawn_gateway_companions(&gw_state, &database, net.bind_addr.port());
    let gateway_task = tokio::spawn(async move {
        crate::gateway::start_gateway_with_ready(gw_state, gateway_port, gateway_ready).await
    });

    // Uma fatia anotada é um reinício pedido pela interface, não uma intenção
    // nova: o nó volta servindo o que servia. O journal não é consumido antes
    // de a tentativa ser persistida novamente, porque um crash nativo pode
    // acontecer entre a leitura e o início do carregamento.
    if lastslice::auto_resume_disabled() {
        warn!("retomada automática bloqueada por comando undeploy");
    } else {
        match lastslice::inspect() {
            lastslice::LastSlice::Serving(cfg) => {
                let label = cfg.model_label.clone();
                let (a, b) = (cfg.start_layer, cfg.end_layer);
                match lastslice::resume_attempt(&cfg) {
                    Ok(attempt) if lastslice::should_resume(attempt) => {
                        match node_runtime.apply_after_restart(cfg).await {
                            Ok(()) => info!(
                                "↩️  fatia restaurada após reinício: {label} {a}-{b} (tentativa {attempt})"
                            ),
                            Err(e) => warn!("não deu para restaurar a fatia anotada: {e}"),
                        }
                    }
                    Ok(attempt) => node_runtime.note_unloadable_slice(&cfg, attempt).await,
                    Err(e) => warn!("não deu para restaurar a fatia anotada: {e}"),
                }
            }
            lastslice::LastSlice::Idle(cfg) => {
                let label = cfg.model_label.clone();
                let (a, b) = (cfg.start_layer, cfg.end_layer);
                node_runtime.restore_idle(cfg).await;
                info!("↩️  fatia ociosa restaurada após reinício: {label} {a}-{b}");
            }
            lastslice::LastSlice::Pending { cfg, attempts } => {
                if attempts >= lastslice::MAX_AUTOMATIC_ATTEMPTS {
                    node_runtime.note_unloadable_slice(&cfg, attempts).await;
                } else {
                    match lastslice::resume_attempt(&cfg) {
                        Ok(attempt) if lastslice::should_resume(attempt) => {
                            match node_runtime.apply_after_restart(cfg).await {
                                Ok(()) => {
                                    info!(
                                        "↩️  retomando implantação pendente na tentativa {attempt}"
                                    )
                                }
                                Err(e) => warn!("não deu para retomar a implantação pendente: {e}"),
                            }
                        }
                        Ok(attempt) => node_runtime.note_unloadable_slice(&cfg, attempt).await,
                        Err(e) => warn!("não deu para persistir a retomada da implantação: {e}"),
                    }
                }
            }
            lastslice::LastSlice::Nothing => {}
        }
    }

    gateway_task.await??;
    Ok(())
}

/// Mainline polls its IPv4 UDP socket with a 50 ms read timeout. Windows
/// reports that expected idle result as WSAETIMEDOUT (10060), and Mainline's
/// socket layer logs it at WARN on every poll. The DHT actor has its own
/// thread, so this is not an application retry loop, but the warning storm
/// makes a healthy node look stuck and can flood the local/cluster log queues.
/// Keep genuine Mainline errors visible while hiding only this expected poll
/// timeout.
fn application_log_filter() -> EnvFilter {
    const MAINLINE_SOCKET_TARGET: &str = "mainline::rpc::socket";
    EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        .add_directive(
            format!("{MAINLINE_SOCKET_TARGET}=error")
                .parse()
                .expect("diretiva de log Mainline válida"),
        )
}

#[cfg(any(windows, test))]
fn windows_diagnostic_log_filename() -> &'static str {
    "pom.log"
}

#[cfg(windows)]
enum WindowsDiagnosticLogWriter {
    File(std::fs::File),
    Stderr(std::io::Stderr),
}

#[cfg(windows)]
impl std::io::Write for WindowsDiagnosticLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::File(file) => file.write(bytes),
            Self::Stderr(stderr) => stderr.write(bytes),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::File(file) => file.flush(),
            Self::Stderr(stderr) => stderr.flush(),
        }
    }
}

#[cfg(windows)]
fn windows_diagnostic_log_writer() -> std::sync::Mutex<WindowsDiagnosticLogWriter> {
    use std::fs::OpenOptions;

    let path = config::project_root().join(windows_diagnostic_log_filename());
    let writer = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => WindowsDiagnosticLogWriter::File(file),
        Err(error) => {
            eprintln!(
                "não foi possível abrir o log Windows em {}: {error}",
                path.display()
            );
            WindowsDiagnosticLogWriter::Stderr(std::io::stderr())
        }
    };
    std::sync::Mutex::new(writer)
}

#[cfg(test)]
mod main_tests {
    use super::*;

    #[test]
    fn o_binario_windows_e_um_app_gui_e_o_shell_tem_tray_webview_e_saida() {
        let main_source = include_str!("main.rs");
        let shell_source = include_str!("windows_status.rs");

        assert!(
            main_source.contains("#![cfg_attr(windows, windows_subsystem = \"windows\")]"),
            "o executável Windows não pode alocar um console próprio"
        );
        assert!(
            shell_source.contains("WebViewBuilder"),
            "o shell Windows precisa criar um WebView nativo"
        );
        assert!(
            shell_source.contains("TrayIconBuilder"),
            "o shell Windows precisa instalar um ícone na bandeja"
        );
        assert!(
            shell_source.contains("MenuEvent::set_event_handler"),
            "o shell Windows precisa encaminhar os cliques do menu ao loop nativo"
        );
        assert!(
            shell_source.contains("WindowAction::HideToTray"),
            "fechar a janela deve mantê-la disponível na bandeja"
        );
        assert!(
            shell_source.contains("WindowAction::Quit"),
            "o menu da bandeja precisa ter uma saída terminal"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn menu_bar_chat_url_uses_loopback_and_gateway_port() {
        assert_eq!(gateway_url(8080), "http://127.0.0.1:8080/admin-ui/chat");
        assert_eq!(gateway_url(8090), "http://127.0.0.1:8090/admin-ui/chat");
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn persisted_gateway_port_wins_over_cli_fallback() {
        assert_eq!(select_gateway_port(Some(9123), 8080), 9123);
        assert_eq!(select_gateway_port(None, 8080), 8080);
    }

    /// `--bootstrap` alone must reach the node: a start path that read only
    /// `pom.json` left a NAT'd node pointed at a relay by the CLI undialled.
    #[test]
    fn cli_bootstrap_counts_without_any_in_the_config() {
        assert_eq!(
            merged_bootstrap_nodes(&[], "203.0.113.10:42777"),
            vec!["203.0.113.10:42777"]
        );
    }

    #[test]
    fn bootstrap_merge_keeps_config_first_and_drops_duplicates() {
        assert_eq!(
            merged_bootstrap_nodes(
                &["203.0.113.10:42777".into(), " ".into()],
                "203.0.113.10:42777, 2001:db8::10:42777,,relay.example:42777",
            ),
            vec![
                "203.0.113.10:42777",
                "[2001:db8::10]:42777",
                "relay.example:42777"
            ]
        );
    }

    #[test]
    fn relay_peers_merge_config_first_without_duplicates() {
        assert_eq!(
            merged_relay_peers(&["pk:relay-a".into(), " ".into()], "pk:relay-b, pk:relay-a"),
            vec!["pk:relay-a", "pk:relay-b"]
        );
        assert_eq!(merged_relay_peers(&[], "pk:relay-b"), vec!["pk:relay-b"]);
        assert_eq!(
            merged_relay_peers(&["pk:relay-a".into()], ""),
            vec!["pk:relay-a"]
        );
    }

    /// Toda opção visível da linha de comando tem variável de ambiente, com o
    /// nome derivado da marca e sem repetir. `mut_arg` com id errado entra em
    /// pânico: montar o comando aqui também prova que a tabela bate com `Args`.
    #[test]
    fn every_cli_option_has_its_env_var() {
        let command = cli_command();
        let mut names = std::collections::HashSet::new();
        for arg in command.get_arguments() {
            if arg.get_long().is_none() || arg.is_hide_set() {
                continue;
            }
            let id = arg.get_id().as_str();
            let (_, suffix) = ARG_ENV
                .iter()
                .find(|(arg_id, _)| *arg_id == id)
                .unwrap_or_else(|| panic!("--{id} sem variável de ambiente em ARG_ENV"));
            let env = arg.get_env().expect("variável aplicada por cli_command");
            assert_eq!(env.to_str(), Some(brand::env_var(suffix).as_str()));
            assert!(names.insert(env.to_owned()), "variável repetida: {env:?}");
        }
    }

    /// As portas vindas do ambiente vencem o `pom.json`; da linha de comando,
    /// não - a gravada continua valendo.
    #[test]
    fn env_ports_override_the_config_and_cli_ports_do_not() {
        let config: config::Config =
            serde_json::from_str(r#"{"profile":"node","p2p_port":42777,"gateway_port":8080}"#)
                .unwrap();

        let mut from_cli = config.clone();
        let cli = Args::parse_from(["pom", "--port", "43000", "--gateway-port", "9000"]);
        apply_env_overrides(&mut from_cli, &cli);
        assert_eq!((from_cli.p2p_port, from_cli.gateway_port), (42777, 8080));

        let mut from_env = config;
        let mut env = cli;
        env.port_from_env = true;
        env.gateway_port_from_env = true;
        apply_env_overrides(&mut from_env, &env);
        assert_eq!((from_env.p2p_port, from_env.gateway_port), (43000, 9000));
    }

    #[test]
    fn comma_list_trims_and_drops_empty_entries() {
        assert_eq!(comma_list(" pk:a, ,pk:b,"), vec!["pk:a", "pk:b"]);
        assert!(comma_list("").is_empty());
    }

    #[test]
    fn diagnostico_do_app_gui_tem_arquivo_estavel() {
        assert_eq!(windows_diagnostic_log_filename(), "pom.log");
    }

    #[test]
    fn the_gateway_starts_before_restoring_a_slice() {
        let source = include_str!("main.rs");
        let gateway = source
            .find("Bind the gateway before restoring a persisted slice")
            .expect("o arranque precisa iniciar o gateway");
        let restore = source[gateway..]
            .find("match lastslice::inspect()")
            .map(|offset| gateway + offset)
            .expect("o arranque precisa tratar a fatia persistida");
        assert!(
            gateway < restore,
            "a restauração da fatia não pode atrasar o bind do gateway"
        );
    }

    /// Todo nó anuncia a porta do Ollama da URL configurada, sem flag nenhuma:
    /// quando o anúncio dependia de uma flag, um nó sem ela servia o Ollama
    /// local sem que nenhum peer soubesse.
    #[test]
    fn every_node_advertises_the_ollama_port() {
        let args = Args::parse_from(["pom"]);
        assert_eq!(advertised_ollama_port(&args.ollama_url), Some(11434));
    }

    /// Sem nenhuma flag de CLI, o detector de ciclos fica desligado - o padrão
    /// pedido pelo produto. `--cycle-detection` é o interruptor explícito para
    /// ligá-lo.
    #[test]
    fn cycle_detection_is_off_by_default_from_cli_args() {
        let args = Args::parse_from(["pom"]);
        assert!(!args.cycle_detection);

        let with_flag = Args::parse_from(["pom", "--cycle-detection"]);
        assert!(with_flag.cycle_detection);
    }

    #[test]
    fn runtime_build_commit_overrides_the_compiled_fallback() {
        assert_eq!(
            build_commit_from(Some(" 97ec1e4 "), "desconhecido"),
            "97ec1e4"
        );
        assert_eq!(build_commit_from(Some("  "), "2a456c8"), "2a456c8");
        assert_eq!(build_commit_from(None, "2a456c8"), "2a456c8");
    }

    #[test]
    fn pom_desenv_accepts_only_exact_one() {
        assert!(pom_desenv_enabled_from(Some("1")));
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("01"),
            Some("1 "),
        ] {
            assert!(!pom_desenv_enabled_from(value));
        }
    }

    /// `--advertise-addr` é mais específico que `--public-addr`, e os dois
    /// vencem o que já estava salvo em `pom.json`.
    #[test]
    fn advertise_addr_wins_over_public_addr_wins_over_config() {
        assert_eq!(
            resolve_advertise_addr("1.1.1.1:1", "2.2.2.2:2", Some("3.3.3.3:3")),
            Some("1.1.1.1:1".to_string())
        );
        assert_eq!(
            resolve_advertise_addr("", "2.2.2.2:2", Some("3.3.3.3:3")),
            Some("2.2.2.2:2".to_string())
        );
        assert_eq!(
            resolve_advertise_addr("", "", Some("3.3.3.3:3")),
            Some("3.3.3.3:3".to_string())
        );
        assert_eq!(resolve_advertise_addr("", "", None), None);
    }

    #[test]
    fn pkarr_advertise_addr_preserves_ipv6_and_port() {
        assert_eq!(
            pkarr_multiaddr_for_advertise(Some("[2001:db8::1]:42777"), None, 42777),
            "/ip6/2001:db8::1/tcp/52777"
        );
        assert_eq!(
            pkarr_multiaddr_for_advertise(Some("192.0.2.1:42777"), None, 42777),
            "/ip4/192.0.2.1/tcp/52777"
        );
        assert_eq!(
            pkarr_multiaddr_for_advertise(Some("2001:db8::1"), None, 42777),
            "/ip6/2001:db8::1/tcp/52777"
        );
    }

    #[test]
    fn mainline_socket_timeouts_are_not_application_warnings() {
        assert!(
            application_log_filter()
                .to_string()
                .contains("mainline::rpc::socket=error"),
            "timeouts de leitura do socket DHT devem ser silenciosos no log da aplicação"
        );
    }
}
