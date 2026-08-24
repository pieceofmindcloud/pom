//! Runtime local que entrega estados ocultos ao bloco MoE Q4_0.
//!
//! O runtime não cria um segundo protocolo: recebe o mesmo vetor f32 que
//! `decode_hidden` já aceitou do peer e devolve outro vetor da mesma forma.
//! Atenção e o restante das camadas continuam no contexto llama.cpp; aqui a
//! fronteira própria é o router GGUF mais o FFN esparso da camada local.

use super::block::{MoeBlockError, Q4_0MoeBlock};
use super::cache::{CacheError, ExpertCache};
use super::format_q4_0::{Q4Error, Q4LoadError, Q4_0Source};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug)]
pub enum MoeRuntimeError {
    Source(Q4LoadError),
    Cache(CacheError),
    Format(Q4Error),
    Block(MoeBlockError),
    MissingLayer {
        layer: u32,
        layers: u32,
    },
    InvalidShape {
        values: usize,
        tokens: usize,
        hidden: usize,
    },
}

impl fmt::Display for MoeRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(formatter),
            Self::Cache(error) => error.fmt(formatter),
            Self::Format(error) => error.fmt(formatter),
            Self::Block(error) => error.fmt(formatter),
            Self::MissingLayer { layer, layers } => {
                write!(formatter, "camada MoE local {layer} fora de [0, {layers})")
            }
            Self::InvalidShape {
                values,
                tokens,
                hidden,
            } => write!(
                formatter,
                "runtime MoE recebeu {values} floats, esperado {tokens} × {hidden}"
            ),
        }
    }
}

impl std::error::Error for MoeRuntimeError {}

impl From<Q4LoadError> for MoeRuntimeError {
    fn from(error: Q4LoadError) -> Self {
        Self::Source(error)
    }
}

impl From<CacheError> for MoeRuntimeError {
    fn from(error: CacheError) -> Self {
        Self::Cache(error)
    }
}

impl From<Q4Error> for MoeRuntimeError {
    fn from(error: Q4Error) -> Self {
        Self::Format(error)
    }
}

impl From<MoeBlockError> for MoeRuntimeError {
    fn from(error: MoeBlockError) -> Self {
        Self::Block(error)
    }
}

pub struct MoeRuntime {
    source: Q4_0Source,
    blocks: Vec<Q4_0MoeBlock>,
}

impl MoeRuntime {
    /// Carrega o primeiro formato próprio suportado e compartilha o cache
    /// entre todas as camadas locais desta fatia.
    pub fn open(
        path: impl AsRef<Path>,
        model: impl Into<String>,
        experts: u32,
        top_k: usize,
        weights_bytes: u64,
    ) -> Result<Self, MoeRuntimeError> {
        let source = Q4_0Source::open_infer(path, experts)?;
        if !source.has_router() {
            return Err(MoeRuntimeError::Source(Q4LoadError::MissingTensor(
                "blk.<layer>.ffn_gate_inp.weight".into(),
            )));
        }
        let layers = source.layer_count();
        if layers == 0 {
            return Err(MoeRuntimeError::Source(Q4LoadError::InvalidTensor(
                "GGUF MoE sem camadas",
            )));
        }
        let cache = ExpertCache::measured_system(weights_bytes, 0, 0)?;
        let model = model.into();
        let mut blocks = Vec::with_capacity(layers as usize);
        for layer in 0..layers {
            blocks.push(Q4_0MoeBlock::new(
                Arc::clone(&cache),
                &model,
                layer,
                top_k,
                source.hidden(),
                source.intermediate(),
            )?);
        }
        Ok(Self { source, blocks })
    }

    pub fn layers(&self) -> u32 {
        self.blocks.len() as u32
    }

    pub fn forward_batch(
        &self,
        layer: u32,
        hidden: &[f32],
        tokens: usize,
    ) -> Result<Vec<f32>, MoeRuntimeError> {
        let block = self
            .blocks
            .get(layer as usize)
            .ok_or(MoeRuntimeError::MissingLayer {
                layer,
                layers: self.layers(),
            })?;
        let hidden_width = self.source.hidden();
        let expected = tokens
            .checked_mul(hidden_width)
            .ok_or(MoeRuntimeError::InvalidShape {
                values: hidden.len(),
                tokens,
                hidden: hidden_width,
            })?;
        if tokens == 0 || hidden.len() != expected {
            return Err(MoeRuntimeError::InvalidShape {
                values: hidden.len(),
                tokens,
                hidden: hidden_width,
            });
        }
        let mut output = Vec::with_capacity(hidden.len());
        for token_hidden in hidden.chunks_exact(hidden_width) {
            output.extend(block.forward_from_source(&self.source, token_hidden)?);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::super::format_q4_0::tests::write_q4_source;
    use super::*;

    #[test]
    fn runtime_mantem_forma_do_batch_de_hidden_states() {
        let path =
            std::env::temp_dir().join(format!("pom-ftk-moe-runtime-{}.gguf", std::process::id()));
        let (hidden, _) = write_q4_source(&path);
        let runtime = MoeRuntime::open(&path, "fixture", 2, 1, 0).unwrap();
        let output = runtime.forward_batch(0, &[1.0; 64], 2).unwrap();
        assert_eq!(output.len(), hidden * 2);
        assert!(runtime.forward_batch(1, &[1.0; 32], 1).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
