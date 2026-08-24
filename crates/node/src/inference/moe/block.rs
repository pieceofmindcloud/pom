//! Bloco esparso que conecta router, cache e executor Q4_0.
//!
//! O bloco recebe uma ativação e os logits produzidos pela camada de router.
//! A projeção do router pode vir do tensor GGUF local, sem mudar o wire entre
//! peers; a atenção continua no runtime que entrega a ativação a esta fronteira.

use super::cache::{ExpertCache, ExpertKey};
use super::format_q4_0::{Q4Error, Q4ExecuteError, Q4_0Executor, Q4_0Source};
use super::router::{dispatch_top_k, DispatchError, RouteError};
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

    pub fn forward_from_source(
        &self,
        source: &Q4_0Source,
        hidden: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let logits = source
            .router_logits(self.layer, hidden)
            .map_err(Q4ExecuteError::Source)?;
        self.forward(source, hidden, &logits)
    }
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
        let first = block.forward_from_source(&source, &[1.0; 32]).unwrap();
        let second = block.forward_from_source(&source, &[1.0; 32]).unwrap();
        assert_eq!(first, second);
        drop(source);
        std::fs::remove_file(path).unwrap();
    }
}
