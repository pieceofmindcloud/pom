//! Selection of the inference path from the GGUF header.
//!
//! The selection is deliberately small: the dense path remains the existing
//! `llama.cpp`, and the MoE path enters the own Q4_0 expert runtime. Attention
//! and the operations that are not yet own remain in the local context, but
//! the routed FFN is not silently delegated.

use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InferenceMode {
    /// llama.cpp is the product's safe default; Auto remains an explicit
    /// mode.
    #[default]
    Dense,
    Auto,
    Moe,
}

impl InferenceMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "dense" | "llama.cpp" | "llama-cpp" => Ok(Self::Dense),
            "moe" => Ok(Self::Moe),
            other => bail!("invalid inference mode '{other}'; use auto, dense or moe"),
        }
    }

    /// Read the product-derived `POM_INFERENCE_MODE` override for an explicit
    /// `auto` request. Absence keeps that explicit request metadata-driven;
    /// missing requests use the `Dense` default before reaching this method.
    pub fn from_env() -> Result<Self> {
        match crate::brand::env("INFERENCE_MODE") {
            Ok(value) if !value.trim().is_empty() => Self::parse(&value),
            Ok(_) | Err(std::env::VarError::NotPresent) => Ok(Self::Auto),
            Err(error) => bail!("could not read POM_INFERENCE_MODE: {error}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InferencePath {
    #[default]
    DenseLlamaCpp,
    MoeQ4_0,
}

/// The engine that actually executed a hop, separate from the selected path.
///
/// This value goes into the per-hop telemetry. `Unknown` is deliberate: an old
/// peer or a response without the field does not allow the interface to guess
/// that it was llama.cpp or the MoE runtime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub enum ExecutionEngine {
    #[serde(rename = "llama.cpp")]
    LlamaCpp,
    #[serde(rename = "moe")]
    Moe,
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
            Self::Moe => "moe",
            Self::Plugin => "plugin",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_telemetry(value: &str) -> Self {
        match value {
            "llama.cpp" | "moe/llama.cpp-delegated" => Self::LlamaCpp,
            "moe" => Self::Moe,
            "plugin" => Self::Plugin,
            _ => Self::Unknown,
        }
    }
}

impl InferencePath {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DenseLlamaCpp => "dense/llama.cpp",
            Self::MoeQ4_0 => "moe/q4_0",
        }
    }

    pub fn is_moe(self) -> bool {
        matches!(self, Self::MoeQ4_0)
    }

    /// Returns the real executor of this selection. It must not be rebuilt in
    /// the interface from `as_str()` or from the model architecture.
    pub fn execution_engine(self) -> ExecutionEngine {
        match self {
            Self::DenseLlamaCpp => ExecutionEngine::LlamaCpp,
            Self::MoeQ4_0 => ExecutionEngine::Moe,
        }
    }
}

pub fn select(mode: InferenceMode, info: Option<&crate::gguf::GgufInfo>) -> Result<InferencePath> {
    let detected_moe = info
        .map(crate::gguf::GgufInfo::model_architecture)
        .is_some_and(|architecture| architecture.is_moe());
    match mode {
        InferenceMode::Auto if detected_moe => Ok(InferencePath::MoeQ4_0),
        InferenceMode::Auto | InferenceMode::Dense => Ok(InferencePath::DenseLlamaCpp),
        InferenceMode::Moe if detected_moe => Ok(InferencePath::MoeQ4_0),
        InferenceMode::Moe => {
            bail!("moe mode requires GGUF metadata with expert_count > 1; dense model refused")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gguf::GgufInfo;

    fn dense() -> GgufInfo {
        GgufInfo {
            architecture: Some("llama".into()),
            ..GgufInfo::default()
        }
    }

    fn moe() -> GgufInfo {
        GgufInfo {
            architecture: Some("qwen3moe".into()),
            expert_count: Some(128),
            expert_used_count: Some(8),
            ..GgufInfo::default()
        }
    }

    #[test]
    fn auto_keeps_dense_models_on_llama_cpp() {
        let path = select(InferenceMode::Auto, Some(&dense())).unwrap();
        assert_eq!(path, InferencePath::DenseLlamaCpp);
        assert!(!path.is_moe());
        assert_eq!(
            path.execution_engine(),
            ExecutionEngine::LlamaCpp,
            "telemetry must reflect the dense executor"
        );
    }

    #[test]
    fn auto_selects_moe_only_from_expert_metadata() {
        let path = select(InferenceMode::Auto, Some(&moe())).unwrap();
        assert_eq!(path, InferencePath::MoeQ4_0);
        assert!(path.is_moe());
        assert_eq!(path.execution_engine(), ExecutionEngine::Moe);
    }

    #[test]
    fn dense_override_never_enters_the_moe_path() {
        let path = select(InferenceMode::Dense, Some(&moe())).unwrap();
        assert_eq!(path, InferencePath::DenseLlamaCpp);
    }

    #[test]
    fn moe_override_rejects_a_dense_model() {
        let error = select(InferenceMode::Moe, Some(&dense())).unwrap_err();
        assert!(error.to_string().contains("dense model refused"));
    }

    #[test]
    fn missing_metadata_is_safe_dense_in_auto_mode() {
        assert_eq!(
            select(InferenceMode::Auto, None).unwrap(),
            InferencePath::DenseLlamaCpp
        );
    }

    #[test]
    fn telemetry_keeps_delegated_moe_on_llama_cpp_and_missing_unknown() {
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
