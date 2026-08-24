//! Bloco esparso que conecta router, cache e executor Q4_0.
//!
//! O bloco recebe uma ativação e os logits produzidos pela camada de router.
//! A projeção do router pode vir do tensor GGUF local, sem mudar o wire entre
//! peers; a atenção continua no runtime que entrega a ativação a esta fronteira.

use super::cache::{ExpertCache, ExpertKey};
use super::format_iq_quants::{
    IqQuantError, IqQuantExecuteError, IqQuantExecutor, IqQuantFormat, IqQuantSource,
};
use super::format_k_quants::{
    KQuantError, KQuantExecuteError, KQuantExecutor, KQuantFormat, KQuantSource,
};
use super::format_nvfp4::{Nvfp4ExecuteError, Nvfp4Executor, Nvfp4Source};
use super::format_plain::{PlainExecuteError, PlainExecutor, PlainFormat, PlainSource};
use super::format_q4_0::{Q4Error, Q4ExecuteError, Q4_0Executor, Q4_0Source};
use super::router::{dispatch_top_k, DispatchError, RouteError};
use std::fmt;
use std::sync::Arc;

#[derive(Debug)]
pub enum MoeBlockError {
    Route(RouteError),
    Executor(String),
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
        Self::Executor(error.to_string())
    }
}

impl From<Nvfp4ExecuteError> for MoeBlockError {
    fn from(error: Nvfp4ExecuteError) -> Self {
        Self::Executor(error.to_string())
    }
}

impl From<KQuantExecuteError> for MoeBlockError {
    fn from(error: KQuantExecuteError) -> Self {
        Self::Executor(error.to_string())
    }
}

impl From<IqQuantExecuteError> for MoeBlockError {
    fn from(error: IqQuantExecuteError) -> Self {
        Self::Executor(error.to_string())
    }
}

impl From<PlainExecuteError> for MoeBlockError {
    fn from(error: PlainExecuteError) -> Self {
        Self::Executor(error.to_string())
    }
}

/// Forward de um bloco MoE escalar F32, F16 ou BF16 para uma camada GGUF.
pub struct PlainMoeBlock {
    model: String,
    layer: u32,
    top_k: usize,
    format: PlainFormat,
    executor: PlainExecutor,
}

impl PlainMoeBlock {
    pub fn new(
        cache: Arc<ExpertCache>,
        model: impl Into<String>,
        layer: u32,
        top_k: usize,
        format: PlainFormat,
        hidden: usize,
        intermediate: usize,
    ) -> Result<Self, String> {
        Ok(Self {
            model: model.into(),
            layer,
            top_k,
            format,
            executor: PlainExecutor::new(cache, format, hidden, intermediate)
                .map_err(|error| error.to_string())?,
        })
    }

    pub fn forward_from_source(
        &self,
        source: &PlainSource,
        hidden: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let logits = source
            .router_logits(self.layer, hidden)
            .map_err(PlainExecuteError::Source)?;
        let model = &self.model;
        let layer = self.layer;
        let format = self.format.name();
        let output = dispatch_top_k(&logits, self.top_k, |expert| {
            let key = ExpertKey::new(model, layer, expert, format);
            self.executor.run_from_source(source, key, layer, hidden)
        });
        output.map_err(|error| match error {
            DispatchError::Route(error) => MoeBlockError::Route(error),
            DispatchError::Expert(error) => MoeBlockError::Executor(error.to_string()),
        })
    }
}

/// Forward de um bloco MoE IQ3_S, IQ2_S ou IQ4_XS para uma camada GGUF.
pub struct IqQuantMoeBlock {
    model: String,
    layer: u32,
    top_k: usize,
    format: IqQuantFormat,
    executor: IqQuantExecutor,
}

