//! Runtime local que entrega estados ocultos ao bloco MoE Q4_0.
//!
//! O runtime não cria um segundo protocolo: recebe o mesmo vetor f32 que
//! `decode_hidden` já aceitou do peer e devolve outro vetor da mesma forma.
//! Atenção e o restante das camadas continuam no contexto llama.cpp; aqui a
//! fronteira própria é o router GGUF mais o FFN esparso da camada local.

use super::block::{MoeBlockError, Nvfp4MoeBlock, Q4_0MoeBlock};
use super::cache::{CacheError, ExpertCache};
use super::format_k_quants::{KQuantFormat, KQuantSource};
use super::format_nvfp4::Nvfp4Source;
use super::format_q4_0::{detect_expert_format, ExpertFormat};
use super::format_q4_0::{Q4Error, Q4LoadError, Q4_0Source};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug)]
pub enum MoeRuntimeError {
    Source(Q4LoadError),
    Cache(CacheError),
    Format(Q4Error),
    Block(String),
    FormatMismatch,
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
            Self::Block(error) => formatter.write_str(error),
            Self::FormatMismatch => {
                formatter.write_str("fonte e bloco MoE usam formatos diferentes")
            }
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
        Self::Block(error.to_string())
    }
}

enum MoeSource {
    Q4(Q4_0Source),
    K(KQuantSource),
    Nvfp4(Nvfp4Source),
}

impl MoeSource {
    fn hidden(&self) -> usize {
        match self {
            Self::Q4(source) => source.hidden(),
            Self::K(source) => source.hidden(),
            Self::Nvfp4(source) => source.hidden(),
        }
    }
}

enum MoeBlock {
    Q4(Q4_0MoeBlock),
    K(super::block::KQuantMoeBlock),
    Nvfp4(Nvfp4MoeBlock),
}

pub struct MoeRuntime {
    source: MoeSource,
    blocks: Vec<MoeBlock>,
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
        let cache = ExpertCache::measured_system(weights_bytes, 0, 0)?;
        let model = model.into();
        let detected = detect_expert_format(&path)?;
        match detected {
            ExpertFormat::Q4_0 => {
                let source = Q4_0Source::open_infer(&path, experts)?;
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
                let mut blocks = Vec::with_capacity(layers as usize);
                for layer in 0..layers {
                    blocks.push(MoeBlock::Q4(Q4_0MoeBlock::new(
                        Arc::clone(&cache),
                        &model,
                        layer,
                        top_k,
                        source.hidden(),
                        source.intermediate(),
                    )?));
                }
                Ok(Self {
                    source: MoeSource::Q4(source),
                    blocks,
                })
            }
            ExpertFormat::Nvfp4 => {
                let source = Nvfp4Source::open_infer(&path, experts)?;
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
                let mut blocks = Vec::with_capacity(layers as usize);
                for layer in 0..layers {
                    blocks.push(MoeBlock::Nvfp4(
                        Nvfp4MoeBlock::new(
                            Arc::clone(&cache),
                            &model,
                            layer,
                            top_k,
                            source.hidden(),
                            source.intermediate(),
                        )
                        .map_err(MoeRuntimeError::Block)?,
                    ));
                }
                Ok(Self {
                    source: MoeSource::Nvfp4(source),
                    blocks,
                })
            }
            ExpertFormat::Q4K | ExpertFormat::Q6K => {
                let format = match detected {
                    ExpertFormat::Q4K => KQuantFormat::Q4K,
                    ExpertFormat::Q6K => KQuantFormat::Q6K,
                    _ => unreachable!("detect_expert_format mudou após a seleção K-quant"),
                };
                let source = KQuantSource::open_infer(&path, experts)?;
                if source.format() != format {
                    return Err(MoeRuntimeError::FormatMismatch);
                }
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
                let mut blocks = Vec::with_capacity(layers as usize);
                for layer in 0..layers {
                    blocks.push(MoeBlock::K(
                        super::block::KQuantMoeBlock::new(
                            Arc::clone(&cache),
                            &model,
                            layer,
                            top_k,
                            format,
                            source.hidden(),
                            source.intermediate(),
                        )
                        .map_err(|error| MoeRuntimeError::Block(error.to_string()))?,
                    ));
                }
                Ok(Self {
                    source: MoeSource::K(source),
                    blocks,
                })
            }
        }
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
            let values = match (&self.source, block) {
                (MoeSource::Q4(source), MoeBlock::Q4(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::K(source), MoeBlock::K(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::Nvfp4(source), MoeBlock::Nvfp4(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                _ => return Err(MoeRuntimeError::FormatMismatch),
            };
            output.extend(values);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::super::format_k_quants::tests::write_k_source;
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

    #[test]
    fn runtime_carrega_e_executa_q4_k_e_q6_k() {
        for (dtype, expected) in [(12u32, "q4_k"), (14u32, "q6_k")] {
            let path = std::env::temp_dir().join(format!(
                "pom-ftk-k-runtime-{dtype}-{}.gguf",
                std::process::id()
            ));
            write_k_source(
                &path,
                dtype,
                if dtype == 12 {
                    super::super::format_k_quants::KQuantFormat::Q4K
                } else {
                    super::super::format_k_quants::KQuantFormat::Q6K
                },
            );
            let runtime = MoeRuntime::open(&path, "fixture", 2, 1, 0).unwrap();
            assert_eq!(runtime.layers(), 1, "formato {expected}");
            let output = runtime.forward_batch(0, &[1.0; 512], 2).unwrap();
            assert_eq!(output.len(), 512, "formato {expected}");
            std::fs::remove_file(path).unwrap();
        }
    }
}
