//! Bloco esparso que conecta router, cache e executor Q4_0.
//!
//! O bloco recebe uma ativação e os logits produzidos pela camada de router.
//! A atenção e a projeção do router continuam no runtime que o integrar; esta
//! fronteira já executa a parte específica de experts sem passar pelo
//! `llama.cpp`.

use super::bandwidth::{BandwidthError, BandwidthProfile};
use super::cache::{ExpertCache, ExpertKey};
use super::coexecution::{run_overlap, OverlapError};
use super::format_q4_0::{Q4Error, Q4ExecuteError, Q4_0Executor, Q4_0Source};
use super::router::{dispatch_top_k, select_top_k, DispatchError, ExpertRoute, RouteError};
use std::fmt;
use std::sync::Arc;

#[derive(Debug)]
pub enum MoeBlockError {
    Route(RouteError),
    Executor(Q4ExecuteError),
}

impl fmt::Display for MoeBlockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Route(error) => error.fmt(formatter),
            Self::Executor(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MoeBlockError {}

impl From<Q4ExecuteError> for MoeBlockError {
    fn from(error: Q4ExecuteError) -> Self {
        Self::Executor(error)
    }
}

#[derive(Debug)]
pub enum MoeOverlapError<GE> {
    Route(RouteError),
    Profile(BandwidthError),
    Cpu(Q4ExecuteError),
    Gpu(GE),
    WorkerPanicked,
    OutputCount { expected: usize, actual: usize },
    MissingOutput { expert: u32 },
}

impl<GE: fmt::Display> fmt::Display for MoeOverlapError<GE> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Route(error) => error.fmt(formatter),
            Self::Profile(error) => error.fmt(formatter),
            Self::Cpu(error) => write!(formatter, "executor CPU falhou: {error}"),
            Self::Gpu(error) => write!(formatter, "executor GPU falhou: {error}"),
            Self::WorkerPanicked => write!(formatter, "worker de co-execução entrou em panic"),
            Self::OutputCount { expected, actual } => write!(
                formatter,
                "GPU devolveu {actual} outputs, esperados {expected}"
            ),
            Self::MissingOutput { expert } => {
                write!(formatter, "executor não devolveu output do expert {expert}")
            }
        }
    }
}

impl<GE: fmt::Debug + fmt::Display> std::error::Error for MoeOverlapError<GE> {}

/// Forward de um bloco MoE Q4_0 para uma camada GGUF.
pub struct Q4_0MoeBlock {
    model: String,
    layer: u32,
    top_k: usize,
    executor: Q4_0Executor,
}

impl Q4_0MoeBlock {
    pub fn new(
        cache: Arc<ExpertCache>,
        model: impl Into<String>,
        layer: u32,
        top_k: usize,
        hidden: usize,
        intermediate: usize,
    ) -> Result<Self, Q4Error> {
        Ok(Self {
            model: model.into(),
            layer,
            top_k,
            executor: Q4_0Executor::new(cache, hidden, intermediate)?,
        })
    }

    pub fn forward(
        &self,
        source: &Q4_0Source,
        hidden: &[f32],
        router_logits: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let model = &self.model;
        let layer = self.layer;
        let output = dispatch_top_k(router_logits, self.top_k, |expert| {
            let key = ExpertKey::new(model, layer, expert, "q4_0");
            self.executor.run_from_source(source, key, layer, hidden)
        });
        output.map_err(|error| match error {
            DispatchError::Route(error) => MoeBlockError::Route(error),
            DispatchError::Expert(error) => MoeBlockError::Executor(error),
        })
    }

    /// Forward com misses divididos pela banda medida entre CPU e GPU.
    ///
    /// O callback GPU recebe os IDs na ordem da lista de misses e devolve um
    /// output por ID. Ele representa tanto a cópia quanto o kernel; o bloco
    /// só combina os resultados depois que as duas lanes terminam.
    pub fn forward_with_gpu<GE, GF>(
        &self,
        source: &Q4_0Source,
        hidden: &[f32],
        router_logits: &[f32],
        profile: BandwidthProfile,
        gpu: GF,
    ) -> Result<Vec<f32>, MoeOverlapError<GE>>
    where
        GE: Send,
        GF: FnOnce(&[u32]) -> Result<Vec<Vec<f32>>, GE> + Send,
    {
        let route = select_top_k(router_logits, self.top_k).map_err(MoeOverlapError::Route)?;
        let split = profile
            .split_misses(&route.ids)
            .map_err(MoeOverlapError::Profile)?;
        let model = &self.model;
        let layer = self.layer;
        let (cpu, gpu) = run_overlap(
            || {
                split
                    .cpu
                    .iter()
                    .map(|&expert| {
                        let key = ExpertKey::new(model, layer, expert, "q4_0");
                        self.executor
                            .run_from_source(source, key, layer, hidden)
                            .map(|output| (expert, output))
                    })
                    .collect::<Result<Vec<_>, Q4ExecuteError>>()
            },
            || gpu(&split.fetched),
        )
        .map_err(|error| match error {
            OverlapError::Cpu(error) => MoeOverlapError::Cpu(error),
            OverlapError::Gpu(error) => MoeOverlapError::Gpu(error),
            OverlapError::WorkerPanicked => MoeOverlapError::WorkerPanicked,
        })?;

        if gpu.len() != split.fetched.len() {
            return Err(MoeOverlapError::OutputCount {
                expected: split.fetched.len(),
                actual: gpu.len(),
            });
        }
        let mut outputs = cpu;
        outputs.extend(split.fetched.iter().copied().zip(gpu));
        combine_route(&route, outputs)
    }
}

