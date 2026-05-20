use serde::{Deserialize, Serialize};

/// Known model architectures and their layer memory profiles
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfile {
    pub name: String,
    pub total_layers: u32,
    pub memory_per_layer_mb: f64,
    pub overhead_mb: f64, // tokenizer, embeddings, output head
    pub description: String,
}

impl ModelProfile {
    /// Returns known model profiles
    pub fn known_models() -> Vec<ModelProfile> {
        vec![
            ModelProfile {
                name: "gemma2".into(),
                total_layers: 42,
                memory_per_layer_mb: 380.0,
                overhead_mb: 800.0,
                description: "Google Gemma 2 9B".into(),
            },
            ModelProfile {
                name: "gemma3".into(),
                total_layers: 28,
                memory_per_layer_mb: 180.0,
                overhead_mb: 512.0,
                description: "Google Gemma 3 2B".into(),
            },
            ModelProfile {
                name: "gemma3:7b".into(),
                total_layers: 32,
                memory_per_layer_mb: 420.0,
                overhead_mb: 800.0,
                description: "Google Gemma 3 7B".into(),
            },
            ModelProfile {
                name: "gemma3:27b".into(),
                total_layers: 46,
                memory_per_layer_mb: 1100.0,
                overhead_mb: 2048.0,
                description: "Google Gemma 3 27B".into(),
            },
            ModelProfile {
                name: "llama3".into(),
                total_layers: 32,
                memory_per_layer_mb: 350.0,
                overhead_mb: 600.0,
                description: "Meta LLaMA 3 8B".into(),
            },
            ModelProfile {
                name: "llama3:70b".into(),
                total_layers: 80,
                memory_per_layer_mb: 1600.0,
                overhead_mb: 3000.0,
                description: "Meta LLaMA 3 70B".into(),
            },
            ModelProfile {
                name: "mistral".into(),
                total_layers: 32,
                memory_per_layer_mb: 320.0,
                overhead_mb: 500.0,
                description: "Mistral 7B".into(),
            },
            ModelProfile {
                name: "qwen2.5".into(),
                total_layers: 28,
                memory_per_layer_mb: 290.0,
                overhead_mb: 450.0,
                description: "Qwen 2.5 7B".into(),
            },
            ModelProfile {
                name: "deepseek-r1".into(),
                total_layers: 64,
                memory_per_layer_mb: 980.0,
                overhead_mb: 2000.0,
                description: "DeepSeek R1 32B".into(),
            },
        ]
    }

    pub fn find(name: &str) -> Option<ModelProfile> {
        let lower = name.to_lowercase();
        Self::known_models()
            .into_iter()
            .find(|m| {
                let m_lower = m.name.to_lowercase();
                lower == m_lower || lower.starts_with(&m_lower)
            })
    }

    /// Build a ModelProfile from an Ollama model's file size (best-effort estimate).
    pub fn from_ollama_model(name: &str, size_bytes: u64) -> ModelProfile {
        let size_mb = size_bytes as f64 / (1024.0 * 1024.0);
        let overhead_mb = size_mb * 0.10;
        let usable_mb = size_mb - overhead_mb;
        let total_layers = ((usable_mb / 220.0).round() as u32).clamp(8, 96);
        let memory_per_layer_mb = usable_mb / total_layers as f64;
        ModelProfile {
            name: name.to_string(),
            total_layers,
            memory_per_layer_mb,
            overhead_mb,
            description: format!("Ollama model ({:.0}MB)", size_mb),
        }
    }

