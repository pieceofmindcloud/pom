//! The engine that ran each inference phase, for the per-hop telemetry.
//!
//! llama.cpp runs every model the POM itself serves (`LLAMA_CPP_PATH`); a
//! plugin's inference service runs a whole model instead when the deployment
//! names one (`crate::plugin_engine`).

/// The `selected_path` a llama.cpp inference server reports in `/health` and
/// in its per-hop telemetry.
pub const LLAMA_CPP_PATH: &str = "dense/llama.cpp";

/// The engine that actually executed a hop.
///
/// `Unknown` is deliberate: an old peer or a response without the field does
/// not allow the interface to guess which engine ran it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub enum ExecutionEngine {
    #[serde(rename = "llama.cpp")]
    LlamaCpp,
    /// A plugin's inference service ran the phase (`plugin_engine`).
    #[serde(rename = "plugin")]
    Plugin,
    #[serde(rename = "unknown")]
    #[default]
    Unknown,
}

impl ExecutionEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama.cpp",
            Self::Plugin => "plugin",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_telemetry(value: &str) -> Self {
        match value {
            "llama.cpp" | "moe/llama.cpp-delegated" => Self::LlamaCpp,
            "plugin" => Self::Plugin,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_reads_llama_cpp_and_leaves_the_rest_unknown() {
        assert_eq!(
            ExecutionEngine::from_telemetry("llama.cpp"),
            ExecutionEngine::LlamaCpp
        );
        assert_eq!(
            ExecutionEngine::from_telemetry("moe/llama.cpp-delegated"),
            ExecutionEngine::LlamaCpp
        );
        assert_eq!(
            ExecutionEngine::from_telemetry("peer-antigo-sem-campo"),
            ExecutionEngine::Unknown
        );
    }
}