fn combine_route<GE>(
    route: &ExpertRoute,
    outputs: Vec<(u32, Vec<f32>)>,
) -> Result<Vec<f32>, MoeOverlapError<GE>> {
    let mut combined = None::<Vec<f32>>;
    for (&expert, &weight) in route.ids.iter().zip(&route.weights) {
        let values = outputs
            .iter()
            .find_map(|(id, values)| (*id == expert).then_some(values))
            .ok_or(MoeOverlapError::MissingOutput { expert })?;
        let target = combined.get_or_insert_with(|| vec![0.0; values.len()]);
        if target.len() != values.len() {
            return Err(MoeOverlapError::Route(RouteError::OutputWidth {
                expected: target.len(),
                actual: values.len(),
            }));
        }
        for (slot, value) in target.iter_mut().zip(values) {
            *slot += weight * value;
        }
    }
    combined.ok_or(MoeOverlapError::Route(RouteError::InvalidWeightSum))
}

#[cfg(test)]
mod tests {
    use super::super::admission::MemoryAccount;
    use super::super::cache::{ExpertCache, FreeMemoryProbe};
    use super::super::format_q4_0::{tests::write_q4_source, Q4_0Source};
    use super::*;
    use std::io;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FixedProbe(AtomicU64);

    impl FixedProbe {
        fn new(bytes: u64) -> Arc<Self> {
            Arc::new(Self(AtomicU64::new(bytes)))
        }
    }

    impl FreeMemoryProbe for FixedProbe {
        fn free_bytes(&self) -> io::Result<u64> {
            Ok(self.0.load(Ordering::Relaxed))
        }
    }

    #[test]
    fn bloco_rota_top_k_e_reutiliza_cache_global() {
        let path = PathBuf::from(std::env::temp_dir())
            .join(format!("pom-ftk-q4-block-{}.gguf", std::process::id()));
        let (hidden, intermediate) = write_q4_source(&path);
        let source = Q4_0Source::open(&path, 2, hidden, intermediate).unwrap();
        let cache = ExpertCache::new(
            MemoryAccount {
                baseline_free_bytes: 100_000,
                weights_bytes: 0,
                fixed_bytes: 0,
                reserve_bytes: 0,
            },
            FixedProbe::new(100_000),
        );
        let block =
            Q4_0MoeBlock::new(cache.clone(), "fixture", 0, 1, hidden, intermediate).unwrap();
        let first = block.forward(&source, &[1.0; 32], &[2.0, 1.0]).unwrap();
        let second = block.forward(&source, &[1.0; 32], &[2.0, 1.0]).unwrap();
        assert_eq!(first, second);
        assert_eq!(cache.stats().admissions, 1);
        assert_eq!(cache.stats().hits, 1);
        drop(source);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bloco_divide_experts_e_soma_cpu_com_gpu() {
        let path = PathBuf::from(std::env::temp_dir())
            .join(format!("pom-ftk-q4-overlap-{}.gguf", std::process::id()));
        let (hidden, intermediate) = write_q4_source(&path);
        let source = Q4_0Source::open(&path, 2, hidden, intermediate).unwrap();
        let cache = ExpertCache::new(
            MemoryAccount {
                baseline_free_bytes: 100_000,
                weights_bytes: 0,
                fixed_bytes: 0,
                reserve_bytes: 0,
            },
            FixedProbe::new(100_000),
        );
        let block = Q4_0MoeBlock::new(cache, "fixture", 0, 2, hidden, intermediate).unwrap();
        let profile = BandwidthProfile {
            cpu_overlap_bytes_per_sec: 1.0,
            transfer_overlap_bytes_per_sec: 1.0,
            cpu_bytes_per_sec: 1.0,
            transfer_bytes_per_sec: 1.0,
        };
        let output = block
            .forward_with_gpu(&source, &[1.0; 32], &[2.0, 1.0], profile, |ids| {
                Ok::<_, &'static str>(ids.iter().map(|&id| vec![id as f32; 32]).collect())
            })
            .unwrap();
        assert_eq!(output.len(), 32);
        assert!(output.iter().all(|value| value.is_finite()));
        drop(source);
        std::fs::remove_file(path).unwrap();
    }
}
