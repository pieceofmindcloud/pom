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
pub(crate) const POM_DEV_MODE_ENV: &str = "POM_DEV_MODE";
static POM_DEV_MODE_ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

pub(crate) fn pom_dev_mode_enabled_from(value: Option<&str>) -> bool {
    value == Some("1")
}

pub(crate) fn initialize_pom_dev_mode() -> bool {
    *POM_DEV_MODE_ENABLED
        .get_or_init(|| pom_dev_mode_enabled_from(std::env::var(POM_DEV_MODE_ENV).ok().as_deref()))
}

pub(crate) fn pom_dev_mode_enabled() -> bool {
    initialize_pom_dev_mode()
}

#[path = "network/addrs.rs"]
pub mod addrs;
#[path = "backend/admin.rs"]
pub(crate) mod admin;
#[path = "backend/auth.rs"]
mod auth;
#[path = "backend/user_session.rs"]
mod user_session;
#[path = "backend/user_keys.rs"]
mod user_keys;
// `brand` and the llama.cpp cluster moved out to their own crates (`crates/brand`
// and `crates/llama-engine`) so that editing backend/network/routing does not
// rebuild them. Re-exported below so that `crate::brand::…`, `crate::llm::…`
// etc. keep resolving across all of `node` without touching the ~40 files that
// use them. See `docs/subsystems/build-and-run.md`.
use ::brand;
pub use llama_engine::{collective, gguf, kv_budget, llm, moe, mtp, multipart, sysmon, wire};
#[path = "admin_trust.rs"]
mod admin_trust;
mod membership;
mod relay_pass;
#[path = "backend/capabilities.rs"]
mod capabilities;
#[path = "models/catalog.rs"]
mod catalog;
#[path = "models/image_catalog.rs"]
mod image_catalog;
mod chat_image;
mod components;
mod image_plan;
mod image_records;
#[path = "network/cluster_logs.rs"]
mod cluster_logs;
#[path = "network/cluster_logs_policy.rs"]
mod cluster_logs_policy;
#[path = "utils/config.rs"]
mod config;
#[cfg(test)]
#[path = "backend/continuation_lab.rs"]
mod continuation_lab;
#[path = "utils/crash.rs"]
mod crash;
#[path = "backend/db.rs"]
mod db;
#[path = "backend/decisions.rs"]
mod decisions;
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
#[path = "network/anon_tunnel.rs"]
mod anon_tunnel;
#[path = "network/discovery.rs"]
mod discovery;
#[path = "enterprise_p2p.rs"]
mod enterprise_p2p;
#[path = "inference/executor.rs"]
mod executor;
#[path = "routing/fastest.rs"]
pub mod fastest;
#[path = "backend/gateway/mod.rs"]
mod gateway;
#[path = "utils/geo.rs"]
mod geo;
#[path = "models/huggingface.rs"]
mod huggingface;
#[path = "network/identity.rs"]
mod identity;
#[path = "inference/infer_server/mod.rs"]
mod infer_server;
#[path = "network/infer_tunnel.rs"]
mod infer_tunnel;
#[path = "initial_ports.rs"]
mod initial_ports;
#[path = "models/lastslice.rs"]
mod lastslice;
#[path = "inference/link.rs"]
mod link;
#[cfg(test)]
#[path = "inference/link_lab.rs"]
mod link_lab;
#[cfg(all(target_os = "linux", feature = "desktop"))]
mod linux_window;
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
#[path = "network/network_lobby.rs"]
mod network_lobby;
#[path = "models/ollama.rs"]
mod ollama;
#[path = "backend/openai.rs"]
mod openai;
#[path = "network/peer_client.rs"]
pub mod peer_client;
#[path = "routing/pipeline.rs"]
mod pipeline;
#[path = "network/p2p.rs"]
mod p2p;
#[path = "network/pkarr_node.rs"]
mod pkarr_node;
#[path = "models/plan.rs"]
mod plan;
mod plugin_dev;
#[path = "inference/engine_telemetry.rs"]
mod engine_telemetry;
#[path = "inference/plugin_engine.rs"]
mod plugin_engine;
#[path = "inference/plugin_session.rs"]
mod plugin_session;
mod plugin_github;
mod plugin_host_api;
#[path = "plugin_host.rs"]
mod plugin_host;
mod plugin_proxy;
mod plugin_services;
#[path = "inference/plugin_stage_link.rs"]
mod plugin_stage_link;
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
mod process_tree;
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
mod rag;
#[path = "inference/relay.rs"]
mod relay;
#[path = "backend/quota.rs"]
mod quota;
#[path = "backend/reqlog.rs"]
mod reqlog;
#[path = "utils/instance_lock.rs"]
mod instance_lock;
#[path = "utils/respawn.rs"]
mod respawn;
#[path = "utils/console.rs"]
mod console;
#[path = "utils/external_links.rs"]
#[cfg_attr(not(any(windows, feature = "desktop", test)), allow(dead_code))]
mod external_links;
#[path = "network/route.rs"]
pub mod route;
#[path = "network/route_plan.rs"]
pub mod route_plan;
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
#[path = "backend/tool_call.rs"]
mod tool_call;
#[path = "network/web_ca.rs"]
pub mod web_ca;
mod trust_store;
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
use tracing::{debug, info, warn};
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
    about = "Distributed LLM inference across machines, with no central server",
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
    #[arg(long, short = 'p', default_value_t = network::DISCOVERY_PORT)]
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

    /// Ollama base URL (model resolution and the `ollama_available` field of /health)
    #[arg(long, default_value = "http://localhost:11434")]
    ollama_url: String,

    /// HTTP port for the gateway/API server (served by every node)
    #[arg(long, default_value_t = initial_ports::DEFAULT_GATEWAY_PORT)]
    gateway_port: u16,

    /// Allow remote HTTP for the browser UI and client API behind an HTTPS proxy.
    /// Does not disable TLS or mutual authentication between nodes.
    #[arg(long)]
    no_tls: bool,

    /// Pkarr public keys of known peers to dial on startup (comma-separated, format: pk:zbase32...).
    #[arg(long, default_value = "")]
    pkarr_peers: String,

    /// External address to publish to the pkarr DHT and to advertise to
    /// peers as this node's own address (HOST:PORT) — same effect as
    /// `--advertise-addr`, which wins if both are set. PORT here is the UDP
    /// port; the endpoint's UDP port is UDP+10000. Without either, peers
    /// reached over the internet fall back to the public address the relays
    /// observed (the NAT report), if any; failing that, this node's own local
    /// network interfaces (usually wrong for a NAT'd node).
    #[arg(long, default_value = "")]
    public_addr: String,

    /// URLs of dedicated relays to use for NAT traversal (comma-separated,
    /// `https://relay.example.org` or `http://203.0.113.7:52777` for a node
    /// running `--serve-relay`). Added to `relay_peers` in pom.json.
    #[arg(long, default_value = "")]
    relay_peers: String,

    /// Relay for any node of any network: a public relay (requires a public
    /// IP). Same as `relay.everyone` in the config file. Without it, a node
    /// with a public address still relays for the members of its own cluster.
    #[arg(long)]
    serve_relay: bool,

    /// Do not relay for the members of this node's cluster either
    /// (`relay.cluster: false`).
    #[arg(long)]
    no_cluster_relay: bool,

    /// Do not let file transfers of the cluster cross this node when it
    /// relays (`relay.transfers: false`).
    #[arg(long)]
    no_relay_transfers: bool,

    /// Do not use n0's public relays: only the dedicated relays
    /// (`relay_peers`) and the member relays of the cluster.
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

    /// Turns pkarr off: publishes no record to the Mainline DHT and resolves no
    /// `pk:` keys (`--pkarr-peers`, trust chain principals). For a network
    /// without internet access, where the DHT is unreachable: paths stay on
    /// UDP/mDNS on the LAN and on `bootstrap_nodes` between segments.
    #[arg(long)]
    no_pkarr: bool,

    /// Linux: also install the local web CA in the system trust store, which
    /// every user of the machine reads (asks for root through `pkexec`).
    /// Without it the CA goes only to this user's browser databases.
    #[arg(long)]
    trust_ca_system_wide: bool,

    /// `--port` came from the environment variable, not from the command line.
    /// Filled in by `parse_args`; see `apply_env_overrides`.
    #[arg(skip)]
    port_from_env: bool,

    /// `--gateway-port` came from the environment variable. See `port_from_env`.
    #[arg(skip)]
    gateway_port_from_env: bool,

    /// `--port` was given explicitly on the command line.
    #[arg(skip)]
    port_from_cli: bool,

    /// `--gateway-port` was given explicitly on the command line.
    #[arg(skip)]
    gateway_port_from_cli: bool,
}

