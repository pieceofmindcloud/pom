/// Commit this binary was built from, stamped by `build.rs`.
pub const BUILD_COMMIT: &str = env!("POM_BUILD_COMMIT");

#[path = "network/addrs.rs"]
pub mod addrs;
#[path = "backend/admin.rs"]
mod admin;
#[path = "backend/auth.rs"]
mod auth;
#[path = "utils/brand.rs"]
mod brand;
#[path = "models/catalog.rs"]
mod catalog;
#[path = "inference/collective.rs"]
mod collective;
#[path = "utils/config.rs"]
mod config;
#[path = "backend/deploy.rs"]
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
#[path = "utils/display.rs"]
mod display;
#[path = "backend/gateway.rs"]
mod gateway;
#[path = "models/gguf.rs"]
mod gguf;
#[path = "network/identity.rs"]
mod identity;
#[path = "inference/infer_server.rs"]
mod infer_server;
#[path = "network/infer_tunnel.rs"]
mod infer_tunnel;
#[path = "models/lastslice.rs"]
mod lastslice;
#[path = "inference/llm.rs"]
mod llm;
#[path = "models/memory.rs"]
mod memory;
#[path = "backend/messages.rs"]
mod messages;
#[path = "models/model.rs"]
mod model;
#[path = "inference/mtp.rs"]
mod mtp;
#[path = "network/network.rs"]
mod network;
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
#[path = "models/pom.rs"]
mod pom;
#[path = "inference/prefill.rs"]
mod prefill;
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
#[path = "utils/sysmon.rs"]
mod sysmon;
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
#[path = "backend/webui.rs"]
mod webui;
#[path = "inference/wire.rs"]
mod wire;

use anyhow::Result;
use clap::Parser;
use discovery::Discovery;
use gateway::{start_gateway, GatewayState};
use memory::load_layer_memory;
use model::{calculate_assignment, parse_memory, ModelProfile};
use network::Network;
use ollama::OllamaClient;
use pipeline::LoadTracker;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "pom",
    about = "Inferência distribuída de LLM entre máquinas, sem servidor central",
    long_about = "Runs a node in a distributed LLM inference network.\n\
                  Each node allocates a slice of RAM, loads model layers that fit,\n\
                  and coordinates with peers to cover the full model."
)]
struct Args {
    /// Model to load — built-in name (e.g. gemma3, llama3) or any Ollama model name.
    /// Not required when using --list-models.
    #[arg(long, short = 'm')]
    model: Option<String>,

    /// Memory to allocate for model layers (e.g. 10GB, 4096MB)
    #[arg(long, short = 'M', default_value = "")]
    memory: String,

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

    /// Refresh UI every N seconds
    #[arg(long, default_value = "3")]
    refresh: u64,

    /// List built-in model profiles and Ollama models, then exit
    #[arg(long)]
    list_models: bool,

    /// Ollama base URL (model resolution, /health, e a porta anunciada aos peers)
    #[arg(long, default_value = "http://localhost:11434")]
    ollama_url: String,

    /// Deprecated — every node now serves the HTTP API, e a porta do Ollama é
    /// anunciada com ou sem a flag. Aceita só para não quebrar linha de comando
    /// antiga; o único efeito que resta é marcar este nó como `primary` na
    /// configuração de inferência montada pela linha de comando.
    #[arg(long)]
    gateway: bool,

    /// HTTP port for the gateway/API server (served by every node)
    #[arg(long, default_value = "8080")]
    gateway_port: u16,

    /// Force this node to start at a specific layer (overrides auto gap-filling).
    /// Example: --start-layer 10  means this node owns layers 10..N.
    #[arg(long)]
    start_layer: Option<u32>,

    /// Force the last layer this node owns. Pairs with --start-layer; sem os
    /// dois, a faixa vem do algoritmo que preenche buracos no cluster.
    #[arg(long)]
    end_layer: Option<u32>,

    /// Port for the llama-cpp-python inference server on this node.
    /// When set, pom auto-starts inference_server.py on this port.
    #[arg(long)]
    inference_port: Option<u16>,

    /// GPU layers to offload to VRAM (passed to inference_server.py --n-gpu-layers).
    /// Use 99 to offload all layers. Default 0 = CPU only.
    #[arg(long, default_value = "0")]
    n_gpu_layers: u32,

    /// KV cache context length for the inference server (tokens).
    #[arg(long, default_value = "8192")]
    n_ctx: u32,

    /// Maximum batch size for prompt processing (tokens). Directly controls
    /// logit buffer size: lower = much less RAM (n_batch * vocab * 4 bytes).
    #[arg(long, default_value = "512")]
    n_batch: u32,

    /// Max new tokens to generate per request.
    #[arg(long, default_value = "512")]
    max_new_tokens: u32,

    /// Generations allowed in flight at once. Each generation gets the full
    /// `n_ctx`; the total KV cache reservation grows with this number.
    #[arg(long, default_value = "1")]
    max_sessions: u32,

