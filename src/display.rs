use crate::model::LayerAssignment;
use crate::network::PeerInfo;

pub fn print_banner() {
    println!(
        r#"
╔══════════════════════════════════════════════════════════════╗
║         ██████╗ ██████╗ ███╗   ███╗ █████╗                  ║
║         ██╔══██╗██╔══██╗████╗ ████║██╔══██╗                 ║
║         ██████╔╝██║  ██║██╔████╔██║███████║                 ║
║         ██╔══██╗██║  ██║██║╚██╔╝██║██╔══██║                 ║
║         ██║  ██║██████╔╝██║ ╚═╝ ██║██║  ██║                 ║
║         ╚═╝  ╚═╝╚═════╝ ╚═╝     ╚═╝╚═╝  ╚═╝                ║
║                                                              ║
║              P2P Distributed LLM Inference                   ║
║                    rdma-sim layer                            ║
╚══════════════════════════════════════════════════════════════╝
"#
    );
}

pub fn print_node_info(node_id: &str, pubkey: &str, port: u16) {
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│  NODE IDENTITY                                          │");
    println!("│  ID  : {}...                  │", &node_id[..48]);
    println!("│  Key : {}...         │", &pubkey[..32]);
    println!("│  Port: {}                                            │", port);
    println!("└─────────────────────────────────────────────────────────┘");
}

pub fn print_assignment(a: &LayerAssignment, mem_source: &str) {
    let used_pct = (a.memory_used_mb / a.memory_allocated_mb * 100.0) as u32;
    let bar = make_bar(used_pct, 30);

    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│  MODEL ASSIGNMENT                                       │");
    println!("│  Model      : {:<43} │", a.model);
    println!("│  Total Layers: {:<42} │", a.total_layers);
    println!("│  My Layers  : {}-{} ({} layers){:<29} │",
        a.start_layer, a.end_layer, a.layers_count,
        ""
    );
    println!("│  Memory     : {:.0}MB / {:.0}MB  {}{}% │",
        a.memory_used_mb,
        a.memory_allocated_mb,
        bar,
        used_pct
    );
    println!("│  Mem loaded : {:<43} │", mem_source);
    println!("└─────────────────────────────────────────────────────────┘");
}

pub fn print_layer_map(total_layers: u32, my_start: u32, my_end: u32, peers: &[PeerInfo]) {
    println!();
    println!("  LAYER COVERAGE MAP  (total: {} layers)", total_layers);
    println!();

    // Build coverage map
    let mut coverage: Vec<Option<String>> = vec![None; total_layers as usize];

    // Mark peer layers
    for peer in peers {
        let label = format!("P{}", &peer.node_id[..4]);
        for l in peer.start_layer..=peer.end_layer {
            if (l as usize) < coverage.len() {
                coverage[l as usize] = Some(label.clone());
            }
        }
    }

    // Mark our layers
    for l in my_start..=my_end {
        if (l as usize) < coverage.len() {
            coverage[l as usize] = Some("ME".into());
        }
    }

    // Print in rows of 8
    let row_size = 8usize;
    let rows = (total_layers as usize + row_size - 1) / row_size;

    for row in 0..rows {
        let start = row * row_size;
        let end = (start + row_size).min(total_layers as usize);

        print!("  Layer {:>2}-{:<2}  │", start, end - 1);

        for i in start..end {
            match &coverage[i] {
                Some(label) if label == "ME" => print!(" \x1b[32m[{:^4}]\x1b[0m", i),
                Some(label) => print!(" \x1b[34m[{:^4}]\x1b[0m", i),
                None => print!(" \x1b[90m[ -- ]\x1b[0m"),
            }
        }
        println!();
    }

    // Legend
    println!();
    println!("  \x1b[32m[■■■■]\x1b[0m = This node   \x1b[34m[■■■■]\x1b[0m = Peer   \x1b[90m[ -- ]\x1b[0m = Uncovered");
}

pub fn print_peers(peers: &[PeerInfo]) {
    println!();
    if peers.is_empty() {
        println!("  ⏳ No peers discovered yet — waiting for network...");
        return;
    }

    println!("  CONNECTED PEERS ({} online)", peers.len());
    println!("  ┌──────────────┬──────────────┬───────────┬───────────────┐");
    println!("  │ Node ID      │ Model        │ Layers    │ Memory        │");
    println!("  ├──────────────┼──────────────┼───────────┼───────────────┤");

    for peer in peers {
        println!(
            "  │ {}... │ {:<12} │ {:<9} │ {:.0}MB          │",
            &peer.node_id[..10],
            truncate(&peer.model, 12),
            peer.layer_range(),
            peer.memory_mb
        );
    }

    println!("  └──────────────┴──────────────┴───────────┴───────────────┘");
}

pub fn print_network_health(total_layers: u32, peers: &[PeerInfo], model: &str) {
    let covered: Vec<u32> = {
        let mut v = Vec::new();
        for peer in peers {
            if peer.model == model {
                for l in peer.start_layer..=peer.end_layer {
                    if !v.contains(&l) {
                        v.push(l);
                    }
                }
            }
        }
        v
    };

    let coverage_pct = (covered.len() as f32 / total_layers as f32 * 100.0) as u32;
    let bar = make_bar(coverage_pct, 20);

    println!();
    println!("  NETWORK HEALTH");
    println!("  Coverage : {} {}%  ({}/{} layers covered)",
        bar, coverage_pct, covered.len(), total_layers
    );

    if coverage_pct < 100 {
        let missing: Vec<u32> = (0..total_layers).filter(|l| !covered.contains(l)).collect();
        if missing.len() <= 10 {
            println!("  ⚠️  Missing layers: {:?}", missing);
        } else {
            println!("  ⚠️  {} layers not yet covered", missing.len());
        }
    } else {
        println!("  ✅ Full model coverage — inference pipeline complete!");
    }
}

fn make_bar(pct: u32, width: u32) -> String {
    let filled = (pct * width / 100) as usize;
    let empty = width as usize - filled;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