/// Environment variable of each command line option, by its clap id.
///
/// Every option has its own: the command line beats the variable, which beats
/// the default (clap's order). The name comes from `brand::env_var`, so it
/// cannot go in the derive's `env` attribute, which requires a literal.
const ARG_ENV: &[(&str, &str)] = &[
    ("port", "PORT"),
    ("bootstrap", "BOOTSTRAP"),
    ("advertise_addr", "ADVERTISE_ADDR"),
    ("ollama_url", "OLLAMA_URL"),
    ("gateway_port", "GATEWAY_PORT"),
    ("no_tls", "NO_TLS"),
    ("pkarr_peers", "PKARR_PEERS"),
    ("public_addr", "PUBLIC_ADDR"),
    ("relay_peers", "RELAY_PEERS"),
    ("serve_relay", "SERVE_RELAY"),
    ("no_cluster_relay", "NO_CLUSTER_RELAY"),
    ("no_relay_transfers", "NO_RELAY_TRANSFERS"),
    ("no_default_relays", "NO_DEFAULT_RELAYS"),
    ("no_discovery", "NO_DISCOVERY"),
    ("no_pkarr", "NO_PKARR"),
    ("trust_ca_system_wide", "TRUST_CA_SYSTEM_WIDE"),
];

/// The command line with the variables of `ARG_ENV`. The boolean flags read the
/// variable as `1/0`, `true/false`, `yes/no` or `on/off`, like the other
/// boolean variables of the project.
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
    let from_cli =
        |id: &str| matches.value_source(id) == Some(clap::parser::ValueSource::CommandLine);
    let mut args = <Args as clap::FromArgMatches>::from_arg_matches(&matches)
        .unwrap_or_else(|error| error.exit());
    args.port_from_env = from_env("port");
    args.gateway_port_from_env = from_env("gateway_port");
    args.port_from_cli = from_cli("port");
    args.gateway_port_from_cli = from_cli("gateway_port");
    args
}

/// The environment variable beats `pom.json`.
///
/// For the other options this already follows from the normal precedence,
/// because the command line beats or adds to the config. `--port` and
/// `--gateway-port` are the exception: on the command line they only apply to
/// setup mode, and the saved port wins - it is what keeps a port change made
/// through the interface after the respawn, which repeats the argv. Coming
/// from the environment, they beat the config.
fn apply_env_overrides(config: &mut config::Config, args: &Args) {
    if args.port_from_env {
        config.p2p_port = args.port;
    }
    if args.gateway_port_from_env {
        config.gateway_port = args.gateway_port;
    }
}

/// The node is a Tokio service, and the main thread is its own on every
/// platform but one.
///
/// On macOS the main thread belongs to AppKit: `NSApplication` only accepts
/// being driven from it, and it is the one that serves the status bar item.
/// On Windows, Tao and the tray have the same UI thread requirement. That is
/// why the service is moved to a background thread with its own runtime,
/// and the main one stays in the native event loop. On the other platforms none
/// of this exists and the runtime is built right here, so that whoever runs the
/// binary on a Linux server gains no dependency on a graphical interface.
fn main() -> Result<()> {
    let result = run_main();
    #[cfg(windows)]
    if let Err(error) = &result {
        windows_status::report_startup_failure(error);
    }
    result
}

fn run_main() -> Result<()> {
    // First of all: a native crash inside llama.cpp leaves no line in the
    // node's own log otherwise.
    crash::install();
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

    let pom_dev_mode = initialize_pom_dev_mode();
    info!("POM_DEV_MODE enabled: {pom_dev_mode}");

    // `llama-engine` knows neither the project layout nor the network: inject the
    // root that `sysmon` measures and the TP collective state functions that `llm`
    // queries from inside the decode (`tp.rs` stays in `node`, see `tp_probe`).
    sysmon::set_disk_root(config::project_root());
    tp::install_probe();

    let mut args = parse_args();
    trust_store::set_system_wide(args.trust_ca_system_wide);
    #[cfg(windows)]
    if let Some(port) = args.windows_firewall_helper {
        return windows_firewall::run_elevated_helper(port);
    }
    if let Some(command) = args.command.as_ref() {
        return run_command(command);
    }

    // Before any port is probed: a leftover POM on this root holds the ports
    // too, and moving away from it would start a second node with its identity.
    let mut instance = if is_windows_supervisor() {
        None
    } else {
        match instance_lock::acquire(instance_lock::RESPAWN_GRACE)
            .context("could not lock the POM home")?
        {
            instance_lock::Acquired::Lock(lock) => Some(lock),
            instance_lock::Acquired::HeldBy { gateway_port } => {
                return already_running(gateway_port)
            }
        }
    };

    if !config::Config::exists() {
        let gateway_was_default = args.gateway_port == initial_ports::DEFAULT_GATEWAY_PORT
            && !args.gateway_port_from_env
            && !args.gateway_port_from_cli;
        let p2p_was_default =
            args.port == network::DISCOVERY_PORT && !args.port_from_env && !args.port_from_cli;
        let selected = initial_ports::select(
            args.gateway_port,
            args.port,
            gateway_was_default,
            p2p_was_default,
        )?;
        if selected.gateway != args.gateway_port || selected.p2p != args.port {
            info!(
                "first boot found occupied default ports; selected gateway {} and P2P {}",
                selected.gateway, selected.p2p
            );
            args.gateway_port = selected.gateway;
            args.port = selected.p2p;
        }
    } else if DESKTOP {
        move_busy_saved_ports(&args)?;
    }
    let gateway_port = configured_gateway_port(&args);
    if let Some(lock) = instance.as_mut() {
        if let Err(error) = lock.record_gateway_port(gateway_port) {
            warn!("could not record the interface port in the instance lock: {error}");
        }
    }

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
            move || service(Some(gateway_ready)),
            gateway_ready_rx,
        );
    }

    // Without a display (a container, an SSH session) the AppImage runs as a server.
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    if linux_window::available() {
        let (gateway_ready, gateway_ready_rx) = std::sync::mpsc::sync_channel(1);
        return linux_window::run(
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
            info!("automatic deployment resume blocked");
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
            "the administrative chain already exists in {}; use --replace only for an explicit manual re-anchoring",
            destination.display()
        );
    }

    let (document, local_root) = if let Some(source) = from_file {
        let source_store = crate::admin_trust::AdminTrust::load(source).with_context(|| {
            format!(
                "could not read the transported chain in {}",
                source.display()
            )
        })?;
        let document = source_store.document();
        if let Some(expected_cluster) = cluster_id {
            if expected_cluster != document.anchor.cluster_id {
                bail!(
                    "the cluster of the transported chain is {}, not {}",
                    document.anchor.cluster_id,
                    expected_cluster
                );
            }
        }
        (document, false)
    } else {
        let cluster_id = cluster_id
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("--cluster-id is required when creating the local root")
            })?;
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
            setup: None,
            setup_signature: String::new(),
        };
        (document, true)
    };

    let expected = crate::admin_trust::anchor_fingerprint(&document);
    println!("authority fingerprint: {expected}");
    // The same check the interface makes. Two would be two policies.
    if !fingerprint.is_some_and(|typed| crate::admin_trust::fingerprint_matches(typed, &expected)) {
        bail!(
            "the fingerprint was not confirmed; check the line above and run again with --fingerprint {expected} --confirm"
        );
    }
    if !confirm {
        bail!("the fingerprint needs explicit confirmation; run again with --confirm");
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
        "administrative anchor installed in {}",
        destination.display()
    );
    Ok(())
}