    /// Keep the KV cache in system RAM instead of GPU VRAM.
    #[arg(long)]
    kv_cache_ram: bool,

    /// Prompt tokens pushed through the pipeline per round trip.
    #[arg(long, default_value = "512")]
    prefill_chunk_tokens: u32,

    /// Predição multi-token pelo bloco `nextn` do modelo (MTP), quando ele
    /// tiver um e este nó servir o modelo inteiro. Não é a "decodificação
    /// especulativa" por lookup — ver `src/inference/mtp.rs`.
    #[arg(long)]
    mtp: bool,

    /// Quantos tokens o MTP rascunha por rodada.
    #[arg(long, default_value = "3")]
    mtp_draft_tokens: u32,

    /// Sampling temperature for inference (0 = greedy).
    #[arg(long, default_value = "0.7")]
    temperature: f32,

    /// Top-p nucleus sampling cutoff.
    #[arg(long, default_value = "0.9")]
    top_p: f32,

    /// Top-k sampling (0 = disabled).
    #[arg(long, default_value = "40")]
    top_k: u32,

    /// Repetition penalty (1.0 = off).
    #[arg(long, default_value = "1.15")]
    repeat_penalty: f32,

    /// How many recent tokens the repetition penalty looks back over.
    #[arg(long, default_value = "64")]
    repeat_last_n: u32,

    /// Disable Qwen3.5 thinking mode (injects empty <think></think> block).
    #[arg(long)]
    no_thinking: bool,

    /// Single-peer mode: loads model without embedding extraction (correct causal attention).
    /// Enable when one node covers all layers.
    #[arg(long)]
    single_peer: bool,

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
}

