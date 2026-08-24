//! Runtime local que entrega estados ocultos ao bloco MoE Q4_0.
//!
//! O runtime não cria um segundo protocolo: recebe o mesmo vetor f32 que
//! `decode_hidden` já aceitou do peer e devolve outro vetor da mesma forma.
//! Atenção e o restante das camadas continuam no contexto llama.cpp; aqui a
//! fronteira própria é o router GGUF mais o FFN esparso da camada local.

use super::block::{
    IqQuantMoeBlock, LegacyQuantMoeBlock, LowBitMoeBlock, MoeBlockError, Nvfp4MoeBlock,
    PlainMoeBlock, Q4_0MoeBlock,
};
use super::cache::{CacheError, ExpertCache};
use super::format_iq_quants::{IqQuantFormat, IqQuantSource};
use super::format_k_quants::{KQuantFormat, KQuantSource};
use super::format_legacy_quants::{LegacyQuantFormat, LegacyQuantSource};
use super::format_low_bits::{LowBitFormat, LowBitSource};
use super::format_nvfp4::Nvfp4Source;
use super::format_plain::{PlainFormat, PlainSource};
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
    Legacy(LegacyQuantSource),
    Plain(PlainSource),
    Q4(Q4_0Source),
    K(KQuantSource),
    Iq(IqQuantSource),
    Nvfp4(Nvfp4Source),
    LowBit(LowBitSource),
}

impl MoeSource {
    fn hidden(&self) -> usize {
        match self {
            Self::Legacy(source) => source.hidden(),
            Self::Plain(source) => source.hidden(),
            Self::Q4(source) => source.hidden(),
            Self::K(source) => source.hidden(),
            Self::Iq(source) => source.hidden(),
            Self::Nvfp4(source) => source.hidden(),
            Self::LowBit(source) => source.hidden(),
        }
    }
}

