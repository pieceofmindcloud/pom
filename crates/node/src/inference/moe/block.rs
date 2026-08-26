//! Bloco esparso que conecta router, cache e executor Q4_0.
//!
//! O bloco recebe uma ativação e os logits produzidos pela camada de router.
//! A projeção do router pode vir do tensor GGUF local, sem mudar o wire entre
//! peers; a atenção continua no runtime que entrega a ativação a esta fronteira.

use super::cache::{ExpertCache, ExpertKey};
#[cfg(feature = "cuda")]
use super::cuda::{CudaHeterogeneousExpert, CudaHeterogeneousTensor};
use super::format_heterogeneous::HeterogeneousMoeSource;
#[cfg(feature = "cuda")]
use super::format_heterogeneous::{HeterogeneousTensorPayload, TensorRole};
use super::format_iq_quants::{
    IqQuantError, IqQuantExecuteError, IqQuantExecutor, IqQuantFormat, IqQuantSource,
};
use super::format_k_quants::{
    KQuantError, KQuantExecuteError, KQuantExecutor, KQuantFormat, KQuantSource,
};
use super::format_legacy_quants::{
    LegacyQuantExecuteError, LegacyQuantExecutor, LegacyQuantFormat, LegacyQuantSource,
};
use super::format_low_bits::{LowBitExecuteError, LowBitExecutor, LowBitFormat, LowBitSource};
use super::format_nvfp4::{Nvfp4BackendKind, Nvfp4ExecuteError, Nvfp4Executor, Nvfp4Source};
use super::format_plain::{PlainExecuteError, PlainExecutor, PlainFormat, PlainSource};
use super::format_q4_0::{Q4Error, Q4ExecuteError, Q4_0Executor, Q4_0Source};
use super::router::{dispatch_route, dispatch_top_k, select_top_k, DispatchError, RouteError};
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

impl From<LegacyQuantExecuteError> for MoeBlockError {
    fn from(error: LegacyQuantExecuteError) -> Self {
        Self::Executor(error.to_string())
    }
}

impl From<LowBitExecuteError> for MoeBlockError {
    fn from(error: LowBitExecuteError) -> Self {
        Self::Executor(error.to_string())
    }
}

fn dispatch_resolved<E, F>(
    ids: &[u32],
    weights: &[f32],
    execute: F,
) -> Result<Vec<f32>, MoeBlockError>
where
    E: fmt::Display,
    F: FnMut(u32) -> Result<Vec<f32>, E>,
{
    dispatch_route(ids, weights, execute).map_err(|error| match error {
        DispatchError::Route(error) => MoeBlockError::Route(error),
        DispatchError::Expert(error) => MoeBlockError::Executor(error.to_string()),
    })
}

/// Forward de um bloco MoE Q4_1, Q5_0, Q5_1 ou Q8_0.
pub struct LegacyQuantMoeBlock {
    model: String,
    layer: u32,
    top_k: usize,
    format: LegacyQuantFormat,
    executor: LegacyQuantExecutor,
}

impl LegacyQuantMoeBlock {
    pub fn new(
        cache: Arc<ExpertCache>,
        model: impl Into<String>,
        layer: u32,
        top_k: usize,
        format: LegacyQuantFormat,
        hidden: usize,
        intermediate: usize,
    ) -> Result<Self, String> {
        Ok(Self {
            model: model.into(),
            layer,
            top_k,
            format,
            executor: LegacyQuantExecutor::new(cache, format, hidden, intermediate)
                .map_err(|error| error.to_string())?,
        })
    }