#[tokio::main]
async fn main() -> Result<()> {
    respawn::remember_argv(std::env::args().collect());
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args = Args::parse();

    info!(
        "Project root: {} (config: {})",
        config::project_root().display(),
        config::Config::config_path().display()
    );

    // ── Setup flow: if no config exists, serve only the setup page ──────────
    if !config::Config::exists() {
        info!("No config found — starting in setup mode");
        start_setup_mode(args.gateway_port).await?;
        return Ok(());
    }

    // ── Load existing config ─────────────────────────────────────────────────
    let app_config = config::Config::load()?;
    info!("Role: {}", app_config.role);

    // Antes de qualquer coisa que possa carregar modelo: os backends do ggml
    // enumeram os dispositivos ao serem registrados, e depois disso a lista está
    // congelada. Aplicar a escolha mais tarde não tiraria nem devolveria placa.
    {
        let detected = sysmon::sample().await.gpus;
        match sysmon::apply_gpu_selection(app_config.gpus.as_deref(), &detected) {
            _ if app_config.gpus.is_none() => {
                info!("GPUs: todas as {} disponíveis (pom.json não restringe)", detected.len());
            }
            escolhidas if escolhidas.is_empty() => {
                info!("GPUs: nenhuma - inferência em CPU (pom.json traz uma lista vazia)");
            }
            escolhidas => info!("GPUs: {}", escolhidas.join(", ")),
        }
    }
    let p2p_port = app_config.p2p_port;
    let gateway_port = app_config.gateway_port;
    let inference_port = Some(
        args.inference_port
            .unwrap_or_else(|| app_config.inference_port_or_default()),
    );

    // ── list-models ─────────────────────────────────────────────────────────
    if args.list_models {
        println!("\nBuilt-in model profiles:\n");
        for m in ModelProfile::known_models() {
            println!(
                "  {:20} {:5} layers  {:.0}MB/layer  {}",
                m.name, m.total_layers, m.memory_per_layer_mb, m.description
            );
        }

        println!("\nOllama models at {}:\n", args.ollama_url);
        let ollama = OllamaClient::new(&args.ollama_url);
        match ollama.list_models().await {
            Ok(models) if models.is_empty() => {
                println!("  (no models found — is Ollama running?)");
            }
            Ok(models) => {
                for m in &models {
                    let size_mb = m.size as f64 / (1024.0 * 1024.0);
                    let profile_tag = if ModelProfile::find(&m.name).is_some() {
                        " ← built-in profile"
                    } else {
                        ""
                    };
                    println!("  {:45} {:>8.0}MB{}", m.name, size_mb, profile_tag);
                }
            }
            Err(e) => println!("  (could not reach Ollama: {})", e),
        }
        println!();
        return Ok(());
    }

    // ── Branch: with model = full stack; without = gateway only (UI + admin) ──
    let model_name = args.model.as_ref().map(|m| m.clone());

    if model_name.is_none() {
        info!("No --model provided — starting gateway without model binding");
        return start_gateway_only(
            gateway_port,
            app_config.clone(),
            args.ollama_url,
            args.no_discovery,
            args.advertise_addr.clone(),
            args.public_addr.clone(),
        )
        .await;
    }

    let model_name = model_name.unwrap();

    // ── require --memory unless inference_port handles model loading ─────────
    if args.memory.is_empty() && inference_port.is_none() {
        error!("--memory is required (e.g. --memory 8GB) unless --inference-port is set.");
        std::process::exit(1);
    }

    display::print_banner();

    // ── Parse memory ─────────────────────────────────────────────────────────
    let memory_mb = if args.memory.is_empty() {
        0.0
    } else {
        match parse_memory(&args.memory) {
            Ok(mb) => mb,
            Err(_) => {
                error!(
                    "Invalid memory format '{}'. Use e.g. 10GB or 4096MB",
                    args.memory
                );
                std::process::exit(1);
            }
        }
    };

    // Priority: (1) built-in profile, (2) Ollama size-based profile,
    // (3) name-based estimate — so any Ollama model always works.

    // If it's a path, use the filename for profile lookup
    let profile_lookup_name = if model_name.contains('/') || model_name.contains('\\') {
        std::path::Path::new(&model_name)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(&model_name)
            .to_string()
    } else {
        model_name.clone()
    };

    let mut profile = if let Some(p) = ModelProfile::find(&profile_lookup_name) {
        p
    } else if let Some(p) = ModelProfile::find(&model_name) {
        p
    } else {
        // Try Ollama to get exact file size for a better estimate
        let ollama = OllamaClient::new(&args.ollama_url);
        match ollama.list_models().await {
            Ok(models) => {
                let lower = profile_lookup_name.to_lowercase();
                if let Some(om) = models.iter().find(|m| {
                    let n = m.name.to_lowercase();
                    n == lower || lower.starts_with(&n) || n.starts_with(&lower)
                }) {
                    info!(
                        "'{}' found in Ollama ({:.0}MB) — using size-based profile",
                        om.name,
                        om.size as f64 / 1_048_576.0
                    );
                    ModelProfile::from_ollama_model(&profile_lookup_name, om.size)
                } else {
                    ModelProfile::guess_from_name(&model_name)
                }
            }
            Err(_) => ModelProfile::guess_from_name(&model_name),
        }
    };

    // Force profile name to be consistent if we found a match
    if let Some(p) = ModelProfile::find(&profile.name) {
        profile.name = p.name;
    }

    info!("Model: {} ({})", profile.name, profile.description);
    info!(
        "Memory allocated: {:.0}MB ({:.1}GB)",
        memory_mb,
        memory_mb / 1024.0
    );

    // ── Generate identity ────────────────────────────────────────────────────
    let identity = identity::Identity::load_or_create(&identity::key_path());
    let node_id = identity.node_id.0.clone();
    let pubkey = identity.public_key_hex();

    let pkarr_peer_keys: Vec<String> = args
        .pkarr_peers
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let relay_peer_keys: Vec<String> = args
        .relay_peers
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // ── Bootstrap nodes: config + CLI extras ─────────────────────────────────
    // Normalised on the way in, config file included: an entry saved before
    // there was any validation is exactly the one that fails at send time.
    let mut bootstrap_nodes: Vec<String> = app_config
        .bootstrap_nodes
        .iter()
        .map(|b| config::normalize_peer_addr(b))
        .filter(|s| !s.is_empty())
        .collect();
    for b in args
        .bootstrap
        .split(',')
        .map(config::normalize_peer_addr)
        .filter(|s| !s.is_empty())
    {
        if !bootstrap_nodes.contains(&b) {
            bootstrap_nodes.push(b);
        }
    }

    // CLI wins over the config file, same as --bootstrap. `--public-addr`
    // used to only reach the pkarr DHT record (below) — an operator who set
    // it to get past NAT for pkarr resolution silently got no benefit for
    // the Announce'd HTTP address, which is the one that actually matters
    // for a peer trying to dial this node directly. Folded in here so one
    // flag does the whole job; `--advertise-addr` stays the more specific
    // override when both happen to be set.
    let advertise_addr = resolve_advertise_addr(
        &args.advertise_addr,
        &args.public_addr,
        app_config.advertise_addr.as_deref(),
    );

    if bootstrap_nodes.is_empty() {
        info!(
            "No bootstrap nodes — will rely on local LAN broadcast (port {})",
            network::DISCOVERY_PORT
        );
        info!("Tip: use --bootstrap 127.0.0.1:<port> to connect multiple local instances");
    } else {
        info!("Bootstrap nodes: {:?}", bootstrap_nodes);
    }

    // ── Determine Ollama port to advertise ───────────────────────────────────
    let ollama_port: Option<u16> = advertised_ollama_port(&args.ollama_url);

    // ── Start P2P network + (best-effort) libp2p discovery ───────────────────
    let node_display_name = match app_config.node_name.clone() {
        Some(name) => name,
        None => sysmon::sample().await.hostname,
    };
    let stack = start_p2p_stack(P2pStackArgs {
        node_id: node_id.clone(),
        pubkey: pubkey.clone(),
        p2p_port,
        bootstrap_nodes: bootstrap_nodes.clone(),
        ollama_port,
        inference_port,
        gateway_port,
        role: app_config.role.to_string(),
        node_display_name,
        advertise_addr,
        tls_required: app_config.tls_required,
        relay_peer_keys: relay_peer_keys.clone(),
        serve_relay: args.serve_relay,
        use_default_relays: !args.no_default_relays,
        no_discovery: args.no_discovery,
    })
    .await?;
    let net = stack.net;
    let assignment_shared = stack.assignment_shared;
    let tunnel_tx = stack.tunnel_tx;
    let pkarr_node = stack.pkarr_node;
    let disc_dial_tx = stack.disc_dial_tx;
    let p2p_port = net.bind_addr.port();
    display::print_node_info(&node_id, &pubkey, p2p_port);

    // ── Compute pkarr multiaddr to publish ───────────────────────────────────
    // Use the actual UDP port when the config requested an ephemeral bind.
    let pkarr_multiaddr = {
        let default_tcp_port = p2p_port.saturating_add(10000);
        if !args.public_addr.is_empty() {
            if let Some((host, port_str)) = args.public_addr.split_once(':') {
                let udp_port = port_str.parse::<u16>().unwrap_or(p2p_port);
                format!("/ip4/{}/tcp/{}", host, udp_port.saturating_add(10000))
            } else {
                format!("/ip4/{}/tcp/{}", args.public_addr, default_tcp_port)
            }
        } else {
            let ip = pkarr_node::detect_local_ip().unwrap_or_else(|| "0.0.0.0".to_string());
            format!("/ip4/{}/tcp/{}", ip, default_tcp_port)
        }
    };

    info!("⏳ Scanning network for existing layer coverage...");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── Calculate layer assignment ───────────────────────────────────────────
    let covered = net.get_covered_layers(&profile.name).await;
    if !covered.is_empty() {
        info!(
            "Network already covers layers: {:?}",
            &covered[..covered.len().min(10)]
        );
    }

    // Layer range, in order of precedence: explicit flags, then the gap-filling
    // algorithm.
    //
    // Havia um degrau no meio - adivinhar a faixa lendo dois números no nome do
    // arquivo (`name_<start>_<end>.gguf`). Ele saiu junto com a convenção: uma
    // parte se chama pelo próprio hash, e um modelo cujo nome termina em dois
    // números não é uma fatia de nada. Quem sabe a faixa é o `.pom`, e quem
    // carrega uma parte solta na mão passa `--start-layer`/`--end-layer`.
    let explicit_range = match (args.start_layer, args.end_layer) {
        (Some(s), Some(e)) => Some((s, e)),
        _ => None,
    };
    let assignment = if let Some((s, e)) = explicit_range {
        info!("Layers {}-{} set explicitly", s, e);
        let actual_count = e - s + 1;
        model::LayerAssignment {
            deploy_id: String::new(),
            model: profile.name.clone(),
            total_layers: profile.total_layers,
            start_layer: s,
            end_layer: e,
            layers_count: actual_count,
            memory_used_mb: actual_count as f64 * profile.memory_per_layer_mb + profile.overhead_mb,
            memory_allocated_mb: memory_mb,
        }
    } else {
        calculate_assignment(&profile, memory_mb, &covered, args.start_layer)
    };

    info!(
        "📦 Assigned layers {}-{} ({} layers, {:.0}MB)",
        assignment.start_layer,
        assignment.end_layer,
        assignment.layers_count,
        assignment.memory_used_mb
    );

    // When inference_server.py manages the model, skip RAM reservation —
    // the Python process owns the model memory.
    let (layer_buf, mem_source) = if inference_port.is_none() {
        let buf = load_layer_memory(
            &profile.name,
            assignment.start_layer,
            assignment.end_layer,
            assignment.total_layers,
            assignment.memory_used_mb,
        );
        let src = buf.source.to_string();
        (Some(buf), src)
    } else {
        (None, "inference_server".to_string())
    };

    display::print_assignment(&assignment, &mem_source);
    let _layer_buf = layer_buf; // keep alive for process lifetime

    {
        let mut a = assignment_shared.write().await;
        *a = Some(assignment.clone());
    }

    net.announce(Some(&assignment)).await?;

    // ── Runtime: owns the model configuration and the inference server ──────
    let auth_config: Arc<tokio::sync::RwLock<config::Config>> =
        Arc::new(tokio::sync::RwLock::new(app_config.clone()));
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
    let tls_server =
        match serve::TlsServer::from_identity(&identity, app_config.tls_required, &tls_sans) {
            Ok(t) => Some(t),
            Err(e) => {
                warn!("não deu para derivar o certificado deste nó, TLS indisponível: {e}");
                None
            }
        };

    let node_runtime = runtime::NodeRuntime::new_with_tls_and_auth(
        net.clone(),
        assignment_shared.clone(),
        tls_server.clone(),
        Some(auth_config.clone()),
    );

    // `--model` na linha de comando é intenção explícita e vence qualquer
    // fatia anotada por um reinício anterior. Este caminho nunca consome o
    // arquivo (só o caminho sem `--model`, mais abaixo, faz isso) — sem esta
    // chamada, um nó que sempre sobe com `--model` deixaria um
    // `*.last-slice.json` órfão no disco para sempre, escrito por uma rota de
    // restart que nunca seria lida de volta.
    let _ = lastslice::take();

    if let Some(port) = inference_port {
        let cfg = runtime::NodeConfig {
            deploy_id: String::new(),
            // Configurado pela linha de comando: não veio de implantação nenhuma.
            model_path: model_name.clone(),
            model_label: profile.name.clone(),
            deployment_name: profile.name.clone(),
            start_layer: assignment.start_layer,
            end_layer: assignment.end_layer,
            total_layers: assignment.total_layers,
            peer_count: if args.single_peer { 1 } else { 2 },
            peer_index: 0,
            primary: args.gateway,
            tp: None,
            inference_port: port,
            n_gpu_layers: args.n_gpu_layers,
            n_ctx: args.n_ctx,
            offload_kqv: !args.kv_cache_ram,
            cache_type_k: config::KvCacheType::F16,
            cache_type_v: config::KvCacheType::F16,
            fit_memory: true,
            n_batch: args.n_batch,
            max_sessions: args.max_sessions,
            batching_enabled: true,
            max_queue_requests: 128,
            batch_window_ms: 4,
            max_batch_sequences: u32::MAX,
            max_batch_tokens: u32::MAX,
            prefill_chunk_tokens: args.prefill_chunk_tokens,
            max_new_tokens: args.max_new_tokens,
            temperature: args.temperature,
            top_p: args.top_p,
            top_k: args.top_k,
            repeat_penalty: args.repeat_penalty,
            repeat_last_n: args.repeat_last_n,
            no_thinking: args.no_thinking,
            single_peer: args.single_peer,
            no_embeddings: false,
            // No CLI flags for these yet — off by default, same as the web
            // UI's kill-switch; turn it on from Configurar → ajustes avançados.
            speculative_decoding_enabled: false,
            speculative_k: 4,
            speculative_lookup_window: 3,
            mtp_enabled: args.mtp,
            mtp_draft_tokens: args.mtp_draft_tokens,
            mtp_min_probability: 0.0,
        };
        if let Err(e) = node_runtime.apply(cfg).await {
            error!("não foi possível iniciar o servidor de inferência: {e:#}");
        }
    }

    // ── HTTP API server — every node serves it ───────────────────────────────
    {
        let peer_clients = Arc::new(peer_client::PeerClients::new(Duration::from_secs(600)));
        peer_clients.apply_cluster_policy(app_config.tls_required);
        peer_clients.set_auth_state(auth_config.clone());
        // A catraca do TLS: escuta o gatilho do gossip e confirma a política
        // com o principal antes de gravar qualquer coisa. Ver `tls_policy`.
        tls_policy::spawn_ratchet(
            net.clone(),
            peer_clients.clone(),
            auth_config.clone(),
            tls_server.clone(),
        );

        let gw_state = GatewayState {
            network: net.clone(),
            ollama: Arc::new(OllamaClient::new(&args.ollama_url)),
            default_model: profile.name.clone(),
            local_assignment: assignment_shared.clone(),
            loads: Arc::new(LoadTracker::new()),
            peer_http: peer_clients.clone(),
            runtime: node_runtime.clone(),
            gateway_port,
            auth: auth_config,
            transfers: Default::default(),
            reply_inbox: crate::direct::ReplyInbox::new(),
            tunnel_tx: Some(tunnel_tx.clone()),
            tls_server: tls_server.clone(),
        };
        let gw_port = gateway_port;
        tokio::spawn(async move {
            if let Err(e) = start_gateway(gw_state, gw_port).await {
                error!("HTTP API server error: {}", e);
            }
        });
    }

    // ── Resolve pkarr peers and dial on startup ──────────────────────────────
    // (libp2p discovery + gossipsub re-announce already started inside
    // `start_p2p_stack`; `disc_dial_tx` came back from it.)
    if let (Some(ref pn), Some(dtx)) = (&pkarr_node, disc_dial_tx.as_ref()) {
        if !pkarr_peer_keys.is_empty() {
            let peers = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                pn.resolve_peers(&pkarr_peer_keys),
            )
            .await
            .unwrap_or_default();
            for peer in &peers {
                if let Ok(ma) = peer.multiaddr.parse::<libp2p::Multiaddr>() {
                    let _ = dtx.send(ma).await;
                }
            }
        }
    }

    // ── Publish pkarr record and schedule 30-min republish ───────────────────
    let serve_relay = args.serve_relay;
    if let Some(pn) = pkarr_node {
        let pn = std::sync::Arc::new(pn);
        let addr = pkarr_multiaddr.clone();
        let inf_port = inference_port;
        let model_name_pk = profile.name.clone();
        let asgn_pk = assignment_shared.clone();

        // Initial publish (best-effort, does not block startup)
        {
            let pn2 = pn.clone();
            let addr2 = addr.clone();
            let mn2 = model_name_pk.clone();
            let asgn2 = asgn_pk.clone();
            tokio::spawn(async move {
                let (start_l, end_l, total_l) = {
                    let a = asgn2.read().await;
                    match *a {
                        Some(ref asgn) => (asgn.start_layer, asgn.end_layer, asgn.total_layers),
                        None => return,
                    }
                }; // guard dropped here
                if let Err(e) = pn2
                    .publish(&addr2, inf_port, &mn2, start_l, end_l, total_l, serve_relay)
                    .await
                {
                    warn!("pkarr initial publish failed: {}", e);
                }
            });
        }

        // Republish every 30 minutes to keep record alive
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30 * 60));
            ticker.tick().await; // skip first immediate tick
            loop {
                ticker.tick().await;
                let (start_l, end_l, total_l) = {
                    let a = asgn_pk.read().await;
                    match *a {
                        Some(ref asgn) => (asgn.start_layer, asgn.end_layer, asgn.total_layers),
                        None => continue,
                    }
                }; // guard dropped here
                if let Err(e) = pn
                    .publish(
                        &addr,
                        inf_port,
                        &model_name_pk,
                        start_l,
                        end_l,
                        total_l,
                        serve_relay,
                    )
                    .await
                {
                    warn!("pkarr republish failed: {}", e);
                }
            }
        });
    }

    // ── Main display loop ────────────────────────────────────────────────────
    let mut ticker = interval(Duration::from_secs(args.refresh));

    println!("\n\x1b[2J\x1b[1;1H");

    loop {
        ticker.tick().await;

        print!("\x1b[2J\x1b[1;1H");

        let peers = net.alive_peers().await;
        let asgn = assignment_shared.read().await;

        if let Some(ref a) = *asgn {
            let gateway_tag = format!(" │ 🌐 http:{}", gateway_port);
            let infer_tag = if let Some(p) = inference_port {
                format!(" │ ⚡ infer:{}", p)
            } else {
                String::new()
            };

            println!(
                "\x1b[1m pom\x1b[0m  node:{} │ {}  │ peers:{} │ {}{}{}",
                &node_id[..12],
                chrono::Utc::now().format("%H:%M:%S"),
                peers.len(),
                if peers.is_empty() {
                    "🔍 discovering..."
                } else {
                    "🟢 connected"
                },
                gateway_tag,
                infer_tag,
            );
            println!();

            // Vivo, não congelado: enquanto isto era a string fixa
            // "inference_server", a TUI anunciava saúde mesmo com o servidor
            // caído em laço (porta presa, por exemplo) e o peer só quebrava na
            // hora da inferência.
            let mem_source = match inference_port {
                Some(_) => runtime_mem_source(&node_runtime.status().await),
                None => mem_source.clone(),
            };
            display::print_assignment(a, &mem_source);
            display::print_layer_map(a.total_layers, a.start_layer, a.end_layer, &peers);
            display::print_peers(&peers);
            display::print_network_health(
                a.total_layers,
                a.start_layer,
                a.end_layer,
                &peers,
                &a.model,
            );

            let covered_now = net.get_covered_layers(&profile.name).await;
            let gap_exists = (0..a.start_layer).any(|l| !covered_now.contains(&l));

            if gap_exists {
                println!();
                println!("  💡 Uncovered layers detected earlier in the model.");
                println!("     Restart with updated --bootstrap to rebalance assignments.");
            }
        }

        println!();
        println!("  \x1b[90mPress Ctrl+C to stop this node\x1b[0m");
    }
}