enum MoeBlock {
    Legacy(LegacyQuantMoeBlock),
    Plain(PlainMoeBlock),
    Q4(Q4_0MoeBlock),
    K(super::block::KQuantMoeBlock),
    Iq(IqQuantMoeBlock),
    Nvfp4(Nvfp4MoeBlock),
    LowBit(LowBitMoeBlock),
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
            ExpertFormat::Q4_1 | ExpertFormat::Q5_0 | ExpertFormat::Q5_1 | ExpertFormat::Q8_0 => {
                let format = match detected {
                    ExpertFormat::Q4_1 => LegacyQuantFormat::Q4_1,
                    ExpertFormat::Q5_0 => LegacyQuantFormat::Q5_0,
                    ExpertFormat::Q5_1 => LegacyQuantFormat::Q5_1,
                    ExpertFormat::Q8_0 => LegacyQuantFormat::Q8_0,
                    _ => unreachable!("detect_expert_format mudou após seleção legacy"),
                };
                let source = LegacyQuantSource::open_infer(&path, experts)?;
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
                    blocks.push(MoeBlock::Legacy(
                        LegacyQuantMoeBlock::new(
                            Arc::clone(&cache),
                            &model,
                            layer,
                            top_k,
                            format,
                            source.hidden(),
                            source.intermediate(),
                        )
                        .map_err(MoeRuntimeError::Block)?,
                    ));
                }
                Ok(Self {
                    source: MoeSource::Legacy(source),
                    blocks,
                })
            }
            ExpertFormat::F32 | ExpertFormat::F16 | ExpertFormat::Bf16 => {
                let format = match detected {
                    ExpertFormat::F32 => PlainFormat::F32,
                    ExpertFormat::F16 => PlainFormat::F16,
                    ExpertFormat::Bf16 => PlainFormat::Bf16,
                    _ => unreachable!("detect_expert_format mudou após seleção escalar"),
                };
                let source = PlainSource::open_infer(&path, experts)?;
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
                    blocks.push(MoeBlock::Plain(
                        PlainMoeBlock::new(
                            Arc::clone(&cache),
                            &model,
                            layer,
                            top_k,
                            format,
                            source.hidden(),
                            source.intermediate(),
                        )
                        .map_err(MoeRuntimeError::Block)?,
                    ));
                }
                Ok(Self {
                    source: MoeSource::Plain(source),
                    blocks,
                })
            }
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
            ExpertFormat::Mxfp4
            | ExpertFormat::Tq1_0
            | ExpertFormat::Tq2_0
            | ExpertFormat::Q1_0
            | ExpertFormat::Q2_0 => {
                let format = match detected {
                    ExpertFormat::Mxfp4 => LowBitFormat::Mxfp4,
                    ExpertFormat::Tq1_0 => LowBitFormat::Tq1_0,
                    ExpertFormat::Tq2_0 => LowBitFormat::Tq2_0,
                    ExpertFormat::Q1_0 => LowBitFormat::Q1_0,
                    ExpertFormat::Q2_0 => LowBitFormat::Q2_0,
                    _ => unreachable!("detect_expert_format mudou após seleção low-bit"),
                };
                let source = LowBitSource::open_infer(&path, experts)?;
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
                    blocks.push(MoeBlock::LowBit(
                        LowBitMoeBlock::new(
                            Arc::clone(&cache),
                            &model,
                            layer,
                            top_k,
                            format,
                            source.hidden(),
                            source.intermediate(),
                        )
                        .map_err(MoeRuntimeError::Block)?,
                    ));
                }
                Ok(Self {
                    source: MoeSource::LowBit(source),
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
            ExpertFormat::Iq3S | ExpertFormat::Iq2S | ExpertFormat::Iq4XS => {
                let format = match detected {
                    ExpertFormat::Iq3S => IqQuantFormat::Iq3S,
                    ExpertFormat::Iq2S => IqQuantFormat::Iq2S,
                    ExpertFormat::Iq4XS => IqQuantFormat::Iq4XS,
                    _ => unreachable!("detect_expert_format mudou após a seleção IQ-quant"),
                };
                let source = IqQuantSource::open_infer(&path, experts)?;
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
                    blocks.push(MoeBlock::Iq(
                        IqQuantMoeBlock::new(
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
                    source: MoeSource::Iq(source),
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
                (MoeSource::Legacy(source), MoeBlock::Legacy(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::Plain(source), MoeBlock::Plain(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::Q4(source), MoeBlock::Q4(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::K(source), MoeBlock::K(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::Iq(source), MoeBlock::Iq(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::Nvfp4(source), MoeBlock::Nvfp4(block)) => {
                    block.forward_from_source(source, token_hidden)?
                }
                (MoeSource::LowBit(source), MoeBlock::LowBit(block)) => {
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
    use super::super::format_iq_quants::{tests::write_iq_source, IqQuantFormat};
    use super::super::format_k_quants::tests::write_k_source;
    use super::super::format_legacy_quants::{tests::write_legacy_source, LegacyQuantFormat};
    use super::super::format_low_bits::{tests::write_low_bit_source, LowBitFormat};
    use super::super::format_plain::{tests::write_plain_source, PlainFormat};
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

    #[test]
    fn runtime_carrega_e_executa_iq3_s_iq2_s_e_iq4_xs() {
        for (dtype, format) in [
            (21u32, IqQuantFormat::Iq3S),
            (22u32, IqQuantFormat::Iq2S),
            (23u32, IqQuantFormat::Iq4XS),
        ] {
            let path = std::env::temp_dir().join(format!(
                "pom-ftk-iq-runtime-{dtype}-{}.gguf",
                std::process::id()
            ));
            write_iq_source(&path, dtype, format, false);
            let runtime = MoeRuntime::open(&path, "fixture", 2, 1, 0).unwrap();
            assert_eq!(runtime.layers(), 1, "formato {}", format.name());
            let output = runtime.forward_batch(0, &[1.0; 512], 2).unwrap();
            assert_eq!(output.len(), 512, "formato {}", format.name());
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn runtime_carrega_e_executa_f32_f16_e_bf16() {
        for format in [PlainFormat::F32, PlainFormat::F16, PlainFormat::Bf16] {
            let path = std::env::temp_dir().join(format!(
                "pom-ftk-plain-runtime-{}-{}.gguf",
                format.name(),
                std::process::id()
            ));
            write_plain_source(&path, format, false);
            let runtime = MoeRuntime::open(&path, "fixture", 2, 1, 0).unwrap();
            let output = runtime.forward_batch(0, &[1.0; 4], 2).unwrap();
            assert_eq!(output.len(), 4, "formato {}", format.name());
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn runtime_carrega_e_executa_q4_1_q5_0_q5_1_e_q8_0() {
        for format in [
            LegacyQuantFormat::Q4_1,
            LegacyQuantFormat::Q5_0,
            LegacyQuantFormat::Q5_1,
            LegacyQuantFormat::Q8_0,
        ] {
            let path = std::env::temp_dir().join(format!(
                "pom-ftk-legacy-runtime-{}-{}.gguf",
                format.name(),
                std::process::id()
            ));
            write_legacy_source(&path, format, false);
            let runtime = MoeRuntime::open(&path, "fixture", 2, 1, 0).unwrap();
            let output = runtime.forward_batch(0, &[1.0; 64], 2).unwrap();
            assert_eq!(output.len(), 64, "formato {}", format.name());
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn runtime_carrega_e_executa_mxfp4_tq1_tq2_q1_e_q2() {
        for format in [
            LowBitFormat::Mxfp4,
            LowBitFormat::Tq1_0,
            LowBitFormat::Tq2_0,
            LowBitFormat::Q1_0,
            LowBitFormat::Q2_0,
        ] {
            let path = std::env::temp_dir().join(format!(
                "pom-ftk-low-bit-runtime-{}-{}.gguf",
                format.name(),
                std::process::id()
            ));
            write_low_bit_source(&path, format, false);
            let runtime = MoeRuntime::open(&path, "fixture", 2, 1, 0).unwrap();
            let output = runtime.forward_batch(0, &[1.0; 512], 2).unwrap();
            assert_eq!(output.len(), 512, "formato {}", format.name());
            std::fs::remove_file(path).unwrap();
        }
    }
}
