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

    /// Disable Qwen3.5 thinking mode (injects empty <think></think> block).
    #[arg(long)]
    no_thinking: bool,

    /// Single-peer mode: loads model without embedding extraction (correct causal attention).
    /// Enable when one node covers all layers.
    #[arg(long)]
    single_peer: bool,
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

    // ── require --memory unless inference_port handles model loading ─────────
    if args.memory.is_empty() && args.inference_port.is_none() {
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

    // When inference_server.py manages the model, skip RAM reservation —
    // the Python process owns the model memory.
    let (layer_buf, mem_source) = if args.inference_port.is_none() {
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

    net.announce(&assignment).await?;

    // ── Start Inference Server (if port provided) ────────────────────────────
    if let Some(port) = args.inference_port {
        let model_path = model_name.clone();
        let start_l = assignment.start_layer;
        let end_l = assignment.end_layer;
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let venv_python = cwd.join("venv/bin/python3");
        let python_exe = if venv_python.exists() {
            venv_python.to_string_lossy().to_string()
        } else {
            "python3".to_string()
        };
        let server_script = cwd.join("inference_server.py");
        let n_gpu_layers = args.n_gpu_layers;
        let n_ctx = args.n_ctx;
        let n_batch = args.n_batch;
        let max_new_tokens = args.max_new_tokens;
        let temperature = args.temperature;
        let top_p = args.top_p;
        let top_k = args.top_k;
        let repeat_penalty = args.repeat_penalty;
        let no_thinking = args.no_thinking;
        let single_peer = args.single_peer;
        let log_path = format!("inference_server_{}.log", port);
        let node_id_log = node_id.clone();

        info!("Starting inference_server.py: layers={}-{} port={} gpu_layers={} n_ctx={} single_peer={}",
            start_l, end_l, port, n_gpu_layers, n_ctx, single_peer);
        info!("Inference server log: {}", log_path);

        tokio::spawn(async move {
            let mut attempts = 0u32;
            loop {
                attempts += 1;
                if attempts > 1 {
                    warn!("Restarting inference server (attempt {}/10)...", attempts);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }

                let mut cmd = Command::new(&python_exe);
                cmd.env("PYTHONUNBUFFERED", "1")
                    .arg(&server_script)
                    .arg("--model").arg(&model_path)
                    .arg("--start-layer").arg(start_l.to_string())
                    .arg("--end-layer").arg(end_l.to_string())
                    .arg("--port").arg(port.to_string())
                    .arg("--n-gpu-layers").arg(n_gpu_layers.to_string())
                    .arg("--n-ctx").arg(n_ctx.to_string())
                    .arg("--n-batch").arg(n_batch.to_string())
                    .arg("--max-new-tokens").arg(max_new_tokens.to_string())
                    .arg("--temperature").arg(temperature.to_string())
                    .arg("--top-p").arg(top_p.to_string())
                    .arg("--top-k").arg(top_k.to_string())
                    .arg("--repeat-penalty").arg(repeat_penalty.to_string())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                if no_thinking { cmd.arg("--no-thinking"); }
                if single_peer { cmd.arg("--single-peer"); }

                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        error!("Failed to spawn inference server: {}", e);
                        if attempts >= 10 {
                            error!("Inference server failed to start 10 times, giving up");
                            break;
                        }
                        continue;
                    }
                };

                let short_id = node_id_log[..8].to_string();

                if let Some(stdout) = child.stdout.take() {
                    let nid = short_id.clone();
                    let lp = log_path.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncBufReadExt, BufReader};
                        use std::io::Write;
                        let mut reader = BufReader::new(stdout).lines();
                        while let Ok(Some(line)) = reader.next_line().await {
                            println!("\x1b[36m[InfServer {}]\x1b[0m {}", nid, line);
                            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&lp) {
                                let _ = writeln!(f, "[OUT] {}", line);
                            }
                        }
                    });
                }

                if let Some(stderr) = child.stderr.take() {
                    let nid = short_id.clone();
                    let lp = log_path.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncBufReadExt, BufReader};
                        use std::io::Write;
                        let mut reader = BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = reader.next_line().await {
                            eprintln!("\x1b[31m[InfServer {} ERR]\x1b[0m {}", nid, line);
                            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&lp) {
                                let _ = writeln!(f, "[ERR] {}", line);
                            }
                        }
                    });
                }

                let status = child.wait().await;
                warn!("Inference server {} exited (attempt {}): {:?}", short_id, attempts, status);

                if attempts >= 10 {
                    error!("Inference server crashed 10 times, giving up. Check {}", log_path);
                    break;
                }
            }
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