/// Extract the port from an Ollama URL like "http://localhost:11434".
/// O que a linha "Mem loaded" da TUI mostra sobre o servidor de inferência.
///
/// Estado e motivo, não um rótulo fixo: um servidor que não sobe tem que
/// aparecer aqui, senão o operador só descobre a falha na primeira requisição.
fn runtime_mem_source(status: &runtime::RuntimeStatus) -> String {
    use runtime::ServerState;
    match status.state {
        ServerState::Ready => "inference_server".to_string(),
        ServerState::Starting => format!("inference_server (subindo: {})", status.detail),
        ServerState::Failed => format!("❌ inference_server parado: {}", status.detail),
        ServerState::Disabled => format!("inference_server desligado: {}", status.detail),
    }
}

/// A porta de Ollama que este nó anuncia aos peers.
///
/// Não depende de flag nenhuma: dependia de `--gateway`, que está marcada como
/// deprecated justamente porque todo nó já serve a API. Sem a flag, o nó
/// servia o Ollama local e nenhum peer sabia disso, então ninguém podia rotear
/// para cá.
fn advertised_ollama_port(ollama_url: &str) -> Option<u16> {
    parse_ollama_port(ollama_url)
}

fn parse_ollama_port(url: &str) -> Option<u16> {
    url.trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .nth(1)
        .and_then(|s| s.split('/').next())
        .and_then(|s| s.parse::<u16>().ok())
}