impl IqQuantMoeBlock {
    pub fn new(
        cache: Arc<ExpertCache>,
        model: impl Into<String>,
        layer: u32,
        top_k: usize,
        format: IqQuantFormat,
        hidden: usize,
        intermediate: usize,
    ) -> Result<Self, IqQuantError> {
        Ok(Self {
            model: model.into(),
            layer,
            top_k,
            format,
            executor: IqQuantExecutor::new(cache, format, hidden, intermediate)?,
        })
    }

    pub fn forward_from_source(
        &self,
        source: &IqQuantSource,
        hidden: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let logits = source
            .router_logits(self.layer, hidden)
            .map_err(IqQuantExecuteError::Source)?;
        let model = &self.model;
        let layer = self.layer;
        let output = dispatch_top_k(&logits, self.top_k, |expert| {
            let key = ExpertKey::new(model, layer, expert, self.format.name());
            self.executor.run_from_source(source, key, layer, hidden)
        });
        output.map_err(|error| match error {
            DispatchError::Route(error) => MoeBlockError::Route(error),
            DispatchError::Expert(error) => MoeBlockError::Executor(error.to_string()),
        })
    }
}

/// Forward de um bloco MoE Q4_K ou Q6_K para uma camada GGUF.
pub struct KQuantMoeBlock {
    model: String,
    layer: u32,
    top_k: usize,
    format: KQuantFormat,
    executor: KQuantExecutor,
}

impl KQuantMoeBlock {
    pub fn new(
        cache: Arc<ExpertCache>,
        model: impl Into<String>,
        layer: u32,
        top_k: usize,
        format: KQuantFormat,
        hidden: usize,
        intermediate: usize,
    ) -> Result<Self, KQuantError> {
        Ok(Self {
            model: model.into(),
            layer,
            top_k,
            format,
            executor: KQuantExecutor::new(cache, format, hidden, intermediate)?,
        })
    }

    pub fn forward_from_source(
        &self,
        source: &KQuantSource,
        hidden: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let logits = source
            .router_logits(self.layer, hidden)
            .map_err(KQuantExecuteError::Source)?;
        let model = &self.model;
        let layer = self.layer;
        let format = match self.format {
            KQuantFormat::Q4K => "q4_k",
            KQuantFormat::Q6K => "q6_k",
        };
        let output = dispatch_top_k(&logits, self.top_k, |expert| {
            let key = ExpertKey::new(model, layer, expert, format);
            self.executor.run_from_source(source, key, layer, hidden)
        });
        output.map_err(|error| match error {
            DispatchError::Route(error) => MoeBlockError::Route(error),
            DispatchError::Expert(error) => MoeBlockError::Executor(error.to_string()),
        })
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
            DispatchError::Expert(error) => MoeBlockError::Executor(error.to_string()),
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

/// Forward de um bloco MoE NVFP4 para uma camada GGUF.
pub struct Nvfp4MoeBlock {
    model: String,
    layer: u32,
    top_k: usize,
    executor: Nvfp4Executor,
}

impl Nvfp4MoeBlock {
    pub fn new(
        cache: Arc<ExpertCache>,
        model: impl Into<String>,
        layer: u32,
        top_k: usize,
        hidden: usize,
        intermediate: usize,
    ) -> Result<Self, String> {
        Ok(Self {
            model: model.into(),
            layer,
            top_k,
            executor: Nvfp4Executor::new(cache, hidden, intermediate)
                .map_err(|error| error.to_string())?,
        })
    }

    pub fn forward_from_source(
        &self,
        source: &Nvfp4Source,
        hidden: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let logits = source
            .router_logits(self.layer, hidden)
            .map_err(|error| MoeBlockError::Executor(error.to_string()))?;
        let model = &self.model;
        let layer = self.layer;
        let output = dispatch_top_k(&logits, self.top_k, |expert| {
            let key = ExpertKey::new(model, layer, expert, "nvfp4");
            self.executor.run_from_source(source, key, layer, hidden)
        });
        output.map_err(|error| match error {
            DispatchError::Route(error) => MoeBlockError::Route(error),
            DispatchError::Expert(error) => MoeBlockError::Executor(error.to_string()),
        })
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
