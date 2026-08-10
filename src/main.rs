mod admin;
mod bitswap;
mod catalog;
mod config;
mod auth;
mod session;
mod discovery;
mod display;
mod gateway;
mod gguf;
mod identity;
mod memory;
mod model;
mod network;
mod ollama;
mod openai;
mod pipeline;
mod prefill;
mod reqlog;
mod pkarr_node;
mod plan;
mod runtime;
mod sysmon;
mod transfer;
mod wire;
mod ui_manifest;
mod webapp;
mod webui;

use anyhow::Result;
use clap::Parser;
use discovery::Discovery;
use gateway::{GatewayState, start_gateway};
use memory::load_layer_memory;
use model::{calculate_assignment, parse_memory, ModelProfile};
use network::{Network, P2PMessage};
use ollama::OllamaClient;
use pipeline::LoadTracker;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "rdma-p2p",
    about = "Distributed P2P LLM inference with RDMA-sim layer",
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
    /// bare IPv6. Overrides `advertise_addr` in rdma-p2p.json. Without it each
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

    /// Ollama base URL (only used when --gateway is set or for model resolution)
    #[arg(long, default_value = "http://localhost:11434")]
    ollama_url: String,

    /// Deprecated — every node now serves the HTTP API. Accepted for backwards compatibility;
    /// only effect remaining is advertising the Ollama port to peers.
    #[arg(long)]
    gateway: bool,

    /// HTTP port for the gateway/API server (served by every node)
    #[arg(long, default_value = "8080")]
    gateway_port: u16,

    /// Force this node to start at a specific layer (overrides auto gap-filling).
    /// Example: --start-layer 10  means this node owns layers 10..N.
    #[arg(long)]
    start_layer: Option<u32>,

    /// Force the last layer this node owns. Pairs with --start-layer; both are
    /// inferred from the filename (`name_<start>_<end>.gguf`) when omitted.
    #[arg(long)]
    end_layer: Option<u32>,

    /// Port for the llama-cpp-python inference server on this node.
    /// When set, rdma-p2p auto-starts inference_server.py on this port.
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

    /// Generations allowed in flight at once. The KV cache is split evenly
    /// between them, so the window one request sees is n_ctx / max_sessions.
    #[arg(long, default_value = "1")]
    max_sessions: u32,

    /// Prompt tokens pushed through the pipeline per round trip.
    #[arg(long, default_value = "512")]
    prefill_chunk_tokens: u32,

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

    /// External address to publish to pkarr DHT (HOST:PORT). Auto-detected if not set.
    /// PORT here is the UDP port; libp2p TCP port will be UDP+10000.
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

/// Extract (start, end) layers from a filename like "gemma2_0_20.gguf"
fn parse_layers_from_filename(path: &str) -> Option<(u32, u32)> {
    let filename = std::path::Path::new(path)
        .file_stem()?
        .to_str()?;
    
    // Look for patterns like _0_20 at the end or middle
    let parts: Vec<&str> = filename.split('_').collect();
    if parts.len() >= 3 {
        // Try last two parts first
        let end_idx = parts.len() - 1;
        let start_idx = parts.len() - 2;
        
        if let (Ok(s), Ok(e)) = (parts[start_idx].parse::<u32>(), parts[end_idx].parse::<u32>()) {
            return Some((s, e));
        }
        
        // Try searching for any two adjacent numeric parts
        for i in 0..parts.len()-1 {
            if let (Ok(s), Ok(e)) = (parts[i].parse::<u32>(), parts[i+1].parse::<u32>()) {
                // Heuristic: start < end and end - start < 200 (sanity check)
                if s < e && e - s < 200 {
                    return Some((s, e));
                }
            }
        }
    }
    None
}