async fn start_setup_mode(gateway_port: u16) -> Result<()> {
    use crate::config::{Config, Role};
    use crate::webui;
    use axum::{routing::get, Router};
    use std::net::SocketAddr;

    let config = Arc::new(RwLock::new(Config {
        role: Role::Principal,
        tls_required: false,
        // Modo setup nao carrega modelo: a escolha de GPU so passa a valer
        // depois que a `pom.json` existir.
        gpus: None,
        cache_type_k: config::KvCacheType::F16,
        cache_type_v: config::KvCacheType::F16,
        connection_key: String::new(),
        gateway_port,
        p2p_port: 42777,
        inference_port: None,
        bootstrap_nodes: vec![],
        advertise_addr: None,
        api_keys: vec![],
        blocked_nodes: vec![],
        // Um nó recém-configurado sobe com as bibliotecas detectadas; a tela de
        // Modelos é onde essa lista deixa de ser um palpite.
        model_dirs: None,
        shard_dir: None,
        shard_max_gb: crate::storage::DEFAULT_SHARD_MAX_GB,
        node_name: None,
    }));

    // The same interface the configured node serves, so setup is not a second
    // look-and-feel to maintain. The app notices it is in setup mode because
    // `/api/ui/node` is not among the routes here.
    let app = Router::new()
        .route("/api/setup/key", get(webui::get_setup_key))
        .route("/api/setup/save", axum::routing::post(webui::save_setup))
        .merge(crate::webapp::routes())
        .with_state(config)
        .layer(tower_http::compression::CompressionLayer::new());

    let addr = SocketAddr::from(([0, 0, 0, 0], gateway_port));
    info!("Setup page available at http://localhost:{}", gateway_port);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// `--advertise-addr` wins, else `--public-addr`, else the persisted config
/// value. Factored out so `main()`'s CLI path and `start_gateway_only` can't
/// drift on this precedence — `--public-addr` used to only reach the pkarr
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

/// Everything `P2pStack` needs to bring up pkarr, a stable libp2p identity,
/// `Network`, and (best-effort) `Discovery` — shared by `main()`'s CLI path
/// and `start_gateway_only`, so the ordering constraint below can't be
/// violated from outside this function: the libp2p keypair must be derived
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
    gateway_port: u16,
    role: String,
    node_display_name: String,
    advertise_addr: Option<String>,
    /// Política inicial, aplicada antes da task de sondagem da `Network`.
    /// Sem isso a URL já nascia em HTTPS pelo cliente global, mas a sonda
    /// começava em HTTP e todos os RTTs remotos ficavam ausentes.
    tls_required: bool,
    /// Raw pkarr public keys (`--relay-peers`), resolved to multiaddrs
    /// *inside* this function, after it loads its own `pkarr_node` — not
    /// pre-resolved by the caller. Resolution needs an already-loaded pkarr
    /// identity, and that identity is loaded here, not by the caller; asking
    /// the caller to resolve first would mean loading pkarr twice (once to
    /// resolve, once inside this function) or restructuring who loads it.
    /// Empty for `start_gateway_only` today — no CLI to source keys from
    /// (see the plan's addendum on deferred parity).
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
    let libp2p_keypair = pkarr_node
        .as_ref()
        .and_then(|pn| {
            let mut seed = pn.secret_key_bytes();
            libp2p::identity::Keypair::ed25519_from_bytes(&mut seed[..]).ok()
        })
        .unwrap_or_else(libp2p::identity::Keypair::generate_ed25519);
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
        .with_identity(args.role, args.node_display_name)
        .with_advertise_addr(args.advertise_addr)
        .with_libp2p_peer_id(Some(libp2p_peer_id.clone())),
    );
    net.set_tls(args.tls_required);

    if args.no_discovery {
        net.detach().await;
        info!("🚫 --no-discovery: nó começa fora da rede, sem anunciar nem procurar peers");
    }

    let assignment_shared: Arc<RwLock<Option<model::LayerAssignment>>> =
        Arc::new(RwLock::new(None));
    net.clone().start(assignment_shared.clone()).await?;

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
        tunnel_tx,
        pkarr_node,
        assignment_shared,
        disc_dial_tx,
    })
}