/// Whether the gateway serves TLS, so the native windows load it over
/// `https://`. Set off at startup only when the certificate could not be made.
static WEB_TLS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// The scheme the native windows use for the local interface: `https`, with
/// the certificate the window pins (`web_ca::is_local_web_certificate`) or the
/// system trusts through the local CA.
fn local_scheme() -> &'static str {
    if WEB_TLS.load(std::sync::atomic::Ordering::Relaxed) {
        "https"
    } else {
        "http"
    }
}

#[cfg(any(target_os = "macos", target_os = "windows", all(target_os = "linux", feature = "desktop")))]
fn gateway_url(port: u16) -> String {
    format!("{}://127.0.0.1:{port}/admin-ui/chat", local_scheme())
}

fn select_gateway_port(config_port: Option<u16>, cli_port: u16) -> u16 {
    config_port.unwrap_or(cli_port)
}

fn configured_gateway_port(args: &Args) -> u16 {
    if args.gateway_port_from_env {
        return args.gateway_port;
    }
    let persisted = config::Config::load()
        .ok()
        .map(|config| config.gateway_port);
    select_gateway_port(persisted, args.gateway_port)
}

/// A build with a native window, used by people rather than by a cluster.
const DESKTOP: bool = cfg!(any(
    target_os = "macos",
    target_os = "windows",
    all(target_os = "linux", feature = "desktop")
));