#[tokio::main]
async fn main() -> Result<()> {
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
                error!("Invalid memory format '{}'. Use e.g. 10GB or 4096MB", args.memory);
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
            Err(_) => {
                ModelProfile::guess_from_name(&model_name)
            }
        }
    };

    // Force profile name to be consistent if we found a match
    if let Some(p) = ModelProfile::find(&profile.name) {
        profile.name = p.name;
    }

    info!("Model: {} ({})", profile.name, profile.description);
    info!("Memory allocated: {:.0}MB ({:.1}GB)", memory_mb, memory_mb / 1024.0);

    // ── Generate identity ────────────────────────────────────────────────────
    let identity = identity::Identity::load_or_create(
        &config::project_root().join(identity::KEY_FILENAME),
    );
    let node_id = identity.node_id.0.clone();
    let pubkey = identity.public_key_hex();

    display::print_node_info(&node_id, &pubkey, p2p_port);

    // ── Pkarr identity (stable across restarts) ──────────────────────────────
    let key_path = std::path::Path::new(".rdma-p2p.key");
    let pkarr_node = match pkarr_node::PkarrNode::load_or_create(key_path) {
        Ok(n) => {
            info!("🔑 Pkarr address: {}", n.public_key_uri());
            Some(n)
        }
        Err(e) => {
            warn!("pkarr init failed (discovery disabled): {}", e);
            None
        }
    };

    // ── Compute pkarr multiaddr to publish ───────────────────────────────────
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
    let mut bootstrap_nodes: Vec<String> = app_config.bootstrap_nodes.clone();
    for b in args.bootstrap.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        if !bootstrap_nodes.contains(&b) {
            bootstrap_nodes.push(b);
        }
    }

    // CLI wins over the config file, same as --bootstrap.
    let advertise_addr: Option<String> = if !args.advertise_addr.trim().is_empty() {
        Some(args.advertise_addr.clone())
    } else {
        app_config.advertise_addr.clone()
    };

    if bootstrap_nodes.is_empty() {
        info!("No bootstrap nodes — will rely on local LAN broadcast (port {})", network::DISCOVERY_PORT);
        info!("Tip: use --bootstrap 127.0.0.1:<port> to connect multiple local instances");
    } else {
        info!("Bootstrap nodes: {:?}", bootstrap_nodes);
    }

    // ── Resolve relay peers via pkarr ────────────────────────────────────────
    let resolved_relay_addrs: Vec<String> = if let Some(ref pn) = pkarr_node {
        if !relay_peer_keys.is_empty() {
            let peers = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                pn.resolve_peers(&relay_peer_keys),
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

    // ── Determine Ollama port to advertise ───────────────────────────────────
    // Only advertise when --gateway is set so other nodes can route requests here.
    let ollama_port: Option<u16> = if args.gateway {
        parse_ollama_port(&args.ollama_url)
    } else {
        None
    };

    // ── Start P2P network ────────────────────────────────────────────────────
    let net = Arc::new(
        Network::new(
            node_id.clone(),
            pubkey.clone(),
            p2p_port,
            bootstrap_nodes.clone(),
            ollama_port,
inference_port,
            Some(gateway_port),
        )
        .await?
        .with_identity(
            app_config.role.to_string(),
            sysmon::sample().await.hostname,
        )
        .with_advertise_addr(advertise_addr.clone()),
    );

    if args.no_discovery {
        net.detach().await;
        info!("🚫 --no-discovery: nó começa fora da rede, sem anunciar nem procurar peers");
    }

    let assignment_shared: Arc<RwLock<Option<model::LayerAssignment>>> =
        Arc::new(RwLock::new(None));

    net.clone().start(assignment_shared.clone()).await?;

    info!("⏳ Scanning network for existing layer coverage...");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── Calculate layer assignment ───────────────────────────────────────────
    let covered = net.get_covered_layers(&profile.name).await;
    if !covered.is_empty() {
        info!("Network already covers layers: {:?}", &covered[..covered.len().min(10)]);
    }

    // Layer range, in order of precedence: explicit flags, then the filename
    // convention (`name_<start>_<end>.gguf`), then the gap-filling algorithm.
    let explicit_range = match (args.start_layer, args.end_layer) {
        (Some(s), Some(e)) => Some((s, e)),
        _ => None,
    };
    let assignment = if let Some((s, e)) = explicit_range.or_else(|| parse_layers_from_filename(&model_name))
    {
        if explicit_range.is_some() {
            info!("Layers {}-{} set explicitly", s, e);
        } else {
            info!("Parsed layers {}-{} from filename '{}'", s, e, model_name);
        }
        let actual_count = e - s + 1;
        model::LayerAssignment {
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
    // Everything the web UI can change lives here, so a reconfiguration
    // restarts the Python child without restarting this process.
    let cwd = config::project_root();
    let node_runtime = runtime::NodeRuntime::new(net.clone(), assignment_shared.clone(), cwd);

    if let Some(port) = inference_port {
        let cfg = runtime::NodeConfig {
            model_path: model_name.clone(),
            model_label: profile.name.clone(),
            start_layer: assignment.start_layer,
            end_layer: assignment.end_layer,
            total_layers: assignment.total_layers,
            peer_count: if args.single_peer { 1 } else { 2 },
            peer_index: 0,
            primary: args.gateway,
            inference_port: port,
            n_gpu_layers: args.n_gpu_layers,
            n_ctx: args.n_ctx,
            n_batch: args.n_batch,
            max_sessions: args.max_sessions,
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
        };
        if let Err(e) = node_runtime.apply(cfg).await {
            error!("não foi possível iniciar o servidor de inferência: {e:#}");
        }
    }

    // ── HTTP API server — every node serves it ───────────────────────────────
    {
        let auth_config: Arc<tokio::sync::RwLock<config::Config>> = Arc::new(tokio::sync::RwLock::new(app_config.clone()));

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;
        let gw_state = GatewayState {
            network: net.clone(),
            ollama: Arc::new(OllamaClient::new(&args.ollama_url)),
            default_model: profile.name.clone(),
            local_assignment: assignment_shared.clone(),
            loads: Arc::new(LoadTracker::new()),
            http: Arc::new(http_client),
            runtime: node_runtime.clone(),
            gateway_port,
            auth: auth_config,
            transfers: Default::default(),
        };
        let gw_port = gateway_port;
        tokio::spawn(async move {
            if let Err(e) = start_gateway(gw_state, gw_port).await {
                error!("HTTP API server error: {}", e);
            }
        });
    }

    // ── libp2p discovery (LAN mDNS + global Kademlia + gossipsub) ────────────
    // libp2p TCP port = UDP port + 10 000 (e.g. 42777 → 52777).
    // Announce messages are published to gossipsub every 5 s so internet peers
    // learn our layer assignment without needing a shared signalling server.
    let (disc_announce_tx, disc_dial_tx) = match Discovery::new(
        p2p_port,
        &bootstrap_nodes,
        &resolved_relay_addrs,
        args.serve_relay,
        !args.no_default_relays,
        net.clone(),
    ) {
        Ok(disc) => {
            let ann_tx = disc.announce_tx.clone();
            let dial_tx = disc.dial_tx.clone();
            tokio::spawn(async move { disc.run().await });
            (Some(ann_tx), Some(dial_tx))
        }
        Err(e) => {
            warn!("libp2p discovery disabled ({}). UDP-only mode.", e);
            (None, None)
        }
    };

    // Publish our own Announce to gossipsub every 5 s so newly-connected
    // libp2p peers learn our model/layer assignment.
    if let Some(tx) = disc_announce_tx {
        let net_disc = net.clone();
        let asgn_disc = assignment_shared.clone();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(5));
            loop {
                ticker.tick().await;
                let asgn = asgn_disc.read().await;
                if let Some(ref a) = *asgn {
                    let msg = P2PMessage::Announce {
                        node_id: net_disc.node_id.clone(),
                        pubkey: net_disc.pubkey.clone(),
                        addr: net_disc.announce_addr(),
                        model: a.model.clone(),
                        start_layer: a.start_layer,
                        end_layer: a.end_layer,
                        total_layers: a.total_layers,
                        memory_mb: a.memory_used_mb,
                        timestamp: chrono::Utc::now(),
                        ollama_port: net_disc.ollama_port,
                        inference_port: net_disc.inference_port,
                        gateway_port: net_disc.gateway_port,
                        role: String::new(),
                        hostname: String::new(),
                        disk_total_mb: 0,
                        disk_free_mb: 0,
                    };
                    if let Ok(data) = serde_json::to_vec(&msg) {
                        let _ = tx.send(data).await;
                    }
                }
            }
        });
    }

    // ── Resolve pkarr peers and dial on startup ──────────────────────────────
    if let (Some(ref pn), Some(dtx)) = (&pkarr_node, disc_dial_tx.as_ref()) {
        if !pkarr_peer_keys.is_empty() {
            let peers = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                pn.resolve_peers(&pkarr_peer_keys),
            ).await.unwrap_or_default();
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
                if let Err(e) = pn2.publish(&addr2, inf_port, &mn2, start_l, end_l, total_l, serve_relay).await {
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
                if let Err(e) = pn.publish(&addr, inf_port, &model_name_pk, start_l, end_l, total_l, serve_relay).await {
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
                "\x1b[1m rdma-p2p\x1b[0m  node:{} │ {}  │ peers:{} │ {}{}{}",
                &node_id[..12],
                chrono::Utc::now().format("%H:%M:%S"),
                peers.len(),
                if peers.is_empty() { "🔍 discovering..." } else { "🟢 connected" },
                gateway_tag,
                infer_tag,
            );
            println!();

            display::print_assignment(a, &mem_source);
            display::print_layer_map(a.total_layers, a.start_layer, a.end_layer, &peers);
            display::print_peers(&peers);
            display::print_network_health(a.total_layers, a.start_layer, a.end_layer, &peers, &a.model);

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
    }));

    // The same interface the configured node serves, so setup is not a second
    // look-and-feel to maintain. The app notices it is in setup mode because
    // `/api/ui/node` is not among the routes here.
    let app = Router::new()
        .route("/api/setup/key", get(webui::get_setup_key))
        .route("/api/setup/save", axum::routing::post(webui::save_setup))
        .merge(crate::webapp::routes())
        .with_state(config);

    let addr = SocketAddr::from(([0, 0, 0, 0], gateway_port));
    info!("Setup page available at http://localhost:{}", gateway_port);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Start the gateway with real routes but without model/network binding.
