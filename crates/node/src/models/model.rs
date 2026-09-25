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
    /// A implantação de que este nó faz parte, quando ele veio de uma.
    ///
    /// Vazio no que um peer de versão anterior anuncia - e é justamente esse
    /// vazio que faz o agrupamento cair de volta no rótulo do modelo. Ver
    /// `deployment.rs`.
    #[serde(default)]
    pub deploy_id: String,
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