    pub fn forward_from_source(
        &self,
        source: &LegacyQuantSource,
        hidden: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let logits = source
            .router_logits(self.layer, hidden)
            .map_err(LegacyQuantExecuteError::Source)?;
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

    pub fn forward_from_source_with_route(
        &self,
        source: &LegacyQuantSource,
        hidden: &[f32],
        ids: &[u32],
        weights: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let model = &self.model;
        let layer = self.layer;
        let format = self.format.name();
        dispatch_resolved(ids, weights, |expert| {
            let key = ExpertKey::new(model, layer, expert, format);
            self.executor.run_from_source(source, key, layer, hidden)
        })
    }
}

/// Forward de um bloco MoE escalar F32, F16 ou BF16 para uma camada GGUF.
pub struct PlainMoeBlock {
    model: String,
    layer: u32,
    top_k: usize,
    format: PlainFormat,
    executor: PlainExecutor,
    #[cfg(feature = "cuda")]
    cuda: Option<super::cuda::PlainCudaExecutor>,
}

/// Forward de um bloco MoE MXFP4, TQ1_0, TQ2_0, Q1_0 ou Q2_0.
pub struct LowBitMoeBlock {
    model: String,
    layer: u32,
    top_k: usize,
    format: LowBitFormat,
    executor: LowBitExecutor,
}

impl LowBitMoeBlock {
    pub fn new(
        cache: Arc<ExpertCache>,
        model: impl Into<String>,
        layer: u32,
        top_k: usize,
        format: LowBitFormat,
        hidden: usize,
        intermediate: usize,
    ) -> Result<Self, String> {
        Ok(Self {
            model: model.into(),
            layer,
            top_k,
            format,
            executor: LowBitExecutor::new(cache, format, hidden, intermediate)
                .map_err(|error| error.to_string())?,
        })
    }