    /// Estimate a profile from the model name alone (no Ollama needed).
    /// Parses parameter counts like "8b", "70b", etc. from the name.
    pub fn guess_from_name(name: &str) -> ModelProfile {
        let lower = name.to_lowercase();
        // Extract filename from path if needed
        let filename = std::path::Path::new(&lower)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(&lower);

        let (clean_name, total_layers, mb_per_layer, overhead_mb) = if filename.contains("gemma2") {
            ("gemma2", 42, 380.0, 800.0)
        } else if filename.contains("gemma3") {
            ("gemma3", 32, 420.0, 800.0)
        } else if filename.contains("llama3") {
            ("llama3", 32, 380.0, 700.0)
        } else if filename.contains("671b") {
            ("deepseek-v3", 128, 4000.0, 8000.0)
        } else if filename.contains("70b") || filename.contains("72b") || filename.contains("65b") {
            ("large-llm", 80, 1600.0, 3000.0)
        } else if filename.contains("34b") || filename.contains("32b") {
            ("medium-llm", 64, 980.0, 2000.0)
        } else if filename.contains("14b") || filename.contains("13b") {
            ("small-llm", 40, 600.0, 1200.0)
        } else if filename.contains("qwen") {
            ("qwen35", 32, 380.0, 700.0)
        } else if filename.contains("9b") || filename.contains("8b") || filename.contains("7b") {
            ("7b-llm", 32, 380.0, 700.0)
        } else if filename.contains("3b") || filename.contains("4b") {
            ("3b-llm", 32, 200.0, 400.0)
        } else if filename.contains("1b") || filename.contains("2b") {
            ("1b-llm", 28, 130.0, 300.0)
        } else {
            ("unknown-llm", 32, 300.0, 600.0) // unknown → assume ~7B
        };

        ModelProfile {
            name: clean_name.to_string(),
            total_layers,
            memory_per_layer_mb: mb_per_layer,
            overhead_mb,
            description: format!("estimated profile ({})", filename),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerAssignment {
    pub model: String,
    pub total_layers: u32,
    pub start_layer: u32, // inclusive
    pub end_layer: u32,   // inclusive
    pub layers_count: u32,
    pub memory_used_mb: f64,
    pub memory_allocated_mb: f64,
}

impl LayerAssignment {
    pub fn layer_range(&self) -> String {
        format!("{}-{}", self.start_layer, self.end_layer)
    }

    pub fn contains_layer(&self, layer: u32) -> bool {
        layer >= self.start_layer && layer <= self.end_layer
    }
}

/// Decides how many layers fit in the given memory, and which layers to take
/// given what's already covered by peers in the network.
///
/// `start_override` pins the starting layer instead of using the gap-filling
/// algorithm — useful when you want explicit control over layer placement.
pub fn calculate_assignment(
    profile: &ModelProfile,
    memory_mb: f64,
    covered_layers: &[u32], // layers already claimed by other peers
    start_override: Option<u32>,
) -> LayerAssignment {
    let usable_mb = memory_mb - profile.overhead_mb.min(memory_mb * 0.2);
    let max_layers = ((usable_mb / profile.memory_per_layer_mb).floor() as u32)
        .min(profile.total_layers);

    let start_layer = match start_override {
        Some(s) => s.min(profile.total_layers - 1),
        None => find_best_start(profile.total_layers, max_layers, covered_layers),
    };
    let end_layer = (start_layer + max_layers - 1).min(profile.total_layers - 1);
    let actual_count = end_layer - start_layer + 1;

    LayerAssignment {
        model: profile.name.clone(),
        total_layers: profile.total_layers,
        start_layer,
        end_layer,
        layers_count: actual_count,
        memory_used_mb: actual_count as f64 * profile.memory_per_layer_mb + profile.overhead_mb,
        memory_allocated_mb: memory_mb,
    }
}

fn find_best_start(total_layers: u32, want_layers: u32, covered: &[u32]) -> u32 {
    if covered.is_empty() {
        return 0;
    }

    // Try to find contiguous uncovered range
    let mut best_start = 0u32;
    let mut best_uncovered = 0u32;

    for candidate_start in 0..total_layers {
        let candidate_end = (candidate_start + want_layers - 1).min(total_layers - 1);
        let range: Vec<u32> = (candidate_start..=candidate_end).collect();
        let uncovered = range.iter().filter(|l| !covered.contains(l)).count() as u32;

        if uncovered > best_uncovered {
            best_uncovered = uncovered;
            best_start = candidate_start;
        }
        if best_uncovered == want_layers {
            break;
        }
    }

    best_start
}

pub fn parse_memory(s: &str) -> anyhow::Result<f64> {
    let s = s.trim().to_uppercase();
    if let Some(n) = s.strip_suffix("GB") {
        Ok(n.trim().parse::<f64>()? * 1024.0)
    } else if let Some(n) = s.strip_suffix("MB") {
        Ok(n.trim().parse::<f64>()?)
    } else {
        // assume GB if no unit
        Ok(s.parse::<f64>()? * 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gemma3_10gb() {
        let profile = ModelProfile::find("gemma3").unwrap();
        let assignment = calculate_assignment(&profile, 10.0 * 1024.0, &[], None);
        println!("{:?}", assignment);
        assert!(assignment.layers_count > 0);
        assert!(assignment.layers_count <= profile.total_layers);
    }

    #[test]
    fn test_gap_filling() {
        let profile = ModelProfile::find("gemma3").unwrap();
        let covered: Vec<u32> = (0..5).collect();
        let assignment = calculate_assignment(&profile, 10.0 * 1024.0, &covered, None);
        println!("Gap fill: {:?}", assignment);
        assert!(assignment.start_layer >= 5 || assignment.end_layer >= 5);
    }

    #[test]
    fn test_start_override() {
        let profile = ModelProfile::find("gemma3").unwrap();
        let assignment = calculate_assignment(&profile, 4.0 * 1024.0, &[], Some(10));
        assert_eq!(assignment.start_layer, 10);
        assert!(assignment.end_layer >= 10);
    }
}