/// The Windows supervisor only relaunches the child, which claims the root.
fn is_windows_supervisor() -> bool {
    #[cfg(windows)]
    {
        std::env::var_os(windows_status::SUPERVISED_CHILD_ENV).is_none()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Another process already serves this root.
///
/// The Linux window opens on it; elsewhere, and on a server, the second launch
/// stops with the port to open.
fn already_running(gateway_port: Option<u16>) -> Result<()> {
    let root = config::project_root();
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    if let Some(port) = gateway_port.filter(|_| linux_window::available()) {
        info!(
            "POM is already running from {}; opening its interface on port {port}",
            root.display()
        );
        return linux_window::show_running(port);
    }
    let location = gateway_port
        .map(|port| format!(", interface at {}://127.0.0.1:{port}/", local_scheme()))
        .unwrap_or_default();
    anyhow::bail!(
        "POM is already running from {}{location}. Close it first, or give this node its own {}",
        root.display(),
        brand::env_var("HOME")
    )
}

/// On a desktop, a saved port another program holds moves to the next free one.
///
/// A desktop user does not choose ports, so failing on a busy one only leaves
/// POM unable to start. A server keeps failing instead: its ports are part of
/// the cluster's configuration. A port set through the environment is never
/// moved. The new ports are saved in `pom.json`, so the rest of the startup,
/// the window and the next start all use them.
fn move_busy_saved_ports(args: &Args) -> Result<()> {
    let Ok(mut saved) = config::Config::load() else {
        // The full startup reports the unreadable config.
        return Ok(());
    };
    let mut effective = saved.clone();
    apply_env_overrides(&mut effective, args);
    let selected = initial_ports::select(
        effective.gateway_port,
        effective.p2p_port,
        !args.gateway_port_from_env,
        !args.port_from_env,
    )?;
    if !moved_ports(&mut saved, args, selected) {
        return Ok(());
    }
    warn!(
        "the saved ports are in use (gateway {}, P2P {}); moved to gateway {} and P2P {}",
        effective.gateway_port, effective.p2p_port, selected.gateway, selected.p2p
    );
    saved.save().context("could not save the new ports")
}

/// Writes into `saved` the selected ports that did not come from the
/// environment, and tells whether any changed.
fn moved_ports(saved: &mut config::Config, args: &Args, selected: initial_ports::InitialPorts) -> bool {
    let mut changed = false;
    if !args.gateway_port_from_env && saved.gateway_port != selected.gateway {
        saved.gateway_port = selected.gateway;
        // The probe checked the inference port that follows the gateway.
        saved.inference_port = None;
        changed = true;
    }
    if !args.port_from_env && saved.p2p_port != selected.p2p {
        saved.p2p_port = selected.p2p;
        changed = true;
    }
    changed
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

    // A machine without a network makes a local one and goes straight to the
    // interface: admin, named after the host, discovery off. Joining someone
    // else's network comes later, from the Networks screen. `setup_pending`
    // marks a pom.json that is only a preference shell, so checking only for
    // file existence would boot a network that is gone. There is no other way
    // to start: the setup wizard is not reachable, and a local network that
    // cannot be created is a startup error.
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
        // Only a machine that never had a configuration is walked through
        // the first steps; one that just left a network is not new.
        let first_run = existing_config.is_none();
        let base = existing_config
            .take()
            .unwrap_or_else(|| blank_config(args.gateway_port, args.port));
        let mut created = webui::bootstrap_local_network(base, &database)
            .await
            .context("could not create the local network")?;
        if first_run {
            created.first_run_guide = Some(Vec::new());
            created.save()?;
        }
        info!(
            "No network configured - created the local network \"{}\"",
            created.network.public_name
        );
        apply_env_overrides(&mut created, &args);
        existing_config = Some(created);
    }

    // ── Load existing config ─────────────────────────────────────────────────
    let mut app_config = existing_config.expect("config already loaded before the full mode");
    if app_config.connection_key.trim().is_empty() {
        app_config.connection_key = crate::webui::fresh_connection_key();
        app_config.save()?;
        info!("Generated a local admin credential for the configured node");
    }
    for (dir, error) in app_config.create_default_storage_dirs() {
        warn!(
            "could not create the default folder {}: {error}",
            dir.display()
        );
    }
    log_capture.set_min_level(app_config.cluster_logs_min_level);
    let cluster_logs = cluster_logs::ClusterLogs::new(log_capture, log_receiver);
    let audit = reqlog::AuditLog::new(database.clone());
    app_config.api_keys = database.api_keys()?;
    if let Err(error) = database.save_network_settings(&app_config.network) {
        warn!("could not store the network details in the database: {error:#}");
    }
    app_config.thermal = database
        .thermal_policy()?
        .unwrap_or_else(crate::thermal::ThermalPolicy::default);
    app_config
        .thermal
        .validate()
        .context("validate the persisted thermal policy")?;
    app_config.huggingface_token = database.huggingface_token()?;
    config::validate_gateway_port(app_config.gateway_port)?;
    info!("Profile: {}", app_config.profile);

    // Before anything that can load a model: the ggml backends enumerate
    // the devices when they are registered, and after that the list is
    // frozen. Applying the choice later would neither remove nor return a card.
    {
        let detected = sysmon::sample().await.gpus;
        match sysmon::apply_gpu_selection(app_config.gpus.as_deref(), &detected) {
            _ if app_config.gpus.is_none() => {
                info!(
                    "GPUs: all {} available (pom.json does not restrict)",
                    detected.len()
                );
            }
            chosen if chosen.is_empty() => {
                info!("GPUs: none - CPU inference (pom.json carries an empty list)");
            }
            chosen => info!("GPUs: {}", chosen.join(", ")),
        }
    }
    let gateway_port = app_config.gateway_port;

    crate::gateway::ensure_gateway_port_available(app_config.gateway_port, None).await?;

    // The node always comes up without a model: what it serves comes from the
    // deployment made through the interface, restored from the slice journal
    // (`lastslice`) on a restart.
    start_node(
        gateway_port,
        database.clone(),
        app_config.clone(),
        args.ollama_url,
        args.no_tls,
        args.no_discovery,
        args.no_pkarr,
        args.no_default_relays,
        merged_bootstrap_nodes(&app_config.bootstrap_nodes, &args.bootstrap),
        merged_relay_peers(&app_config.relay_peers, &args.relay_peers),
        comma_list(&args.pkarr_peers),
        crate::network::RelayOverrides {
            everyone: args.serve_relay,
            no_cluster: args.no_cluster_relay,
            no_transfers: args.no_relay_transfers,
        },
        args.advertise_addr.clone(),
        args.public_addr.clone(),
        cluster_logs,
        audit,
        gateway_ready,
    )
    .await
}

/// Where the last consumed `POM_FORCE_SETUP` is recorded.
#[allow(dead_code)]
fn forced_setup_marker() -> std::path::PathBuf {
    config::project_root().join(format!(".{}-setup-forced", crate::brand::slug()))
}

/// Decides whether the wizard is forced on this start, once per request.
///
/// The wizard's save restarts the node, and what forces it must not apply again
/// on that restart - otherwise the freshly created configuration would go to the
/// backup and the wizard would come back forever. Two paths, because the
/// restarts are different:
///
/// - `--setup`: the restart re-executes the binary with the same arguments and
///   the same environment, so the `POM_SETUP_FORCED` mark in the environment is enough.
/// - `POM_FORCE_SETUP=<token>`: in Docker process 1 exits on the restart and the
///   container comes up with the compose environment, with no mark at all. The
///   consumed token is kept in a file in the project root; the same token does
///   not force again. `up.sh <cluster> --wizard` (`pom-cluster-config` repository)
///   generates a new token per call.
#[allow(dead_code)]
fn force_setup_if_requested(cli_flag: bool) -> anyhow::Result<()> {
    if cli_flag {
        let consumed = crate::brand::env_var("SETUP_FORCED");
        if std::env::var(&consumed).is_ok_and(|value| value == "1") {
            return Ok(());
        }
        std::env::set_var(&consumed, "1");
        return force_setup_mode();
    }
    let token = crate::brand::env("FORCE_SETUP").unwrap_or_default();
    let token = token.trim();
    if token.is_empty()
        || matches!(
            token.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    {
        return Ok(());
    }
    let marker = forced_setup_marker();
    if std::fs::read_to_string(&marker).is_ok_and(|last| last.trim() == token) {
        return Ok(());
    }
    force_setup_mode()?;
    std::fs::write(&marker, token)?;
    Ok(())
}

/// Stores the current configuration and leaves the node without it, to come up
/// in the wizard. Runs before opening the database.
#[allow(dead_code)]
fn force_setup_mode() -> anyhow::Result<()> {
    WIZARD_FORCED.store(true, std::sync::atomic::Ordering::Relaxed);
    let database = config::database_path();
    let with_suffix = |suffix: &str| {
        let mut name = database.clone().into_os_string();
        name.push(suffix);
        std::path::PathBuf::from(name)
    };
    let candidates = [
        config::Config::config_path(),
        config::admin_trust_path(),
        crate::admin_trust::replay_path_for(&config::admin_trust_path()),
        database.clone(),
        with_suffix("-wal"),
        with_suffix("-shm"),
    ];
    let present: Vec<_> = candidates
        .into_iter()
        .filter(|path| path.exists())
        .collect();
    if present.is_empty() {
        info!("--setup: no configuration to set aside; starting in the wizard");
        return Ok(());
    }
    let backup = config::project_root()
        .join("setup-backups")
        .join(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
    std::fs::create_dir_all(&backup)?;
    for path in &present {
        let Some(name) = path.file_name() else {
            continue;
        };
        std::fs::rename(path, backup.join(name)).map_err(|error| {
            anyhow::anyhow!("--setup: could not set aside {}: {error}", path.display())
        })?;
    }
    warn!(
        "--setup: configuration set aside in {} ({} file(s)); to go back, stop the node and move them back",
        backup.display(),
        present.len()
    );
    Ok(())
}

/// The configuration of a machine that has none yet: what the setup wizard
/// starts from, and what a first launch turns into a local network.
fn blank_config(gateway_port: u16, p2p_port: u16) -> config::Config {
    use crate::config::Config;
    Config {
        retired_tls_required: (),
        saved_at: None,
        profile: crate::profile::NodeProfile::Admin,
        setup_pending: true,
        local_auto_discovery: false,
        // On from the start: a new network uses QUIC between its nodes and has
        // its Chat, and the principal can turn either off in the preferences.
        quic_enabled: true,
        cluster_logs_enabled: false,
        network_chat_enabled: true,
        cluster_logs_min_level: Default::default(),
        // Setup mode does not load a model: the GPU choice only takes effect
        // once `pom.json` exists.
        gpus: None,
        thermal: Default::default(),
        cache_type_k: config::KvCacheType::F16,
        cache_type_v: config::KvCacheType::F16,
        connection_key: String::new(),
        gateway_port,
        admin_exposure: config::AdminExposure::Localhost,
        admin_access_token: None,
        admin_password_default: false,
        p2p_port,
        inference_port: None,
        bootstrap_nodes: vec![],
        relay_peers: vec![],
        relay: Default::default(),
        advertise_addr: None,
        network: Default::default(),
        api_keys: vec![],
        blocked_nodes: vec![],
        // A freshly configured node comes up with the detected libraries; the
        // Models screen is where that list stops being a guess.
        model_dirs: None,
        shard_dir: None,
        workspace_root: None,
        shard_max_gb: crate::storage::DEFAULT_SHARD_MAX_GB,
        resources: Default::default(),
        setup_model: None,
        first_run_guide: None,
        node_name: None,
        huggingface_token: None,
        slot_save_path: None,
        slot_persist_prefix_cache: true,
        slot_auto_max_bytes: None,
        slot_auto_ttl_secs: None,
        cache_ram_mib: None,
    }
}

/// Part of the deprecated wizard (see `start_setup_mode`): nothing sets it.
#[allow(dead_code)]
static WIZARD_FORCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The deprecated setup wizard's server: a router carrying only
/// `/api/setup/*`. **Nothing calls it** - no flag, variable or startup failure
/// leads here, and the interface has no screen for it. The code is kept until
/// the wizard is removed for good; do not wire it back.
#[allow(dead_code)]
async fn start_setup_mode(
    bind_ip: std::net::IpAddr,
    gateway_port: u16,
    p2p_port: u16,
    preserved: Option<config::Config>,
    database: Arc<db::PomDb>,
    gateway_ready: Option<gateway::GatewayReadySender>,
) -> Result<()> {
    use crate::webui;
    use axum::{routing::get, Router};
    use std::net::SocketAddr;

    config::validate_gateway_port(gateway_port)?;
    let setup_p2p_port = preserved
        .as_ref()
        .map_or(p2p_port, |config| config.p2p_port);
    let config = Arc::new(RwLock::new(
        preserved.unwrap_or_else(|| blank_config(gateway_port, setup_p2p_port)),
    ));

    let setup_p2p_port = config.read().await.p2p_port;
    let setup_socket =
        tokio::net::UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], setup_p2p_port)))
            .await
            .with_context(|| format!("could not open local discovery on port {setup_p2p_port}"))?;
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
            "/api/setup/geo/countries",
            get(webui::get_setup_geo_countries),
        )
        .route("/api/setup/geo/cities", get(webui::get_setup_geo_cities))
        .route("/api/setup/geo/nearest", get(webui::get_setup_geo_nearest))
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
        .route(
            "/api/setup/public-networks",
            get(webui::setup_public_networks),
        )
        .route(
            "/api/setup/lobby",
            get(webui::setup_lobby_fetch).post(webui::setup_lobby_post),
        )
        .route("/api/setup/lobby/info", get(webui::setup_lobby_info))
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
        let _ = ready.send(gateway_port);
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

