//! Seleção do caminho de inferência a partir do cabeçalho GGUF.
//!
//! A seleção é deliberadamente pequena: o caminho denso continua sendo o
//! `llama.cpp` existente, e o caminho MoE hoje usa o mesmo motor como delegação
//! reversível. A bifurcação registrada aqui é o ponto que a Fase 4 substituirá
//! pelo executor próprio, sem permitir que um modelo denso caia nele.

use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceMode {
    Auto,
    Dense,
    Moe,
}

impl Default for InferenceMode {
    fn default() -> Self {
        Self::Auto
    }
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

    /// Read the product-derived `POM_INFERENCE_MODE` override. Absence means
    /// automatic metadata classification, preserving the current behavior.
    pub fn from_env() -> Result<Self> {
        match crate::brand::env("INFERENCE_MODE") {
            Ok(value) if !value.trim().is_empty() => Self::parse(&value),
            Ok(_) | Err(std::env::VarError::NotPresent) => Ok(Self::Auto),
            Err(error) => bail!("não foi possível ler POM_INFERENCE_MODE: {error}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferencePath {
    DenseLlamaCpp,
    MoeLlamaCppDelegated,
}

impl InferencePath {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DenseLlamaCpp => "dense/llama.cpp",
            Self::MoeLlamaCppDelegated => "moe/llama.cpp-delegated",
        }
    }

    pub fn is_moe(self) -> bool {
        matches!(self, Self::MoeLlamaCppDelegated)
    }
}

pub fn select(mode: InferenceMode, info: Option<&crate::gguf::GgufInfo>) -> Result<InferencePath> {
    let detected_moe = info
        .map(crate::gguf::GgufInfo::model_architecture)
        .is_some_and(|architecture| architecture.is_moe());
    match mode {
        InferenceMode::Auto if detected_moe => Ok(InferencePath::MoeLlamaCppDelegated),
        InferenceMode::Auto | InferenceMode::Dense => Ok(InferencePath::DenseLlamaCpp),
        InferenceMode::Moe if detected_moe => Ok(InferencePath::MoeLlamaCppDelegated),
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
    }

    #[test]
    fn auto_selects_moe_only_from_expert_metadata() {
        let path = select(InferenceMode::Auto, Some(&moe())).unwrap();
        assert_eq!(path, InferencePath::MoeLlamaCppDelegated);
        assert!(path.is_moe());
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
}