    pub fn forward_from_source(
        &self,
        source: &LowBitSource,
        hidden: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let logits = source
            .router_logits(self.layer, hidden)
            .map_err(LowBitExecuteError::Source)?;
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

    pub fn forward_from_source_with_route(
        &self,
        source: &LowBitSource,
        hidden: &[f32],
        ids: &[u32],
        weights: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let model = &self.model;
        let layer = self.layer;
        let format = self.format.name();
        dispatch_resolved(ids, weights, |expert| {
            let key = ExpertKey::new(model, layer, expert, format);
            self.executor.run_from_source(source, key, layer, hidden)
        })
    }
}

impl PlainMoeBlock {
    #[cfg(not(feature = "cuda"))]
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
            #[cfg(feature = "cuda")]
            cuda: None,
        })
    }

    #[cfg(feature = "cuda")]
    pub fn new_with_device_cache(
        cache: Arc<ExpertCache>,
        device_cache: Arc<super::cache::DeviceExpertCache>,
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
            cuda: Some(
                super::cuda::PlainCudaExecutor::new_with_cache(
                    format,
                    hidden,
                    intermediate,
                    device_cache,
                )
                .map_err(|error| error.to_string())?,
            ),
        })
    }

    #[cfg(feature = "cuda")]
    pub fn uses_cuda(&self) -> bool {
        self.cuda.is_some()
    }

    #[cfg(feature = "cuda")]
    pub fn cuda_diagnostics(&self) -> super::cuda::Nvfp4CudaDiagnostics {
        self.cuda
            .as_ref()
            .map_or_else(Default::default, |executor| executor.diagnostics())
    }

    #[cfg(feature = "cuda")]
    pub fn resident_cache_stats(&self) -> super::cache::DeviceCacheStats {
        self.cuda
            .as_ref()
            .map_or_else(super::cache::DeviceCacheStats::default, |executor| {
                executor.resident_cache_stats()
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

    pub fn forward_from_source_with_route(
        &self,
        source: &PlainSource,
        hidden: &[f32],
        ids: &[u32],
        weights: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let model = &self.model;
        let layer = self.layer;
        let format = self.format.name();
        dispatch_resolved(ids, weights, |expert| {
            let key = ExpertKey::new(model, layer, expert, format);
            self.executor.run_from_source(source, key, layer, hidden)
        })
    }

    #[cfg(feature = "cuda")]
    pub fn forward_batch_from_source_with_route(
        &self,
        source: &PlainSource,
        hidden: &[f32],
        experts: &[u32],
        token_ids: &[u32],
        weights: &[f32],
        token_offsets: &[usize],
        tokens: usize,
    ) -> Result<Vec<f32>, MoeBlockError> {
        let Some(cuda) = self.cuda.as_ref() else {
            return Err(MoeBlockError::Executor(
                "lote CUDA escalar solicitado sem ponte CUDA ativa".into(),
            ));
        };
        if experts.is_empty()
            || experts.len() != token_ids.len()
            || experts.len() != weights.len()
            || tokens == 0
            || token_offsets.len() != tokens + 1
            || token_offsets.first() != Some(&0)
            || token_offsets.last() != Some(&experts.len())
        {
            return Err(MoeBlockError::Executor(
                "lote CUDA escalar tem rotas inválidas".into(),
            ));
        }
        let mut leases = Vec::with_capacity(experts.len());
        for &expert in experts {
            let key = ExpertKey::new(&self.model, self.layer, expert, self.format.name());
            let lease = match cuda.resident_lookup(&key) {
                Some(lease) => lease,
                None => {
                    let payload = source
                        .load_expert(self.layer, expert)
                        .map_err(|error| MoeBlockError::Executor(error.to_string()))?;
                    cuda.resident_admit(key, &payload)
                        .map_err(|error| MoeBlockError::Executor(error.to_string()))?
                }
            };
            leases.push(lease);
        }
        cuda.run_route_batch_resident(&leases, token_ids, weights, token_offsets, hidden, tokens)
            .map_err(|error| MoeBlockError::Executor(error.to_string()))
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

    #[cfg(feature = "cuda")]
    pub fn new_with_device_cache(
        cache: Arc<ExpertCache>,
        device_cache: Arc<super::cache::DeviceExpertCache>,
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
            executor: IqQuantExecutor::new_with_device_cache(
                cache,
                device_cache,
                format,
                hidden,
                intermediate,
            )?,
        })
    }

    #[cfg(feature = "cuda")]
    pub fn uses_cuda(&self) -> bool {
        self.executor.uses_cuda()
    }

    #[cfg(feature = "cuda")]
    pub fn cuda_diagnostics(&self) -> super::cuda::Nvfp4CudaDiagnostics {
        self.executor.cuda_diagnostics()
    }

    #[cfg(feature = "cuda")]
    pub fn resident_cache_stats(&self) -> super::cache::DeviceCacheStats {
        self.executor.resident_cache_stats()
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

    pub fn forward_from_source_with_route(
        &self,
        source: &IqQuantSource,
        hidden: &[f32],
        ids: &[u32],
        weights: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let model = &self.model;
        let layer = self.layer;
        let format = self.format.name();
        dispatch_resolved(ids, weights, |expert| {
            let key = ExpertKey::new(model, layer, expert, format);
            self.executor.run_from_source(source, key, layer, hidden)
        })
    }

    #[cfg(feature = "cuda")]
    pub fn forward_batch_from_source_with_route(
        &self,
        source: &IqQuantSource,
        hidden: &[f32],
        experts: &[u32],
        token_ids: &[u32],
        weights: &[f32],
        token_offsets: &[usize],
        tokens: usize,
    ) -> Result<Vec<f32>, MoeBlockError> {
        if !self.uses_cuda() {
            return Err(MoeBlockError::Executor(
                "lote CUDA IQ-quant solicitado sem ponte CUDA ativa".into(),
            ));
        }
        self.executor
            .run_route_batch_from_source(
                source,
                &self.model,
                self.layer,
                experts,
                token_ids,
                weights,
                token_offsets,
                hidden,
                tokens,
            )
            .map_err(MoeBlockError::from)
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

    #[cfg(feature = "cuda")]
    pub fn new_with_device_cache(
        cache: Arc<ExpertCache>,
        device_cache: Arc<super::cache::DeviceExpertCache>,
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
            executor: KQuantExecutor::new_with_device_cache(
                cache,
                device_cache,
                format,
                hidden,
                intermediate,
            )?,
        })
    }

    #[cfg(feature = "cuda")]
    pub fn uses_cuda(&self) -> bool {
        self.executor.uses_cuda()
    }

    #[cfg(feature = "cuda")]
    pub fn cuda_diagnostics(&self) -> super::cuda::Nvfp4CudaDiagnostics {
        self.executor.cuda_diagnostics()
    }

    #[cfg(feature = "cuda")]
    pub fn resident_cache_stats(&self) -> super::cache::DeviceCacheStats {
        self.executor.resident_cache_stats()
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

    pub fn forward_from_source_with_route(
        &self,
        source: &KQuantSource,
        hidden: &[f32],
        ids: &[u32],
        weights: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        #[cfg(feature = "cuda")]
        if self.uses_cuda() {
            return self
                .executor
                .run_route_batch_from_source(
                    source,
                    &self.model,
                    self.layer,
                    ids,
                    &vec![0; ids.len()],
                    weights,
                    &[0, ids.len()],
                    hidden,
                    1,
                )
                .map_err(MoeBlockError::from);
        }
        let model = &self.model;
        let layer = self.layer;
        let format = self.format.name();
        dispatch_resolved(ids, weights, |expert| {
            let key = ExpertKey::new(model, layer, expert, format);
            self.executor.run_from_source(source, key, layer, hidden)
        })
    }

    #[cfg(feature = "cuda")]
    pub fn forward_batch_from_source_with_route(
        &self,
        source: &KQuantSource,
        hidden: &[f32],
        experts: &[u32],
        token_ids: &[u32],
        weights: &[f32],
        token_offsets: &[usize],
        tokens: usize,
    ) -> Result<Vec<f32>, MoeBlockError> {
        if !self.uses_cuda() {
            return Err(MoeBlockError::Executor(
                "lote CUDA K-quant solicitado sem ponte CUDA ativa".into(),
            ));
        }
        self.executor
            .run_route_batch_from_source(
                source,
                &self.model,
                self.layer,
                experts,
                token_ids,
                weights,
                token_offsets,
                hidden,
                tokens,
            )
            .map_err(MoeBlockError::from)
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

    pub fn forward_from_source_with_route(
        &self,
        source: &Q4_0Source,
        hidden: &[f32],
        ids: &[u32],
        weights: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let model = &self.model;
        let layer = self.layer;
        dispatch_resolved(ids, weights, |expert| {
            let key = ExpertKey::new(model, layer, expert, "q4_0");
            self.executor.run_from_source(source, key, layer, hidden)
        })
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

    #[cfg(feature = "cuda")]
    pub fn new_cpu(
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
            executor: Nvfp4Executor::new_cpu(cache, hidden, intermediate)
                .map_err(|error| error.to_string())?,
        })
    }

    #[cfg(feature = "cuda")]
    pub fn new_with_device_cache(
        cache: Arc<ExpertCache>,
        device_cache: Arc<super::cache::DeviceExpertCache>,
        model: impl Into<String>,
        layer: u32,
        top_k: usize,
        hidden: usize,
        intermediate: usize,
        coexecution_profile: Option<super::bandwidth::BandwidthProfile>,
    ) -> Result<Self, String> {
        Ok(Self {
            model: model.into(),
            layer,
            top_k,
            executor: Nvfp4Executor::new_with_device_cache(
                cache,
                device_cache,
                hidden,
                intermediate,
                coexecution_profile,
            )
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
        self.forward_from_source_with_logits(source, hidden, &logits)
    }

    pub fn forward_from_source_with_logits(
        &self,
        source: &Nvfp4Source,
        hidden: &[f32],
        logits: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        let route = select_top_k(logits, self.top_k).map_err(MoeBlockError::Route)?;
        self.forward_from_source_with_route(source, hidden, &route.ids, &route.weights)
    }

    pub fn executor_kind(&self) -> Nvfp4BackendKind {
        self.executor.backend_kind()
    }

    #[cfg(feature = "cuda")]
    pub fn cuda_diagnostics(&self) -> super::cuda::Nvfp4CudaDiagnostics {
        self.executor.cuda_diagnostics()
    }

    #[cfg(feature = "cuda")]
    pub fn host_diagnostics(&self) -> super::format_nvfp4::Nvfp4HostDiagnostics {
        self.executor.host_diagnostics()
    }

    #[cfg(feature = "cuda")]
    pub fn resident_cache_stats(&self) -> super::cache::DeviceCacheStats {
        self.executor.resident_cache_stats()
    }

    pub fn forward_from_source_with_route(
        &self,
        source: &Nvfp4Source,
        hidden: &[f32],
        ids: &[u32],
        weights: &[f32],
    ) -> Result<Vec<f32>, MoeBlockError> {
        if ids.is_empty() {
            return Err(MoeBlockError::Route(RouteError::InvalidWeightSum));
        }
        if ids.len() != weights.len() {
            return Err(MoeBlockError::Route(RouteError::MismatchedRouteLengths {
                ids: ids.len(),
                weights: weights.len(),
            }));
        }
        if weights.iter().any(|weight| !weight.is_finite()) {
            return Err(MoeBlockError::Route(RouteError::InvalidWeightSum));
        }
        #[cfg(feature = "cuda")]
        if matches!(self.executor.backend_kind(), Nvfp4BackendKind::Cuda) {
            let token_ids = vec![0; ids.len()];
            return self
                .executor
                .run_route_batch_from_source(
                    source,
                    &self.model,
                    self.layer,
                    ids,
                    &token_ids,
                    weights,
                    &[0, ids.len()],
                    hidden,
                    1,
                )
                .map_err(MoeBlockError::from);
        }
        let model = &self.model;
        let layer = self.layer;
        dispatch_resolved(ids, weights, |expert| {
            let key = ExpertKey::new(model, layer, expert, "nvfp4");
            self.executor.run_from_source(source, key, layer, hidden)
        })
    }

    #[cfg(feature = "cuda")]
    pub fn forward_batch_from_source_with_route(
        &self,
        source: &Nvfp4Source,
        hidden: &[f32],
        experts: &[u32],
        token_ids: &[u32],
        weights: &[f32],
        token_offsets: &[usize],
        tokens: usize,
    ) -> Result<Vec<f32>, MoeBlockError> {
        if experts.is_empty() {
            return Err(MoeBlockError::Route(RouteError::InvalidWeightSum));
        }
        if experts.len() != token_ids.len() || experts.len() != weights.len() {
            return Err(MoeBlockError::Route(RouteError::MismatchedRouteLengths {
                ids: experts.len(),
                weights: weights.len(),
            }));
        }
        if weights.iter().any(|weight| !weight.is_finite()) {
            return Err(MoeBlockError::Route(RouteError::InvalidWeightSum));
        }
        self.executor
            .run_route_batch_from_source(
                source,
                &self.model,
                self.layer,
                experts,
                token_ids,
                weights,
                token_offsets,
                hidden,
                tokens,
            )
            .map_err(MoeBlockError::from)
    }
}

/// Bloco MoE cujo decoder é escolhido por tensor, para GGUF com dtypes
/// heterogêneos entre gate, up e down.
pub struct HeterogeneousMoeBlock {
    model: String,
    layer: u32,
    top_k: usize,
    cache: Arc<ExpertCache>,
    #[cfg(feature = "cuda")]
    cuda: Option<super::cuda::HeterogeneousCudaExecutor>,
}

impl HeterogeneousMoeBlock {
    pub fn new(
        cache: Arc<ExpertCache>,
        model: impl Into<String>,
        layer: u32,
        top_k: usize,
    ) -> Self {
        Self {
            model: model.into(),
            layer,
            top_k,
            cache,
            #[cfg(feature = "cuda")]
            cuda: None,
        }
    }

    #[cfg(feature = "cuda")]
    pub fn new_with_device_cache(
        cache: Arc<ExpertCache>,
        device_cache: Arc<super::cache::DeviceExpertCache>,
        model: impl Into<String>,
        layer: u32,
        top_k: usize,
        hidden: usize,
        intermediate: usize,
    ) -> Self {
        Self {
            model: model.into(),
            layer,
            top_k,
            cache,
            cuda: Some(super::cuda::HeterogeneousCudaExecutor::new_with_cache(
                hidden,
                intermediate,
                device_cache,
            )),
        }
    }

    #[cfg(feature = "cuda")]
    pub fn uses_cuda(&self) -> bool {
        self.cuda.is_some()
    }

    #[cfg(feature = "cuda")]
    pub fn cuda_diagnostics(&self) -> super::cuda::Nvfp4CudaDiagnostics {
        self.cuda.as_ref().map_or_else(
            Default::default,
            super::cuda::HeterogeneousCudaExecutor::diagnostics,
        )
    }

    #[cfg(feature = "cuda")]
    pub fn resident_cache_stats(&self) -> super::cache::DeviceCacheStats {
        self.cuda.as_ref().map_or_else(
            Default::default,
            super::cuda::HeterogeneousCudaExecutor::resident_cache_stats,
        )
    }

    pub fn forward_from_source_with_route(
        &self,
        source: &HeterogeneousMoeSource,
        hidden: &[f32],
        ids: &[u32],
        weights: &[f32],
        activation: i32,
    ) -> Result<Vec<f32>, MoeBlockError> {
        if ids.len() != weights.len() {
            return Err(MoeBlockError::Route(RouteError::MismatchedRouteLengths {
                ids: ids.len(),
                weights: weights.len(),
            }));
        }
        if ids.is_empty() || ids.len() > self.top_k {
            return Err(MoeBlockError::Route(RouteError::InvalidTopK {
                top_k: ids.len(),
                experts: self.top_k,
            }));
        }
        source
            .run_from_source_with_route(
                &self.cache,
                &self.model,
                self.layer,
                hidden,
                ids,
                weights,
                activation,
            )
            .map_err(MoeBlockError::Executor)
    }

    #[cfg(feature = "cuda")]
    pub fn forward_batch_from_source_with_route(
        &self,
        source: &HeterogeneousMoeSource,
        experts: &[u32],
        token_ids: &[u32],
        weights: &[f32],
        token_offsets: &[usize],
        hidden: &[f32],
        tokens: usize,
        activation: i32,
    ) -> Result<Vec<f32>, MoeBlockError> {
        if experts.is_empty() || experts.len() != token_ids.len() || experts.len() != weights.len()
        {
            return Err(MoeBlockError::Route(RouteError::MismatchedRouteLengths {
                ids: experts.len(),
                weights: weights.len(),
            }));
        }
        if experts.len() > self.top_k.saturating_mul(tokens.max(1)) {
            return Err(MoeBlockError::Route(RouteError::InvalidTopK {
                top_k: experts.len(),
                experts: self.top_k,
            }));
        }
        let Some(cuda) = self.cuda.as_ref() else {
            return Err(MoeBlockError::Executor(
                "executor CUDA heterogêneo não foi selecionado".into(),
            ));
        };

        let mut descriptors = Vec::with_capacity(experts.len());
        let mut leases = Vec::new();
        for &expert in experts {
            let payloads = source
                .load_expert_tensors(self.layer, expert)
                .map_err(|error| MoeBlockError::Executor(error.to_string()))?;
            let mut descriptor = CudaHeterogeneousExpert::default();
            for payload in payloads {
                let HeterogeneousTensorPayload { role, dtype, bytes } = payload;
                let key = ExpertKey::new(
                    &self.model,
                    self.layer,
                    expert,
                    format!("heterogeneous:{}:{dtype}", role.as_str()),
                );
                let lease = match cuda.resident_lookup(&key) {
                    Some(lease) => lease,
                    None => cuda
                        .resident_admit(key, &bytes)
                        .map_err(|error| MoeBlockError::Executor(error.to_string()))?,
                };
                let tensor = CudaHeterogeneousTensor {
                    data: lease.handle() as *const u8,
                    bytes: bytes.len(),
                    dtype,
                };
                match role {
                    TensorRole::GateUp if descriptor.gate_up.data.is_null() => {
                        descriptor.gate_up = tensor
                    }
                    TensorRole::Gate if descriptor.gate.data.is_null() => descriptor.gate = tensor,
                    TensorRole::Up if descriptor.up.data.is_null() => descriptor.up = tensor,
                    TensorRole::Down if descriptor.down.data.is_null() => descriptor.down = tensor,
                    role => {
                        return Err(MoeBlockError::Executor(format!(
                            "papel de tensor heterogêneo duplicado: {}",
                            role.as_str()
                        )))
                    }
                }
                leases.push(lease);
            }
            descriptors.push(descriptor);
        }
        let output = cuda
            .run_route_batch_resident(
                &descriptors,
                token_ids,
                weights,
                token_offsets,
                hidden,
                tokens,
                activation,
            )
            .map_err(|error| MoeBlockError::Executor(error.to_string()))?;
        drop(leases);
        Ok(output)
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