/// Keys this node must reach without knowing an address: those of
/// `--pkarr-peers` and the principals (current and secondary) of the local
/// trust chain. The authority key is the same as the pkarr record's.
fn pkarr_bootstrap_keys(
    admin_trust: &crate::admin_trust::AdminTrust,
    extra: &[String],
) -> Vec<String> {
    let mut keys: Vec<String> = extra.to_vec();
    if admin_trust.is_anchored() {
        let principals =
            std::iter::once(admin_trust.current_primary()).chain(admin_trust.latest_secondary());
        for principal in principals {
            if let Some(uri) = pkarr_node::PkarrNode::public_key_uri_from_hex(&principal.pubkey) {
                if !keys.contains(&uri) {
                    keys.push(uri);
                }
            }
        }
    }
    keys
}

/// Interval of the cold dial by key while the target is not in the table.
const PKARR_BOOTSTRAP_INTERVAL: Duration = Duration::from_secs(30);
/// Longest wait between cold dials of a key that keeps failing.
const PKARR_BOOTSTRAP_CEILING: Duration = Duration::from_secs(10 * 60);

/// Wait before the next cold dial of a key after `misses` dials in a row that
/// left it out of the table: the interval, doubled per miss up to the ceiling.
/// Each dial through a relay costs a circuit from the relay's per-source
/// budget, which every node behind the same NAT shares; redialing a dead
/// target every 30 s from each of them drained it and the relay refused the
/// whole site.
fn pkarr_bootstrap_wait(misses: u32) -> Duration {
    PKARR_BOOTSTRAP_INTERVAL
        .saturating_mul(1u32 << misses.saturating_sub(1).min(8))
        .min(PKARR_BOOTSTRAP_CEILING)
}

/// Resolves through pkarr, and hands to the endpoint, every key in
/// `pkarr_bootstrap_keys` that is not yet a live peer. The record carries the
/// direct addresses and the relays together (`pkarr_node::dial_addr`); iroh
/// tries them all and upgrades a relayed path to a direct one when the NAT
/// allows. A target already in `alive_peers` is not dialed again, and one
/// that stays out backs off (`pkarr_bootstrap_wait`).
fn spawn_pkarr_bootstrap(
    node: Arc<pkarr_node::PkarrNode>,
    dial_tx: mpsc::Sender<p2p::EndpointAddr>,
    net: Arc<Network>,
    admin_trust: Arc<crate::admin_trust::AdminTrust>,
    extra_keys: Vec<String>,
) {
    let own = node.public_key_uri();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PKARR_BOOTSTRAP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Per key: dials in a row that did not bring it into the table, and
        // when the next one may go out.
        let mut backoff: std::collections::HashMap<String, (u32, tokio::time::Instant)> =
            std::collections::HashMap::new();
        loop {
            ticker.tick().await;
            let now = tokio::time::Instant::now();
            let alive: std::collections::HashSet<String> = net
                .alive_peers()
                .await
                .into_iter()
                .filter_map(|peer| peer.endpoint_id)
                .collect();
            for key in pkarr_bootstrap_keys(&admin_trust, &extra_keys) {
                if key == own {
                    continue;
                }
                let Some(peer_id) = pkarr_node::endpoint_id_from_uri(&key) else {
                    continue;
                };
                if alive.contains(&peer_id.to_string()) {
                    backoff.remove(&key);
                    continue;
                }
                let entry = backoff.entry(key.clone()).or_insert((0, now));
                if entry.1 > now {
                    continue;
                }
                entry.0 = entry.0.saturating_add(1);
                entry.1 = now + pkarr_bootstrap_wait(entry.0);
                let addr =
                    tokio::time::timeout(Duration::from_secs(10), node.resolve_dial_addr(&key))
                        .await
                        .ok()
                        .flatten();
                let Some(addr) = addr else {
                    debug!("pkarr: {key} has no dialable record yet");
                    continue;
                };
                if dial_tx.send(addr).await.is_err() {
                    return;
                }
            }
        }
    });
}

/// Everything `P2pStack` needs to bring up pkarr, the endpoint identity,
/// `Network`, and (best-effort) `Discovery`, kept in one function so the
/// ordering constraint below can't be violated from outside it: the endpoint key must be derived
/// *before* `Network::new()`'s builder chain (`.with_endpoint_id(...)`
/// needs the endpoint id string), and `Network` becomes immutable the
/// moment it's wrapped in `Arc`, right after that chain.
struct P2pStackArgs {
    node_id: String,
    pubkey: String,
    /// The node key; see `identity.rs`.
    signing_key: ed25519_dalek::SigningKey,
    p2p_port: u16,
    bootstrap_nodes: Vec<String>,
    inference_port: Option<u16>,
    inference_quic_port: Option<u16>,
    gateway_port: u16,
    gateway_quic_port: Option<u16>,
    profile: crate::profile::NodeProfile,
    node_display_name: String,
    advertise_addr: Option<String>,
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
    /// Dedicated relay URLs (`relay_peers` plus `--relay-peers`).
    relay_urls: Vec<String>,
    relay: config::RelayConfig,
    relay_overrides: crate::network::RelayOverrides,
    /// The operator declared a public address (`--public-addr`): the node
    /// counts as reachable before the NAT report says anything.
    public_addr_configured: bool,
    use_default_relays: bool,
    no_discovery: bool,
    /// `--no-pkarr`: no pkarr client is created.
    no_pkarr: bool,
}

/// What a caller needs after `start_p2p_stack` returns, to keep going with
/// whatever is specific to its own path (layer assignment, `GatewayState`,
/// pkarr dial/publish — none of which belong inside this shared helper).
struct P2pStack {
    net: Arc<Network>,
    authority_signing_key: ed25519_dalek::SigningKey,
    endpoint_id: String,
    cluster_logs: Arc<cluster_logs::ClusterLogs>,
    network_chat: Arc<network_chat::NetworkChat>,
    tunnel_tx: mpsc::Sender<infer_tunnel::TunnelOutbound>,
    pkarr_node: Option<pkarr_node::PkarrNode>,
    assignment_shared: Arc<RwLock<Option<model::LayerAssignment>>>,
    /// `Some` only if `Discovery::new` succeeded - lets a caller dial
    /// pkarr-resolved peers by key on startup. `None` means the endpoint
    /// never came up this run; callers must treat that as "skip it," not
    /// an error (this function itself already logged why).
    disc_dial_tx: Option<mpsc::Sender<p2p::EndpointAddr>>,
}

