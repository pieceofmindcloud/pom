mod display;
mod gateway;
mod identity;
mod memory;
mod model;
mod network;
mod ollama;
mod pipeline;

use anyhow::Result;
use clap::Parser;
use gateway::{GatewayState, start_gateway};
use memory::load_layer_memory;
use model::{calculate_assignment, parse_memory, ModelProfile};
use network::Network;
use ollama::OllamaClient;
use pipeline::LoadTracker;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tokio::process::Command;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use std::process::Stdio;

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

    /// Port for the llama-cpp-python inference server on this node.
    /// When set, this node participates in pipeline inference.
    /// Run inference_server.py --port <N> --model <path> --start-layer S --end-layer E
    #[arg(long)]
    inference_port: Option<u16>,
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
            args.inference_port,
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

    // Try to extract layers from filename, otherwise calculate
    let assignment = if let Some((s, e)) = parse_layers_from_filename(&model_name) {
        info!("Parsed layers {}-{} from filename '{}'", s, e, model_name);
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

    // ── Start Inference Server (if port provided) ────────────────────────────
    if let Some(port) = args.inference_port {
        let model_path = model_name.clone();
        let start_l = assignment.start_layer;
        let end_l = assignment.end_layer;
        
        info!("Starting inference server on port {} for layers {}-{}...", port, start_l, end_l);
        
        // Find python executable (prefer pyenv if available, fallback to python3)
        let python_exe = "/Users/jairpereira/.pyenv/versions/email/bin/python";
        let python_exe = if std::path::Path::new(python_exe).exists() {
            python_exe.to_string()
        } else {
            "python3".to_string()
        };

        // Get absolute path to inference_server.py
        let server_script = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join("inference_server.py");

        info!("Inference command: {} {} --model {} --start-layer {} --end-layer {} --port {}", 
            python_exe, server_script.display(), model_path, start_l, end_l, port);

        let mut child = Command::new(python_exe)
            .env("PYTHONUNBUFFERED", "1")
            .arg(server_script)
            .arg("--model")
            .arg(&model_path)
            .arg("--start-layer")
            .arg(start_l.to_string())
            .arg("--end-layer")
            .arg(end_l.to_string())
            .arg("--port")
            .arg(port.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to start inference_server.py");

        // Log output in background
        let node_id_log = node_id.clone();
        let mut stdout = child.stdout.take().expect("Failed to open stdout");
        let mut stderr = child.stderr.take().expect("Failed to open stderr");

        // Task for stdout
        let nid1 = node_id_log.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                println!("\x1b[36m[InfServer {}]\x1b[0m {}", &nid1[..8], line);
            }
        });

        // Task for stderr
        let nid2 = node_id_log.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                eprintln!("\x1b[31m[InfServer {} ERR]\x1b[0m {}", &nid2[..8], line);
            }
        });

        tokio::spawn(async move {
            let status = child.wait().await;
            warn!("Inference server {} exited with status: {:?}", &node_id_log[..8], status);
        });
    }

    // ── Gateway HTTP server (optional) ───────────────────────────────────────
    if args.gateway {
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
                format!(" │ 🌐 gateway:{}", args.gateway_port)
            } else {
                String::new()
            };
            let infer_tag = if let Some(p) = args.inference_port {
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
