mod display;
mod gateway;
mod identity;
mod memory;
mod model;
mod network;
mod ollama;

use anyhow::Result;
use clap::Parser;
use gateway::{GatewayState, start_gateway};
use memory::load_layer_memory;
use model::{calculate_assignment, parse_memory, ModelProfile};
use network::Network;
use ollama::OllamaClient;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tracing::{error, info};
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

    /// Refresh UI every N seconds
    #[arg(long, default_value = "3")]
    refresh: u64,

    /// List built-in model profiles and Ollama models, then exit
    #[arg(long)]
    list_models: bool,

    /// Ollama base URL (only used when --gateway is set or for model resolution)
    #[arg(long, default_value = "http://localhost:11434")]
    ollama_url: String,

    /// Enable gateway HTTP API — exposes POST /generate, GET /peers, GET /health
    #[arg(long)]
    gateway: bool,

    /// HTTP port for the gateway server
    #[arg(long, default_value = "8080")]
    gateway_port: u16,

    /// Force this node to start at a specific layer (overrides auto gap-filling).
    /// Example: --start-layer 10  means this node owns layers 10..N.
    #[arg(long)]
    start_layer: Option<u32>,
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

    // ── require --model for normal operation ────────────────────────────────
    let model_name = match args.model {
        Some(ref m) => m.clone(),
        None => {
            error!("--model is required. Run with --list-models to see available options.");
            std::process::exit(1);
        }
    };

    // ── require --memory for normal operation ────────────────────────────────
    if args.memory.is_empty() {
        error!("--memory is required (e.g. --memory 8GB).");
        std::process::exit(1);
    }

    display::print_banner();

    // ── Parse memory ─────────────────────────────────────────────────────────
    let memory_mb = match parse_memory(&args.memory) {
        Ok(mb) => mb,
        Err(_) => {
            error!("Invalid memory format '{}'. Use e.g. 10GB or 4096MB", args.memory);
            std::process::exit(1);
        }
    };

    // ── Resolve model profile ────────────────────────────────────────────────
    // Priority: (1) built-in profile, (2) Ollama size-based profile,
    // (3) name-based estimate — so any Ollama model always works.
    let profile = if let Some(p) = ModelProfile::find(&model_name) {
        p
    } else {
        // Try Ollama to get exact file size for a better estimate
        let ollama = OllamaClient::new(&args.ollama_url);
        match ollama.list_models().await {
            Ok(models) => {
                let lower = model_name.to_lowercase();
                if let Some(om) = models.iter().find(|m| {
                    let n = m.name.to_lowercase();
                    // Match "qwen3:8b" == "qwen3:8b", or "qwen3" starts "qwen3:8b"
                    n == lower || lower.starts_with(&n) || n.starts_with(&lower)
                }) {
                    info!(
                        "'{}' found in Ollama ({:.0}MB) — using size-based profile",
                        om.name,
                        om.size as f64 / 1_048_576.0
                    );
                    ModelProfile::from_ollama_model(&model_name, om.size)
                } else {
                    // Model not in Ollama list (yet / different tag) — estimate from name
                    let p = ModelProfile::guess_from_name(&model_name);
                    info!(
                        "'{}' not in Ollama list — using name-based estimate: {} layers",
                        model_name, p.total_layers
                    );
                    p
                }
            }
            Err(_) => {
                // Ollama unreachable — still proceed with name-based estimate
                let p = ModelProfile::guess_from_name(&model_name);
                info!(
                    "Ollama unreachable — estimated profile for '{}': {} layers, {:.0}MB/layer",
                    model_name, p.total_layers, p.memory_per_layer_mb
                );
                p
            }
        }
    };

    info!("Model: {} ({})", profile.name, profile.description);
    info!("Memory allocated: {:.0}MB ({:.1}GB)", memory_mb, memory_mb / 1024.0);

    // ── Generate identity ────────────────────────────────────────────────────
    let identity = identity::Identity::generate();
    let node_id = identity.node_id.0.clone();
    let pubkey = identity.public_key_hex();

    display::print_node_info(&node_id, &pubkey, args.port);

    // ── Bootstrap nodes ──────────────────────────────────────────────────────
    let bootstrap_nodes: Vec<String> = args
        .bootstrap
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if bootstrap_nodes.is_empty() {
        info!("No bootstrap nodes — will rely on local LAN broadcast (port {})", network::DISCOVERY_PORT);
        info!("Tip: use --bootstrap 127.0.0.1:<port> to connect multiple local instances");
    } else {
        info!("Bootstrap nodes: {:?}", bootstrap_nodes);
    }

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
            args.port,
            bootstrap_nodes,
            ollama_port,
        )
        .await?,
    );

    let assignment_shared: Arc<RwLock<Option<model::LayerAssignment>>> =
        Arc::new(RwLock::new(None));

    net.clone().start(assignment_shared.clone()).await?;

    // ── Bootstrap discovery ──────────────────────────────────────────────────
    net.bootstrap_discovery().await?;

    info!("⏳ Scanning network for existing layer coverage...");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── Calculate layer assignment ───────────────────────────────────────────
    let covered = net.get_covered_layers(&profile.name).await;
    if !covered.is_empty() {
        info!("Network already covers layers: {:?}", &covered[..covered.len().min(10)]);
    }

    let assignment = calculate_assignment(&profile, memory_mb, &covered, args.start_layer);

    info!(
        "📦 Assigned layers {}-{} ({} layers, {:.0}MB)",
        assignment.start_layer,
        assignment.end_layer,
        assignment.layers_count,
        assignment.memory_used_mb
    );

    // Actually load (or reserve) the layer data into physical memory.
    // The buffer must stay alive for the entire process.
    let layer_buf = load_layer_memory(
        &profile.name,
        assignment.start_layer,
        assignment.end_layer,
        assignment.total_layers,
        assignment.memory_used_mb,
    );
    let mem_source = layer_buf.source.to_string();

    display::print_assignment(&assignment, &mem_source);

    {
        let mut a = assignment_shared.write().await;
        *a = Some(assignment.clone());
    }

    net.announce(&assignment).await?;

    // ── Gateway HTTP server (optional) ───────────────────────────────────────
    if args.gateway {
        let gw_state = GatewayState {
            network: net.clone(),
            ollama: Arc::new(OllamaClient::new(&args.ollama_url)),
            default_model: profile.name.clone(),
        };
        let gw_port = args.gateway_port;
        tokio::spawn(async move {
            if let Err(e) = start_gateway(gw_state, gw_port).await {
                error!("Gateway error: {}", e);
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
            let gateway_tag = if args.gateway {
                format!(" │ 🌐 gateway:{}",  args.gateway_port)
            } else {
                String::new()
            };

            println!(
                "\x1b[1m rdma-p2p\x1b[0m  node:{} │ {}  │ peers:{} │ {}{}",
                &node_id[..12],
                chrono::Utc::now().format("%H:%M:%S"),
                peers.len(),
                if peers.is_empty() { "🔍 discovering..." } else { "🟢 connected" },
                gateway_tag,
            );
            println!();

            display::print_assignment(a, &mem_source);
            display::print_layer_map(a.total_layers, a.start_layer, a.end_layer, &peers);
            display::print_peers(&peers);
            display::print_network_health(a.total_layers, &peers, &a.model);

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
