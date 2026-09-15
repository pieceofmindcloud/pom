//! Seleção do caminho de inferência a partir do cabeçalho GGUF.
//!
//! A seleção é deliberadamente pequena: o caminho denso continua sendo o
//! `llama.cpp` existente, e o caminho MoE entra no runtime próprio de experts
//! Q4_0. A atenção e as operações que ainda não são próprias continuam no
//! contexto local, mas o FFN roteado não é delegado silenciosamente.

use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InferenceMode {
    /// llama.cpp é o padrão seguro do produto; Auto continua sendo um modo
    /// explícito.
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
            other => bail!("modo de inferência inválido '{other}'; use auto, dense ou moe"),
        }
    }

    /// Read the product-derived `POM_INFERENCE_MODE` override for an explicit
    /// `auto` request. Absence keeps that explicit request metadata-driven;
    /// missing requests use the `Dense` default before reaching this method.
    pub fn from_env() -> Result<Self> {
        match crate::brand::env("INFERENCE_MODE") {
            Ok(value) if !value.trim().is_empty() => Self::parse(&value),
            Ok(_) | Err(std::env::VarError::NotPresent) => Ok(Self::Auto),
            Err(error) => bail!("não foi possível ler POM_INFERENCE_MODE: {error}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InferencePath {
    #[default]
    DenseLlamaCpp,
    MoeQ4_0,
}

/// O motor que de fato executou um hop, separado do caminho selecionado.
///
/// Este valor vai para a telemetria por hop. `Unknown` é deliberado: um peer
/// antigo ou uma resposta sem o campo não autoriza a interface a adivinhar
/// que foi llama.cpp ou o runtime MoE.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub enum ExecutionEngine {
    #[serde(rename = "llama.cpp")]
    LlamaCpp,
    #[serde(rename = "moe")]
    Moe,
    #[serde(rename = "unknown")]
    #[default]
    Unknown,
}

impl ExecutionEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama.cpp",
            Self::Moe => "moe",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_telemetry(value: &str) -> Self {
        match value {
            "llama.cpp" | "moe/llama.cpp-delegated" => Self::LlamaCpp,
            "moe" => Self::Moe,
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

    /// Retorna o executor real desta seleção. Não deve ser reconstruído na
    /// interface a partir de `as_str()` ou da arquitetura do modelo.
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
            bail!("modo moe exige metadados GGUF com expert_count > 1; modelo denso recusado")
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
            "telemetria deve refletir o executor denso"
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
        assert!(error.to_string().contains("modelo denso recusado"));
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