/// Start the gateway with real routes but without model/network binding.
/// The dashboard, admin, and handshake routes all work so the user can
/// configure the model through the web UI.
async fn start_gateway_only(
    gateway_port: u16,
    app_config: config::Config,
    ollama_url: String,
    no_discovery: bool,
    advertise_addr_cli: String,
    public_addr_cli: String,
) -> Result<()> {
    use crate::gateway::{start_gateway, GatewayState};
    use crate::pipeline::LoadTracker;

    let identity = identity::Identity::load_or_create(&identity::key_path());
    let node_id = identity.node_id.0.clone();
    let pubkey = identity.public_key_hex();

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
    let stack = start_p2p_stack(P2pStackArgs {
        node_id: node_id.clone(),
        pubkey: pubkey.clone(),
        p2p_port: app_config.p2p_port,
        bootstrap_nodes: app_config.bootstrap_nodes.clone(),
        ollama_port,
        inference_port: Some(app_config.inference_port_or_default()),
        gateway_port,
        role: app_config.role.to_string(),
        node_display_name,
        advertise_addr,
        tls_required: app_config.tls_required,
        // No CLI here to source pkarr/relay peer keys from — see the plan's
        // addendum on deferred parity for `start_gateway_only`.
        relay_peer_keys: vec![],
        serve_relay: false,
        use_default_relays: true,
        no_discovery,
    })
    .await?;
    let net = stack.net;
    let assignment_shared = stack.assignment_shared;
    let tunnel_tx = stack.tunnel_tx;

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
    let tls_server =
        match serve::TlsServer::from_identity(&identity, app_config.tls_required, &tls_sans) {
            Ok(t) => Some(t),
            Err(e) => {
                warn!("não deu para derivar o certificado deste nó, TLS indisponível: {e}");
                None
            }
        };

    let auth_state: Arc<RwLock<config::Config>> = Arc::new(RwLock::new(app_config.clone()));
    let node_runtime = runtime::NodeRuntime::new_with_tls_and_auth(
        net.clone(),
        assignment_shared.clone(),
        tls_server.clone(),
        Some(auth_state.clone()),
    );

    // Uma fatia anotada é um reinício pedido pela interface, não uma intenção
    // nova: o nó volta servindo o que servia. Só aqui, porque este é o
    // caminho sem `--model`: quando a linha de comando pede um modelo, essa é
    // a intenção explícita e não deve ser sobreposta por uma fatia velha.
    match lastslice::take() {
        lastslice::LastSlice::Serving(cfg) => {
            let label = cfg.model_label.clone();
            let (a, b) = (cfg.start_layer, cfg.end_layer);
            match node_runtime.apply(cfg).await {
                Ok(()) => info!("↩️  fatia restaurada após reinício: {label} {a}-{b}"),
                Err(e) => warn!("não deu para restaurar a fatia anotada: {e}"),
            }
        }
        // A fatia foi aplicada e o processo morreu antes de ela carregar uma
        // única vez. Retomá-la é o laço infinito: a mesma fatia derruba o
        // processo, o Docker religa o container, e assim para sempre. O nó
        // sobe vazio — mas dizendo por quê, senão o operador não tem como
        // saber que a máquina não aguentou a fatia que recebeu.
        lastslice::LastSlice::NeverLoaded(cfg) => node_runtime.note_unloadable_slice(&cfg).await,
        lastslice::LastSlice::Nothing => {}
    }

    let peer_clients = Arc::new(peer_client::PeerClients::new(Duration::from_secs(600)));
    peer_clients.apply_cluster_policy(app_config.tls_required);
    peer_clients.set_auth_state(auth_state.clone());

    tls_policy::spawn_ratchet(
        net.clone(),
        peer_clients.clone(),
        auth_state.clone(),
        tls_server.clone(),
    );

    let gw_state = GatewayState {
        network: net.clone(),
        ollama: Arc::new(OllamaClient::new(&ollama_url)),
        default_model: String::new(),
        local_assignment: assignment_shared.clone(),
        loads: Arc::new(LoadTracker::new()),
        peer_http: peer_clients.clone(),
        runtime: node_runtime.clone(),
        gateway_port,
        auth: auth_state,
        transfers: Default::default(),
        reply_inbox: crate::direct::ReplyInbox::new(),
        // `start_p2p_stack` always hands back a working sender, even when
        // `Discovery::new` itself failed — sending on it then just fails
        // fast with a closed-channel error, the same already-handled
        // "no tunnel available" outcome `send_frame_via_tunnel` treats it as.
        tunnel_tx: Some(tunnel_tx),
        tls_server: tls_server.clone(),
    };

    info!(
        "Gateway running at http://localhost:{} (model not yet configured)",
        gateway_port
    );
    start_gateway(gw_state, gateway_port).await?;
    Ok(())
}

#[cfg(test)]
mod main_tests {
    use super::*;

    /// A porta do Ollama anunciada não pode depender de `--gateway`: a flag está
    /// marcada como deprecated porque todo nó já serve a API, e enquanto o
    /// anúncio dependia dela um nó sem a flag servia o Ollama local sem que
    /// nenhum peer soubesse - ninguém conseguia rotear para cá.
    #[test]
    fn the_advertised_ollama_port_does_not_depend_on_the_deprecated_flag() {
        let args = Args::parse_from(["pom"]);
        assert!(!args.gateway, "sem a flag, que é o caso que quebrava");
        assert_eq!(advertised_ollama_port(&args.ollama_url), Some(11434));

        let com_flag = Args::parse_from(["pom", "--gateway"]);
        assert_eq!(
            advertised_ollama_port(&com_flag.ollama_url),
            advertised_ollama_port(&args.ollama_url),
            "a flag não pode mudar o que se anuncia"
        );
    }

    /// `--advertise-addr` é mais específico que `--public-addr`, e os dois
    /// vencem o que já estava salvo em `pom.json` — a mesma ordem que
    /// `main()`'s CLI path e `start_gateway_only` têm que aplicar igual, ou
    /// os dois divergem silenciosamente com o tempo.
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
}