/// pkarr load → endpoint key → tunnel channel → `Network` (with the endpoint
/// id) → `net.start()` (UDP) → best-effort `Discovery::new` (iroh endpoint)
/// → the 5s periodic gossip re-announce. Never fails on anything
/// endpoint-related - only `Network::new()`'s own `?` (UDP bind failure)
/// propagates. This is deliberate: every caller of this function needs to
/// always come up and serve its gateway, with or without the endpoint.
async fn start_p2p_stack(args: P2pStackArgs) -> Result<P2pStack> {
    let bootstrap_nodes: Vec<String> = args
        .bootstrap_nodes
        .iter()
        .map(|addr| crate::config::normalize_peer_addr(addr))
        .filter(|addr| !addr.is_empty())
        .collect();
    infer_tunnel::set_local_node_id(args.node_id.clone());
    infer_tunnel::set_local_gateway_port(args.gateway_port);
    let cluster_logs = args.cluster_logs.clone();
    let primary_authority_pubkey = args.primary_authority_pubkey.clone();
    let plugins = args.plugins.clone();
    cluster_logs.attach_plugins(plugins.clone());
    cluster_logs.set_identity(args.node_id.clone(), args.node_display_name.clone());
    // Keep the startup behavior of the existing process-log receiver until
    // the authenticated trust document is loaded below; the final receiver
    // state is narrowed to effective admins before the gateway is exposed.
    cluster_logs.set_admin_receiver(args.profile == crate::profile::NodeProfile::Admin);
    cluster_logs.set_enabled(args.cluster_logs_enabled);
    let audit = args.audit.clone();
    audit.set_admin_receiver(args.profile == crate::profile::NodeProfile::Admin);

    // A single key (`.pom-node.key`): `node_id`, endpoint id, pkarr record and
    // authority signature all come from it. See `identity.rs`.
    let pkarr_node = if args.no_pkarr {
        info!("pkarr off (--no-pkarr): no DHT publication or key resolution");
        None
    } else {
        match pkarr_node::PkarrNode::from_signing_key(&args.signing_key) {
            Ok(n) => {
                info!("🔑 Pkarr address: {}", n.public_key_uri());
                Some(n)
            }
            Err(e) => {
                warn!("pkarr init failed (discovery disabled): {}", e);
                None
            }
        }
    };

    let authority_signing_key = args.signing_key.clone();
    let endpoint_key = p2p::secret_key(&authority_signing_key);
    let endpoint_id = endpoint_key.public().to_string();

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
            args.inference_port,
            Some(args.gateway_port),
        )
        .await?
        .with_identity(args.profile, args.node_display_name)
        .with_advertise_addr(args.advertise_addr)
        .with_quic_ports(args.inference_quic_port, args.gateway_quic_port)
        .with_endpoint_id(Some(endpoint_id.clone()))
        .with_relay_gate(
            crate::network::RelayGate::new(
                args.relay.everyone,
                args.relay.cluster,
                args.relay.transfers,
            )
            .with_overrides(args.relay_overrides)
            .with_anonymous(args.relay.anonymous, args.admin_trust.clone()),
        )
        .with_admission(args.auth.clone(), args.admin_trust.clone()),
    );
    net.relay_gate().set_reachable(args.public_addr_configured);
    net.set_policy_principal_pubkey(primary_authority_pubkey);
    // TLS between nodes is always on: set before the probing task, so no
    // URL is ever built as plain HTTP.
    net.set_tls(true);
    net.set_cluster_logs_enabled(args.cluster_logs_enabled);
    net.set_network_chat_enabled(args.network_chat_enabled);
    // Discovery off: the node announces nothing and listens to nobody. It is
    // the same state as a machine taken off the network by its operator, and
    // turning discovery on in Network settings brings it back.
    if args.auth.read().await.network.visibility == config::NetworkVisibility::Local {
        net.detach().await;
    }
    if args.local_auto_discovery {
        if let Err(error) = net.set_local_auto_discovery(true).await {
            warn!("local discovery could not be enabled at startup: {error:#}");
        }
    }

    if args.no_discovery {
        net.detach().await;
        info!("🚫 --no-discovery: the node starts outside the network, without announcing or looking for peers");
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
    network_chat.load_admissions();
    let (audit_tx, audit_rx) = mpsc::channel(reqlog::AUDIT_TRANSPORT_QUEUE_CAPACITY);
    audit.start_replication(net.clone(), audit_tx);

    // Endpoint UDP port (and relay server TCP port) = UDP port + 10 000
    // (e.g. 42777 → 52777). Announce messages go out on gossip every 5 s so
    // internet peers learn our layer assignment without needing a shared
    // signalling server. `Network::new` may have received port 0 and
    // selected an ephemeral UDP port. Discovery must derive its port from the
    // assigned value, not from the unresolved configuration value.
    let (disc_announce_tx, disc_dial_tx) = match Discovery::new(
        net.bind_addr.port(),
        &args.relay_urls,
        args.use_default_relays,
        net.clone(),
        endpoint_key,
        tunnel_rx,
        log_rx,
        cluster_logs.clone(),
        chat_rx,
        network_chat.clone(),
        audit_rx,
        audit,
        plugins,
    )
    .await
    {
        Ok(disc) => {
            let ann_tx = disc.announce_tx.clone();
            let dial_tx = disc.dial_tx.clone();
            net.set_p2p_tunnel_ready(true);
            tokio::spawn(async move { disc.run().await });
            (Some(ann_tx), Some(dial_tx))
        }
        Err(e) => {
            warn!("p2p endpoint disabled ({e:#}). UDP-only mode.");
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
                // Same constructor as the UDP path (`Network::self_announce`):
                // same capabilities, role and hostname, and it **always**
                // publishes, with or without an assignment. A node still without
                // a model has to be seen from the internet the same way it was
                // already seen on the LAN.
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
        endpoint_id,
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

/// Relay URLs from `pom.json` (`relay_peers`) plus `--relay-peers`,
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

/// Background tasks that go with every gateway.
fn spawn_gateway_companions(gw_state: &GatewayState, database: &Arc<db::PomDb>, bind_port: u16) {
    crate::plugin_stage_link::install(gw_state.clone());
    crate::windows_firewall::spawn_startup_check(database.clone(), bind_port);
    crate::webui::spawn_network_settings_ratchet(gw_state.clone());
    crate::plugins::spawn_host_context(gw_state.clone());
    crate::plugins::spawn_serving_watcher(gw_state.clone());
    crate::gateway::spawn_public_directory_republisher(gw_state.clone());
    crate::webui::spawn_public_stats_publisher(gw_state.clone());
    crate::gateway::spawn_directory_board_publisher(gw_state.clone());
    crate::gateway::spawn_directory_publisher(gw_state.clone());
    crate::webui::spawn_admission_poller(gw_state.clone());
    crate::webui::spawn_member_list_keeper(gw_state.clone());
    crate::webui::spawn_token_keeper(gw_state.clone());
    crate::webui::spawn_roster_keeper(gw_state.clone());
    crate::webui::spawn_revocation_keeper(gw_state.clone());
    crate::webui::spawn_key_reclaimer(gw_state.clone());
    crate::admin::spawn_trust_chain_keeper(gw_state.clone());
    crate::webui::spawn_departure_worker(gw_state.clone());
    crate::gateway::spawn_admission_warmup(gw_state.clone());
}

/// Brings up the configured node: network, gateway with all the routes and, if
/// there is one, the slice recorded by a restart. The model never comes from the
/// command line: it is deployed through the interface.
async fn start_node(
    gateway_port: u16,
    database: Arc<db::PomDb>,
    app_config: config::Config,
    ollama_url: String,
    no_tls: bool,
    no_discovery: bool,
    no_pkarr: bool,
    no_default_relays: bool,
    bootstrap_nodes: Vec<String>,
    relay_urls: Vec<String>,
    pkarr_peer_keys: Vec<String>,
    relay_overrides: crate::network::RelayOverrides,
    advertise_addr_cli: String,
    public_addr_cli: String,
    cluster_logs: Arc<cluster_logs::ClusterLogs>,
    audit: Arc<reqlog::AuditLog>,
    gateway_ready: Option<gateway::GatewayReadySender>,
) -> Result<()> {
    use crate::gateway::GatewayState;
    let identity = identity::Identity::load_or_create(&identity::key_path());
    let node_id = identity.node_id.0.clone();
    let pubkey = identity.public_key_hex();
    // Internal keys a node swapped before the principal knew to retire them
    // (the swap of the migration to hash): only the newest of each node counts.
    // Keys never leave the node that minted them: a row another node pushed
    // here is dropped, never honoured.
    match database.forget_foreign_api_keys(&node_id) {
        Ok(0) => {}
        Ok(count) => info!("{count} API key(s) minted by other nodes forgotten"),
        Err(error) => warn!("could not forget the API keys of other nodes: {error:#}"),
    }
    match database.revoke_superseded_node_keys(&node_id) {
        Ok(0) => {}
        // Validation reads the database, not the snapshot in `api_keys`: nothing to reload.
        Ok(count) => info!("{count} replaced internal key(s) revoked"),
        Err(error) => warn!("could not revoke replaced internal keys: {error:#}"),
    }
    let plugins = plugins::PluginManager::runtime_with_database(&node_id, database.clone());
    plugin_engine::install(plugins.clone());

    let node_display_name = match app_config.node_name.clone() {
        Some(name) => name,
        None => sysmon::sample().await.hostname,
    };
    let advertise_addr = resolve_advertise_addr(
        &advertise_addr_cli,
        &public_addr_cli,
        app_config.advertise_addr.as_deref(),
    );
    let inference_port = Some(app_config.inference_port_or_default());
    // QUIC ports derived from this node's `p2p_port`: see `quic::gateway_quic_port`.
    let inference_quic_port =
        inference_port.and_then(|_| crate::quic::inference_quic_port(app_config.p2p_port));
    let gateway_quic_port = crate::quic::gateway_quic_port(app_config.p2p_port);
    peer_client::set_quic_enabled(app_config.quic_enabled);
    let auth_state: Arc<RwLock<config::Config>> = Arc::new(RwLock::new(app_config.clone()));
    let admin_trust = Arc::new(crate::admin_trust::AdminTrust::load_or_empty(
        config::admin_trust_path(),
    ));
    let stack = start_p2p_stack(P2pStackArgs {
        node_id: node_id.clone(),
        pubkey: pubkey.clone(),
        signing_key: identity.signing_key.clone(),
        p2p_port: app_config.p2p_port,
        bootstrap_nodes,
        inference_port,
        inference_quic_port,
        gateway_port,
        gateway_quic_port,
        profile: app_config.profile,
        node_display_name,
        advertise_addr: advertise_addr.clone(),
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
        relay_urls: relay_urls.clone(),
        relay: app_config.relay,
        relay_overrides,
        public_addr_configured: !public_addr_cli.trim().is_empty(),
        use_default_relays: !no_default_relays,
        no_discovery,
        no_pkarr,
    })
    .await?;
    let authority_signing_key = Arc::new(stack.authority_signing_key.clone());
    // The chain's `peer_id` form, never the endpoint id: see
    // `identity::trust_peer_id`.
    let authority_principal = crate::admin_trust::AuthorityPrincipal::from_signing_key(
        node_id.clone(),
        identity.peer_id(),
        &stack.authority_signing_key,
    );
    if app_config.profile == crate::profile::NodeProfile::Admin
        && admin_trust
            .effective_authority(&authority_principal, app_config.profile)
            .is_none()
    {
        warn!(
            "admin profile without a valid administrative grant; the effective capability stays node. To anchor the first principal on this machine: `pom admin-anchor --cluster-id <name>`, check the fingerprint shown and repeat with `--fingerprint <value> --confirm`. For a node whose cluster already has a principal, copy {} from the admin through a channel of your own and run `pom admin-anchor --from <file> --fingerprint <value> --confirm`",
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

    // The pkarr record is always published, with or without a deployment, and
    // republished when the endpoint addresses change: it is how a node that only
    // knows the key (trust document, public directory, `--relay-peers`) finds
    // out where to dial.
    let pkarr_node = stack.pkarr_node.map(Arc::new);
    let pkarr_publisher = pkarr_node.as_ref().map(|node| {
        Arc::new(crate::gateway::PkarrPublishHandle::new(
            node.clone(),
            Some(gateway_port),
            None,
            net.clone(),
            app_config.network.geo_label.clone(),
        ))
    });
    if let Some(publisher) = pkarr_publisher.clone() {
        publisher.spawn_republisher();
    }
    // Cold dial by key: `--pkarr-peers` and the trust chain principals. No
    // IP comes from configuration or a document.
    if let (Some(node), Some(dial_tx)) = (pkarr_node.clone(), disc_dial_tx) {
        spawn_pkarr_bootstrap(
            node,
            dial_tx,
            net.clone(),
            admin_trust.clone(),
            pkarr_peer_keys,
        );
    }
    let local_is_admin = admin_trust
        .effective_authority(&authority_principal, app_config.profile)
        .is_some();
    net.set_policy_principal_pubkey(admin_trust.current_primary().pubkey);
    net.set_trust_version(admin_trust.chain_version());
    net.set_announce_key(authority_signing_key.clone());
    {
        let chat = network_chat.clone();
        net.set_readmission(Arc::new(move |node_id, revoked_at| {
            chat.admitted_role(node_id, Some(revoked_at)).is_some()
        }));
    }
    crate::auth::set_member_oracle(net.clone());
    cluster_logs.set_admin_receiver(local_is_admin);
    audit.set_admin_receiver(local_is_admin);

    // ── TLS between peers ────────────────────────────────────────────────────
    // The node certificate is always derived from its identity. `--no-tls`
    // permits HTTP only for the browser/client surface behind a proxy; peers
    // still require their own TLS client certificate on the shared port.
    // Addresses that go into the browser certificate: this machine's local
    // ones, the one the operator pinned, and the hostname. Without them the
    // browser would complain about the host, and not about the issuer.
    // The same certificate is this node's client certificate on every peer
    // connection, so the other end knows which node is calling.
    if let Err(error) = tls::set_client_identity(&identity) {
        warn!("could not derive the client certificate of this node: {error}");
    }
    let tls_sans = serve::browser_sans(app_config.advertise_addr.as_deref());
    let quic_config = match serve::quic_server_config(&identity) {
        Ok(config) => Some(config),
        Err(error) => {
            warn!("HTTP/3 endpoint unavailable, keeping the TCP fallback: {error}");
            None
        }
    };
    if no_tls {
        warn!("--no-tls/POM_NO_TLS: remote HTTP enabled for the UI and client API; keep the gateway behind an HTTPS proxy on a private network (peer mTLS stays enabled)");
    }
    let tls_server =
        match serve::TlsServer::from_identity(&identity, true, &tls_sans) {
            Ok(t) => Some(t.with_plain_ui(no_tls)),
            Err(e) => {
                warn!("could not derive the certificate of this node, TLS unavailable: {e}");
                WEB_TLS.store(false, std::sync::atomic::Ordering::Relaxed);
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
    peer_clients.apply_cluster_policy(true);
    peer_clients.set_quic(app_config.quic_enabled);
    peer_clients.set_auth_state(auth_state.clone());

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

    // A recorded slice is a restart requested by the interface, not a new
    // intent: the node comes back serving what it served. The journal is not
    // consumed before the attempt is persisted again, because a native crash can
    // happen between the read and the start of the load.
    if lastslice::auto_resume_disabled() {
        warn!("automatic resume blocked by the undeploy command");
    } else {
        match lastslice::inspect() {
            lastslice::LastSlice::Serving(cfg) => {
                let label = cfg.model_label.clone();
                let (a, b) = (cfg.start_layer, cfg.end_layer);
                match lastslice::resume_attempt(&cfg) {
                    Ok(attempt) if lastslice::should_resume(attempt) => {
                        match node_runtime.apply_after_restart(cfg).await {
                            Ok(()) => info!(
                                "↩️  slice restored after restart: {label} {a}-{b} (attempt {attempt})"
                            ),
                            Err(e) => warn!("could not restore the recorded slice: {e}"),
                        }
                    }
                    Ok(attempt) => node_runtime.note_unloadable_slice(&cfg, attempt).await,
                    Err(e) => warn!("could not restore the recorded slice: {e}"),
                }
            }
            lastslice::LastSlice::Idle(cfg) => {
                let label = cfg.model_label.clone();
                let (a, b) = (cfg.start_layer, cfg.end_layer);
                node_runtime.restore_idle(cfg).await;
                info!("↩️  idle slice restored after restart: {label} {a}-{b}");
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
                                        "↩️  resuming the pending deployment on attempt {attempt}"
                                    )
                                }
                                Err(e) => warn!("could not resume the pending deployment: {e}"),
                            }
                        }
                        Ok(attempt) => node_runtime.note_unloadable_slice(&cfg, attempt).await,
                        Err(e) => warn!("could not persist the deployment resume: {e}"),
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
                .expect("valid Mainline log directive"),
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
                "could not open the Windows log in {}: {error}",
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
    fn the_windows_binary_is_a_gui_app_and_the_shell_has_tray_webview_and_exit() {
        let main_source = include_str!("main.rs");
        let shell_source = include_str!("windows_status.rs");

        assert!(
            main_source.contains("#![cfg_attr(windows, windows_subsystem = \"windows\")]"),
            "the Windows executable cannot allocate its own console"
        );
        assert!(
            shell_source.contains("WebViewBuilder"),
            "the Windows shell must create a native WebView"
        );
        assert!(
            shell_source.contains("TrayIconBuilder"),
            "the Windows shell must install a tray icon"
        );
        assert!(
            shell_source.contains("MenuEvent::set_event_handler"),
            "the Windows shell must forward the menu clicks to the native loop"
        );
        assert!(
            shell_source.contains("WindowAction::HideToTray"),
            "closing the window must keep it available in the tray"
        );
        assert!(
            shell_source.contains("WindowAction::Quit"),
            "the tray menu must have a terminal exit"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn menu_bar_chat_url_uses_loopback_and_gateway_port() {
        assert_eq!(gateway_url(8080), "https://127.0.0.1:8080/admin-ui/chat");
        assert_eq!(gateway_url(8090), "https://127.0.0.1:8090/admin-ui/chat");
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn persisted_gateway_port_wins_over_cli_fallback() {
        assert_eq!(select_gateway_port(Some(9123), 8080), 9123);
        assert_eq!(select_gateway_port(None, 8080), 8080);
    }

    #[test]
    fn proxy_http_flag_is_explicit_and_defaults_off() {
        let parse = |args: &[&str]| {
            let matches = cli_command().try_get_matches_from(args).unwrap();
            <Args as clap::FromArgMatches>::from_arg_matches(&matches).unwrap()
        };
        assert!(!parse(&["pom"]).no_tls);
        assert!(parse(&["pom", "--no-tls"]).no_tls);
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
            merged_relay_peers(
                &["https://a.example".into(), " ".into()],
                "https://b.example, https://a.example"
            ),
            vec!["https://a.example", "https://b.example"]
        );
        assert_eq!(
            merged_relay_peers(&[], "https://b.example"),
            vec!["https://b.example"]
        );
        assert_eq!(
            merged_relay_peers(&["https://a.example".into()], ""),
            vec!["https://a.example"]
        );
    }

    /// Every visible command line option has an environment variable, with the
    /// name derived from the brand and without repeats. `mut_arg` with a wrong id
    /// panics: building the command here also proves the table matches `Args`.
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
                .unwrap_or_else(|| panic!("--{id} without an environment variable in ARG_ENV"));
            let env = arg.get_env().expect("variable applied by cli_command");
            assert_eq!(env.to_str(), Some(brand::env_var(suffix).as_str()));
            assert!(names.insert(env.to_owned()), "repeated variable: {env:?}");
        }
    }

    /// The ports coming from the environment beat `pom.json`; from the command
    /// line, they do not - the saved one still applies.
    #[test]
    fn env_ports_override_the_config_and_cli_ports_do_not() {
        let config: config::Config =
            serde_json::from_str(r#"{"profile":"worker","p2p_port":42777,"gateway_port":8080}"#)
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

    /// A moved port is saved; one set through the environment stays out of
    /// `pom.json`, and moving the gateway lets the inference port follow it.
    #[test]
    fn moved_ports_save_only_what_the_environment_did_not_set() {
        let mut saved: config::Config = serde_json::from_str(
            r#"{"profile":"worker","p2p_port":42777,"gateway_port":8080,"inference_port":8181}"#,
        )
        .unwrap();
        let mut args = Args::parse_from(["pom"]);
        args.port_from_env = true;
        let selected = initial_ports::InitialPorts {
            gateway: 8081,
            p2p: 43000,
        };

        assert!(moved_ports(&mut saved, &args, selected));
        assert_eq!((saved.gateway_port, saved.p2p_port), (8081, 42777));
        assert_eq!(saved.inference_port, None);

        let unchanged = initial_ports::InitialPorts {
            gateway: 8081,
            p2p: 42777,
        };
        assert!(!moved_ports(&mut saved, &args, unchanged));
    }

    #[test]
    fn comma_list_trims_and_drops_empty_entries() {
        assert_eq!(comma_list(" pk:a, ,pk:b,"), vec!["pk:a", "pk:b"]);
        assert!(comma_list("").is_empty());
    }

    #[test]
    fn the_gui_app_diagnostic_has_a_stable_file() {
        assert_eq!(windows_diagnostic_log_filename(), "pom.log");
    }

    #[test]
    fn the_gateway_starts_before_restoring_a_slice() {
        let source = include_str!("main.rs");
        let gateway = source
            .find("Bind the gateway before restoring a persisted slice")
            .expect("the startup must start the gateway");
        let restore = source[gateway..]
            .find("match lastslice::inspect()")
            .map(|offset| gateway + offset)
            .expect("the startup must handle the persisted slice");
        assert!(
            gateway < restore,
            "restoring the slice cannot delay the gateway bind"
        );
    }

    #[test]
    fn runtime_build_commit_overrides_the_compiled_fallback() {
        assert_eq!(build_commit_from(Some(" 97ec1e4 "), "unknown"), "97ec1e4");
        assert_eq!(build_commit_from(Some("  "), "2a456c8"), "2a456c8");
        assert_eq!(build_commit_from(None, "2a456c8"), "2a456c8");
    }

    #[test]
    fn pom_dev_mode_accepts_only_exact_one() {
        assert!(pom_dev_mode_enabled_from(Some("1")));
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("01"),
            Some("1 "),
        ] {
            assert!(!pom_dev_mode_enabled_from(value));
        }
    }

    /// `--advertise-addr` is more specific than `--public-addr`, and both
    /// beat what was already saved in `pom.json`.
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
    fn mainline_socket_timeouts_are_not_application_warnings() {
        assert!(
            application_log_filter()
                .to_string()
                .contains("mainline::rpc::socket=error"),
            "DHT socket read timeouts must be silent in the application log"
        );
    }
}

#[cfg(test)]
mod llama_engine_contract {
    /// `llama-engine` does not depend on `brand`: the prefix of the POM GGUF keys
    /// (`pom.<suffix>`) is a constant of its own. This test guarantees it stays
    /// equal to the `slug` of `brand.json`, so that the GGUF reader and the GGUF writer
    /// use the same name.
    #[test]
    fn gguf_prefix_matches_the_brand_slug() {
        assert_eq!(llama_engine::GGUF_PREFIX, brand::slug());
        assert_eq!(
            llama_engine::gguf_key("tp_size"),
            brand::gguf_key("tp_size")
        );
    }

    #[test]
    fn a_key_that_stays_unreachable_is_dialed_less_and_less_often() {
        use std::time::Duration;
        assert_eq!(super::pkarr_bootstrap_wait(1), Duration::from_secs(30));
        assert_eq!(super::pkarr_bootstrap_wait(2), Duration::from_secs(60));
        assert_eq!(super::pkarr_bootstrap_wait(5), Duration::from_secs(480));
        assert_eq!(super::pkarr_bootstrap_wait(6), super::PKARR_BOOTSTRAP_CEILING);
        assert_eq!(super::pkarr_bootstrap_wait(u32::MAX), super::PKARR_BOOTSTRAP_CEILING);
    }
}