/// The dashboard, admin, and handshake routes all work so the user can
/// configure the model through the web UI.
async fn start_gateway_only(
    gateway_port: u16,
    app_config: config::Config,
    ollama_url: String,
    no_discovery: bool,
) -> Result<()> {
    use crate::gateway::{GatewayState, start_gateway};
    use crate::pipeline::LoadTracker;

    let identity = identity::Identity::load_or_create(
        &config::project_root().join(identity::KEY_FILENAME),
    );
    let node_id = identity.node_id.0.clone();
    let pubkey = identity.public_key_hex();

    let net = Arc::new(
        Network::new(
            node_id.clone(),
            pubkey.clone(),
            app_config.p2p_port,
            app_config.bootstrap_nodes.clone(),
            None,
            Some(app_config.inference_port_or_default()),
            Some(gateway_port),
        )
        .await?
        .with_identity(
            app_config.role.to_string(),
            sysmon::sample().await.hostname,
        )
        .with_advertise_addr(app_config.advertise_addr.clone()),
    );

    if no_discovery {
        net.detach().await;
        info!("🚫 --no-discovery: nó começa fora da rede, sem anunciar nem procurar peers");
    }

    let assignment_shared: Arc<RwLock<Option<model::LayerAssignment>>> =
        Arc::new(RwLock::new(None));

    net.clone().start(assignment_shared.clone()).await?;

    let cwd = config::project_root();
    let node_runtime = runtime::NodeRuntime::new(net.clone(), assignment_shared.clone(), cwd);

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;

    let auth_state: Arc<RwLock<config::Config>> = Arc::new(RwLock::new(app_config));

    let gw_state = GatewayState {
        network: net.clone(),
        ollama: Arc::new(OllamaClient::new(&ollama_url)),
        default_model: String::new(),
        local_assignment: assignment_shared.clone(),
        loads: Arc::new(LoadTracker::new()),
        http: Arc::new(http_client),
        runtime: node_runtime.clone(),
        gateway_port,
        auth: auth_state,
        transfers: Default::default(),
    };

    info!("Gateway running at http://localhost:{} (model not yet configured)", gateway_port);
    start_gateway(gw_state, gateway_port).await?;
    Ok(())
}

fn gethostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".into())
}
