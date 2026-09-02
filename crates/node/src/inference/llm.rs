//! Low-level llama.cpp inference engine.
//!
//! Wraps `llama-cpp-sys-2` raw FFI to give the inference server a safe,
//! typed surface. All GPU/CPU calls are here; the rest of the codebase is pure
//! Rust.
//!
//! # Residual capture
//!
//! A non-last peer must hand the *residual stream* (the un-normalised `h` that
//! enters the next block's attention) to its successor. `llama_get_embeddings_ith`
//! returns the post-output_norm value instead, so we install a
//! `ggml_backend_sched_eval_callback` that fires when the last block's output
//! tensor (`l_out-<N>`, or an equivalent last-layer name used by the graph) is
//! computed
//! and copies it before the output head sees it. The graph keeps running after
//! the copy, so a required output head may cost compute without changing the
//! residual already exported to the next peer. If `n_embd_out` differs from
//! `n_embd`, generic llama.cpp embedding extraction stays disabled: that path
//! sizes its backend read from the metadata width, which may exceed the real
//! output tensor. The callback remains the only embedding-like output used by
//! the POM in that case.
//!
//! The callback identifies the token axis from the known token count of the
//! current physical microbatch and the tensor shape. It never uses
//! `n_embd_inp` to find that axis: dimensions before it are features and are
//! preserved in their physical order. Trailing singleton dimensions are
//! ignored, while trailing dimensions greater than one are rejected.
//!
//! The callback is an `extern "C"` fn with no closure environment, so the
//! capture state travels in llama.cpp's own `cb_eval_user_data`: each context
//! owns a `CaptureSlot` and the callback is handed its address. It used to be
//! two process-global `Mutex`es, safe only while a single decode ran at a time;
//! with two contexts decoding in parallel that became a race in which one
//! request would carry the other's residual - no error, just a wrong answer.

use crate::config::KvCacheType;
use crate::wire::TensorShape;
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, CStr, CString};
use std::ops::Range;
use std::ptr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::Instant;
use tracing::warn;

use crate::device_placement;
use llama_cpp_sys_2 as sys;

// ── Capture state, one per context ────────────────────────────────────────────

/// Estado da captura de um contexto.
///
/// Já foi global de processo, e o comentário do topo deste arquivo dizia que
/// isso era seguro porque um decode rodava por vez. Deixou de ser verdade no
/// momento em que dois contextos passaram a decodificar em paralelo: as duas
/// requisições escreveriam no mesmo buffer e uma levaria o residual da outra -
/// sem erro nenhum, só resposta errada. Agora cada contexto tem o seu, e o
/// endereço viaja no `cb_eval_user_data` do próprio llama.cpp.
struct CaptureSlot {
    want: Option<CString>,
    buf: Option<Vec<f32>>,
    expected_tokens: usize,
    max_microbatch_tokens: usize,
    n_embd: usize,
    expected_floats: Option<usize>,
    layout: Option<ResidualCaptureLayout>,
    error: Option<String>,
    last_shape: Option<[i64; 4]>,
    last_strides: Option<[usize; 4]>,
    last_nbytes: Option<usize>,
    copy_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResidualCaptureLayout {
    feature_shape: Vec<usize>,
    token_axis: usize,
}

pub(crate) fn validate_hidden_tensor(
    values_len: usize,
    shape: &TensorShape,
    n_tokens: usize,
    n_embd: usize,
    n_embd_inp: usize,
    n_embd_out: usize,
    item: &str,
) -> Result<usize> {
    if shape.len() < 2 {
        bail!(
            "{item}: hidden state shape {:?} não suportado, esperado [tokens, n_embd, ...]",
            shape
        );
    }
    if shape[0] != n_tokens {
        bail!(
            "{item}: hidden state declara {} tokens no shape {:?}, esperado {}",
            shape[0],
            shape,
            n_tokens
        );
    }
    if shape[1] != n_embd {
        bail!(
            "{item}: dimensão base {} no hidden state {:?} diverge de n_embd {}",
            shape[1],
            shape,
            n_embd
        );
    }
    let feature_width = shape[1..]
        .iter()
        .try_fold(1usize, |width, &dimension| width.checked_mul(dimension))
        .ok_or_else(|| anyhow::anyhow!("{item}: largura do hidden state excede o limite"))?;
    // A forma 2D continua usando a entrada base. Quando o emissor preserva
    // eixos de fluxo, llama.cpp reporta em n_embd_out a largura física que
    // aparece no residual. A contagem de fluxos da metadata é estrutural e
    // não deve ser multiplicada aqui em paralelo com essa fonte autoritativa.
    let has_non_singleton_extra_axis = shape[2..].iter().any(|&dimension| dimension > 1);
    let expected_width = if !has_non_singleton_extra_axis {
        n_embd_inp
    } else {
        n_embd_out
    };
    if feature_width != expected_width {
        bail!(
            "{item}: hidden state shape {:?} tem largura de entrada {}, esperado {} a partir da metadata",
            shape,
            feature_width,
            expected_width
        );
    }
    let expected_values = shape
        .iter()
        .try_fold(1usize, |values, &dimension| values.checked_mul(dimension))
        .ok_or_else(|| anyhow::anyhow!("{item}: tamanho do hidden state excede o limite"))?;
    if values_len != expected_values {
        bail!(
            "{item}: hidden state tem {} floats, esperado {} para shape {:?}",
            values_len,
            expected_values,
            shape
        );
    }
    Ok(feature_width)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CapturedResidual {
    pub(crate) values: Vec<f32>,
    pub(crate) shape: TensorShape,
}

#[derive(Default)]
struct MoeCallbackTelemetry {
    forward_us: u64,
    executors: Vec<crate::moe::runtime::MoeExecutorKind>,
}

const MOE_ROUTE_DIAGNOSTICS_ENV: &str = "POM_MOE_ROUTE_DIAGNOSTICS";

#[derive(Debug, PartialEq)]
struct MoeRouteDiagnosticSample {
    slot: usize,
    source_offset_elements: usize,
    expert: i32,
    weight: f32,
}

#[derive(Debug, PartialEq)]
struct MoeRouteDiagnostic {
    llama_declared_route_elements: usize,
    pom_expected_route_elements: usize,
    selected_read_elements: usize,
    weights_read_elements: usize,
    raw_nan_count: usize,
    raw_non_finite_count: usize,
    token_zero_samples: Vec<MoeRouteDiagnosticSample>,
}

fn collect_moe_route_diagnostic(
    selected_experts: &[i32],
    weights: &[f32],
    n_tokens: usize,
    expert_count_used: usize,
) -> MoeRouteDiagnostic {
    let declared_route_elements = n_tokens
        .checked_mul(expert_count_used)
        .unwrap_or(usize::MAX);
    let token_zero_samples = if n_tokens == 0 {
        Vec::new()
    } else {
        (0..expert_count_used.min(4))
            .filter_map(|slot| {
                let source_offset_elements = slot;
                Some(MoeRouteDiagnosticSample {
                    slot,
                    source_offset_elements,
                    expert: *selected_experts.get(source_offset_elements)?,
                    weight: *weights.get(source_offset_elements)?,
                })
            })
            .collect()
    };

    MoeRouteDiagnostic {
        llama_declared_route_elements: declared_route_elements,
        pom_expected_route_elements: declared_route_elements,
        selected_read_elements: selected_experts.len(),
        weights_read_elements: weights.len(),
        raw_nan_count: weights.iter().filter(|weight| weight.is_nan()).count(),
        raw_non_finite_count: weights.iter().filter(|weight| !weight.is_finite()).count(),
        token_zero_samples,
    }
}

fn moe_route_diagnostics_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var(MOE_ROUTE_DIAGNOSTICS_ENV).ok().as_deref(),
            Some("1" | "true" | "yes" | "on")
        )
    })
}

fn log_moe_route_diagnostics_configuration() {
    static LOGGED: OnceLock<()> = OnceLock::new();
    if LOGGED.set(()).is_err() {
        return;
    }
    let raw = std::env::var(MOE_ROUTE_DIAGNOSTICS_ENV).unwrap_or_else(|_| "<unset>".to_owned());
    let enabled = moe_route_diagnostics_enabled();
    warn!("POM_MOE_ROUTE_DIAGNOSTICS configuration raw={raw:?} enabled={enabled}");
}

fn route_diagnostic_should_emit(first_callback: bool, raw_non_finite: bool) -> bool {
    first_callback || raw_non_finite
}

fn first_non_finite(values: &[f32]) -> Option<(usize, f32)> {
    values
        .iter()
        .copied()
        .enumerate()
        .find(|(_, value)| !value.is_finite())
}

struct MoeCallbackState {
    runtime: Arc<crate::moe::runtime::MoeRuntime>,
    telemetry: Mutex<MoeCallbackTelemetry>,
    error: Mutex<Option<String>>,
    route_diagnostic_emitted: AtomicBool,
}

impl MoeCallbackState {
    fn begin(&self) {
        *self
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        *self
            .telemetry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = MoeCallbackTelemetry::default();
        self.route_diagnostic_emitted
            .store(false, Ordering::Relaxed);
    }

    fn claim_route_diagnostic(&self) -> bool {
        moe_route_diagnostics_enabled()
            && !self.route_diagnostic_emitted.swap(true, Ordering::Relaxed)
    }

    fn fail(&self, message: String) {
        let mut error = self
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if error.is_none() {
            *error = Some(message);
        }
    }

    fn take(&self) -> Result<MoeCallbackTelemetry> {
        let error = self
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(error) = error {
            bail!("hook MoE do llama.cpp falhou: {error}");
        }
        Ok(std::mem::take(
            &mut *self
                .telemetry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ))
    }
}

fn log_moe_callback_entry(
    user_data: *mut c_void,
    descriptor: *const sys::llama_moe_ffn_descriptor,
    hidden: *const f32,
    selected_experts: *const i32,
    weights: *const f32,
    n_tokens: usize,
    hidden_width: usize,
    output: *mut f32,
) {
    tracing::warn!(
        "POM_MOE_ROUTE_DIAGNOSTICS callback_entry_default raw_args=1 descriptor_ptr={:p} user_data_ptr={:p} hidden_ptr={:p} selected_ptr={:p} weights_ptr={:p} output_ptr={:p} n_tokens={n_tokens} hidden_width={hidden_width}",
        descriptor,
        user_data,
        hidden,
        selected_experts,
        weights,
        output,
    );
    tracing::warn!(
        target: "moe.route_diagnostic",
        "POM_MOE_ROUTE_DIAGNOSTICS callback_entry raw_args=1 descriptor_ptr={:p} user_data_ptr={:p} hidden_ptr={:p} selected_ptr={:p} weights_ptr={:p} output_ptr={:p} n_tokens={n_tokens} hidden_width={hidden_width}",
        descriptor,
        user_data,
        hidden,
        selected_experts,
        weights,
        output,
    );
}

#[derive(Debug, PartialEq)]
struct MoeCallbackExitDiagnostic {
    reason: &'static str,
    raw_pointers: [usize; 6],
    n_tokens: usize,
    hidden_width: usize,
}

fn build_moe_callback_exit_diagnostic(
    reason: &'static str,
    user_data: *mut c_void,
    descriptor: *const sys::llama_moe_ffn_descriptor,
    hidden: *const f32,
    selected_experts: *const i32,
    weights: *const f32,
    n_tokens: usize,
    hidden_width: usize,
    output: *mut f32,
) -> MoeCallbackExitDiagnostic {
    MoeCallbackExitDiagnostic {
        reason,
        raw_pointers: [
            user_data as usize,
            descriptor as usize,
            hidden as usize,
            selected_experts as usize,
            weights as usize,
            output as usize,
        ],
        n_tokens,
        hidden_width,
    }
}

fn log_moe_callback_exit(diagnostic: &MoeCallbackExitDiagnostic) {
    let [user_data, descriptor, hidden, selected_experts, weights, output] =
        diagnostic.raw_pointers;
    tracing::warn!(
        target: "moe.route_diagnostic",
        "POM_MOE_ROUTE_DIAGNOSTICS callback_exit reason={} user_data_ptr=0x{user_data:x} descriptor_ptr=0x{descriptor:x} hidden_ptr=0x{hidden:x} selected_ptr=0x{selected_experts:x} weights_ptr=0x{weights:x} output_ptr=0x{output:x} n_tokens={} hidden_width={} null_args=user_data:{} descriptor:{} hidden:{} selected:{} weights:{} output:{}",
        diagnostic.reason,
        diagnostic.n_tokens,
        diagnostic.hidden_width,
        user_data == 0,
        descriptor == 0,
        hidden == 0,
        selected_experts == 0,
        weights == 0,
        output == 0,
    );
}

unsafe extern "C" fn moe_ffn_callback(
    user_data: *mut c_void,
    descriptor: *const sys::llama_moe_ffn_descriptor,
    hidden: *const f32,
    selected_experts: *const i32,
    weights: *const f32,
    n_tokens: usize,
    hidden_width: usize,
    output: *mut f32,
) -> bool {
    let diagnostics_enabled = moe_route_diagnostics_enabled();
    if diagnostics_enabled {
        log_moe_callback_entry(
            user_data,
            descriptor,
            hidden,
            selected_experts,
            weights,
            n_tokens,
            hidden_width,
            output,
        );
    }

    macro_rules! diagnostic_return {
        ($reason:literal) => {{
            if diagnostics_enabled {
                let diagnostic = build_moe_callback_exit_diagnostic(
                    $reason,
                    user_data,
                    descriptor,
                    hidden,
                    selected_experts,
                    weights,
                    n_tokens,
                    hidden_width,
                    output,
                );
                log_moe_callback_exit(&diagnostic);
            }
            return false;
        }};
    }

    if user_data.is_null()
        || descriptor.is_null()
        || hidden.is_null()
        || selected_experts.is_null()
        || weights.is_null()
        || output.is_null()
    {
        diagnostic_return!("null_callback_argument");
    }
    let state = unsafe { &*(user_data as *const MoeCallbackState) };
    let descriptor = unsafe { &*descriptor };
    let used = match usize::try_from(descriptor.expert_count_used) {
        Ok(used) if used > 0 => used,
        _ => {
            state.fail("descriptor MoE sem quantidade válida de experts ativos".to_string());
            diagnostic_return!("invalid_expert_count_used");
        }
    };
    let expected = match n_tokens.checked_mul(hidden_width) {
        Some(expected) => expected,
        None => {
            state.fail("overflow na geometria hidden do hook".to_string());
            diagnostic_return!("hidden_geometry_overflow");
        }
    };
    let route_len = match n_tokens.checked_mul(used) {
        Some(length) => length,
        None => {
            state.fail("overflow na geometria da rota do hook".to_string());
            diagnostic_return!("route_geometry_overflow");
        }
    };
    let hidden = unsafe { std::slice::from_raw_parts(hidden, expected) };
    let selected_experts = unsafe { std::slice::from_raw_parts(selected_experts, route_len) };
    let weights = unsafe { std::slice::from_raw_parts(weights, route_len) };
    let architecture = unsafe {
        CStr::from_ptr(descriptor.architecture.as_ptr())
            .to_string_lossy()
            .into_owned()
    };
    if let Some((index, value)) = first_non_finite(hidden) {
        let kind = if value.is_nan() { "NaN" } else { "infinito" };
        state.fail(format!(
            "entrada hidden do hook MoE da arquitetura {architecture}, camada {}, índice {index} tem valor não finito ({kind}); o forward MoE anterior contaminou esta camada",
            descriptor.layer
        ));
        diagnostic_return!("hidden_input_non_finite");
    }
    let raw_non_finite = weights.iter().any(|weight| !weight.is_finite());
    let emit_route_diagnostic = state.claim_route_diagnostic()
        || (diagnostics_enabled && route_diagnostic_should_emit(false, raw_non_finite));
    if emit_route_diagnostic {
        log_moe_route_input_diagnostic(
            &architecture,
            descriptor,
            hidden,
            selected_experts,
            weights,
            n_tokens,
            hidden_width,
            expected,
            route_len,
            used,
            output,
        );
    }
    let (selected_experts, weights) = match normalize_moe_route(
        &architecture,
        descriptor.layer,
        selected_experts,
        weights,
        n_tokens,
        used,
    ) {
        Ok(route) => route,
        Err(error) => {
            state.fail(error.to_string());
            diagnostic_return!("route_normalization_failed");
        }
    };
    if emit_route_diagnostic {
        log_moe_route_post_diagnostic(descriptor.layer, &selected_experts, &weights, used);
    }
    if descriptor.gate_up_bias.present != 0
        || descriptor.gate_bias.present != 0
        || descriptor.up_bias.present != 0
        || descriptor.down_bias.present != 0
    {
        state.fail(format!(
            "arquitetura {architecture} usa bias de experts não suportado pelo hook"
        ));
        diagnostic_return!("unsupported_expert_bias");
    }
    if descriptor.down.present == 0
        || (descriptor.gate_up.present == 0
            && (descriptor.gate.present == 0 || descriptor.up.present == 0))
    {
        state.fail(format!(
            "arquitetura {architecture} não forneceu metadata completa de gate/up/down"
        ));
        diagnostic_return!("incomplete_expert_metadata");
    }
    let mut metadata_owned = Vec::new();
    for (role, tensor) in [
        (
            crate::moe::format_heterogeneous::TensorRole::GateUp,
            &descriptor.gate_up,
        ),
        (
            crate::moe::format_heterogeneous::TensorRole::Gate,
            &descriptor.gate,
        ),
        (
            crate::moe::format_heterogeneous::TensorRole::Up,
            &descriptor.up,
        ),
        (
            crate::moe::format_heterogeneous::TensorRole::Down,
            &descriptor.down,
        ),
    ] {
        if tensor.present == 0 {
            continue;
        }
        let end = tensor
            .name
            .iter()
            .position(|&character| character == 0)
            .unwrap_or(tensor.name.len());
        let name = tensor.name[..end]
            .iter()
            .map(|&character| u8::try_from(character).map_err(|_| "nome de tensor inválido"))
            .collect::<Result<Vec<_>, _>>();
        let name = match name
            .and_then(|bytes| String::from_utf8(bytes).map_err(|_| "nome de tensor não UTF-8"))
        {
            Ok(name) => name,
            Err(error) => {
                state.fail(format!("arquitetura {architecture}: {error}"));
                diagnostic_return!("invalid_tensor_name");
            }
        };
        let dtype = match u32::try_from(tensor.dtype) {
            Ok(dtype) => dtype,
            Err(_) => {
                state.fail(format!(
                    "arquitetura {architecture}: tensor {name} tem dtype negativo {}",
                    tensor.dtype
                ));
                diagnostic_return!("negative_tensor_dtype");
            }
        };
        metadata_owned.push((role, name, dtype, tensor.n_dims, tensor.ne));
    }
    let metadata = metadata_owned
        .iter()
        .map(|(role, name, dtype, n_dims, dims)| {
            (*role, name.as_str(), *dtype, *n_dims, *dims)
        })
        .collect::<Vec<_>>();
    if let Err(error) = state
        .runtime
        .validate_callback_metadata(descriptor.layer as u32, &metadata)
    {
        state.fail(format!("arquitetura {architecture}: {error}"));
        diagnostic_return!("callback_metadata_validation_failed");
    }
    let expert_count = match usize::try_from(descriptor.expert_count) {
        Ok(count) => count,
        Err(_) => {
            state.fail(format!(
                "arquitetura {architecture}: quantidade de experts excede usize"
            ));
            diagnostic_return!("expert_count_overflow");
        }
    };
    let started = Instant::now();
    let result = state
        .runtime
        .forward_batch_with_route(crate::moe::runtime::MoeRouteBatch {
            layer: descriptor.layer as u32,
            hidden,
            selected_experts: &selected_experts,
            weights: &weights,
            tokens: n_tokens,
            expert_count,
            expert_count_used: used,
            activation: descriptor.activation,
            weight_before_ffn: descriptor.weight_before_ffn != 0,
        });
    let values = match result {
        Ok(values) if values.len() == expected => values,
        Ok(values) => {
            state.fail(format!(
                "hook MoE devolveu {} floats, esperado {expected}",
                values.len()
            ));
            diagnostic_return!("forward_output_geometry_mismatch");
        }
        Err(error) => {
            state.fail(format!("arquitetura {architecture}: {error}"));
            diagnostic_return!("forward_failed");
        }
    };
    if let Some((index, value)) = first_non_finite(&values) {
        let kind = if value.is_nan() { "NaN" } else { "infinito" };
        state.fail(format!(
            "executor MoE da arquitetura {architecture}, camada {}, devolveu saída não finita no índice {index} ({kind}); o resultado não será enviado ao llama.cpp",
            descriptor.layer
        ));
        diagnostic_return!("forward_output_non_finite");
    }
    unsafe { std::ptr::copy_nonoverlapping(values.as_ptr(), output, expected) };
    let mut telemetry = state
        .telemetry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    telemetry.forward_us = telemetry
        .forward_us
        .saturating_add(started.elapsed().as_micros() as u64);
    let executor = state.runtime.executor_kind();
    if !telemetry.executors.contains(&executor) {
        telemetry.executors.push(executor);
    }
    true
}

fn diagnostic_tensor_name(tensor: &sys::llama_moe_tensor_descriptor) -> String {
    let end = tensor
        .name
        .iter()
        .position(|&character| character == 0)
        .unwrap_or(tensor.name.len());
    let bytes = tensor.name[..end]
        .iter()
        .map(|&character| character as u8)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn log_moe_route_input_diagnostic(
    architecture: &str,
    descriptor: &sys::llama_moe_ffn_descriptor,
    hidden: &[f32],
    selected_experts: &[i32],
    weights: &[f32],
    n_tokens: usize,
    hidden_width: usize,
    hidden_elements: usize,
    route_elements: usize,
    expert_count_used: usize,
    output: *mut f32,
) {
    let diagnostic =
        collect_moe_route_diagnostic(selected_experts, weights, n_tokens, expert_count_used);
    let selected_bytes = route_elements.saturating_mul(std::mem::size_of::<i32>());
    let weights_bytes = route_elements.saturating_mul(std::mem::size_of::<f32>());
    let hidden_bytes = hidden_elements.saturating_mul(std::mem::size_of::<f32>());
    let output_bytes = hidden_bytes;

    tracing::warn!(
        target: "moe.route_diagnostic",
        "POM_MOE_ROUTE_DIAGNOSTICS input architecture={architecture} layer={} token=0 selected_ptr={:p} selected_bytes={selected_bytes} weights_ptr={:p} weights_bytes={weights_bytes} hidden_ptr={:p} hidden_bytes={hidden_bytes} output_ptr={:p} output_bytes={output_bytes} hidden_width={hidden_width} n_tokens={n_tokens} llama_declared_expert_count={} llama_declared_expert_count_used={} llama_declared_route_elements={} pom_expected_route_elements={} selected_read_elements={} weights_read_elements={}",
        descriptor.layer,
        selected_experts.as_ptr(),
        weights.as_ptr(),
        hidden.as_ptr(),
        output,
        descriptor.expert_count,
        descriptor.expert_count_used,
        diagnostic.llama_declared_route_elements,
        diagnostic.pom_expected_route_elements,
        diagnostic.selected_read_elements,
        diagnostic.weights_read_elements,
    );
    tracing::warn!(
        target: "moe.route_diagnostic",
        "POM_MOE_ROUTE_DIAGNOSTICS raw route_dtype_assumed=i32 selected_dtype_assumed=i32 weight_dtype_assumed=f32 hidden_dtype_assumed=f32 output_dtype_assumed=f32 raw_has_nan={} raw_nan_count={} raw_has_non_finite={} raw_non_finite_count={} raw_token_zero_slots={:?}",
        diagnostic.raw_nan_count > 0,
        diagnostic.raw_nan_count,
        diagnostic.raw_non_finite_count > 0,
        diagnostic.raw_non_finite_count,
        diagnostic.token_zero_samples,
    );

    for (role, tensor) in [
        ("gate_up", &descriptor.gate_up),
        ("gate", &descriptor.gate),
        ("up", &descriptor.up),
        ("down", &descriptor.down),
        ("gate_up_bias", &descriptor.gate_up_bias),
        ("gate_bias", &descriptor.gate_bias),
        ("up_bias", &descriptor.up_bias),
        ("down_bias", &descriptor.down_bias),
        ("gate_scale", &descriptor.gate_scale),
        ("up_scale", &descriptor.up_scale),
        ("down_scale", &descriptor.down_scale),
    ] {
        tracing::warn!(
            target: "moe.route_diagnostic",
            "POM_MOE_ROUTE_DIAGNOSTICS tensor role={role} present={} dtype={} n_dims={} ne={:?} name={}",
            tensor.present,
            tensor.dtype,
            tensor.n_dims,
            tensor.ne,
            diagnostic_tensor_name(tensor),
        );
    }
}

fn log_moe_route_post_diagnostic(
    layer: i32,
    selected_experts: &[i32],
    weights: &[f32],
    expert_count_used: usize,
) {
    let post_nan_count = weights.iter().filter(|weight| weight.is_nan()).count();
    let post_non_finite_count = weights.iter().filter(|weight| !weight.is_finite()).count();
    let token_zero_weights = weights
        .iter()
        .take(expert_count_used.min(4))
        .copied()
        .collect::<Vec<_>>();

    tracing::warn!(
        target: "moe.route_diagnostic",
        "POM_MOE_ROUTE_DIAGNOSTICS after_pom_route_normalization layer={layer} normalized_selected_ptr={:p} normalized_weights_ptr={:p} normalized_selected_elements={} normalized_weight_elements={} normalized_weight_dtype=f32 post_pom_has_nan={} post_pom_nan_count={} post_pom_has_non_finite={} post_pom_non_finite_count={} normalized_token_zero_experts={:?} normalized_token_zero_weights={:?}",
        selected_experts.as_ptr(),
        weights.as_ptr(),
        selected_experts.len(),
        weights.len(),
        post_nan_count > 0,
        post_nan_count,
        post_non_finite_count > 0,
        post_non_finite_count,
        selected_experts.iter().take(expert_count_used.min(4)).copied().collect::<Vec<_>>(),
        token_zero_weights,
    );
}

fn normalize_moe_route(
    architecture: &str,
    layer: i32,
    selected_experts: &[i32],
    weights: &[f32],
    n_tokens: usize,
    expert_count_used: usize,
) -> Result<(Vec<i32>, Vec<f32>)> {
    let expected = n_tokens
        .checked_mul(expert_count_used)
        .ok_or_else(|| anyhow::anyhow!("overflow na rota MoE"))?;
    if selected_experts.len() != expected || weights.len() != expected {
        bail!(
            "tensor(es) de rota MoE têm {} ids e {} pesos, esperado {expected}",
            selected_experts.len(),
            weights.len()
        );
    }

    let mut selected = Vec::with_capacity(expected);
    let mut normalized = Vec::with_capacity(expected);
    for source in 0..expected {
        let token = source / expert_count_used;
        let slot = source % expert_count_used;
        let expert = selected_experts[source];
        if expert < 0 {
            bail!(
                "tensor ffn_moe_selected_experts da arquitetura {architecture}, camada {layer}, token {token}, slot {slot} tem expert negativo {expert}"
            );
        }
        let weight = weights[source];
        if !weight.is_finite() {
            let kind = if weight.is_nan() { "NaN" } else { "infinito" };
            bail!(
                "tensor ffn_moe_weights da arquitetura {architecture}, camada {layer}, token {token}, slot {slot}, expert {expert} tem valor não finito ({kind})"
            );
        }
        selected.push(expert);
        normalized.push(weight);
    }
    Ok((selected, normalized))
}

#[cfg(test)]
mod moe_route_tests {
    use super::*;

    #[test]
    fn primeira_saida_nao_finita_e_identificada_antes_da_copia() {
        assert!(first_non_finite(&[0.0, 1.0, f32::NAN])
            .is_some_and(|(index, value)| index == 2 && value.is_nan()));
        assert!(first_non_finite(&[0.0, f32::INFINITY])
            .is_some_and(|(index, value)| index == 1 && value == f32::INFINITY));
        assert_eq!(first_non_finite(&[0.0, -1.0]), None);
    }

    #[test]
    fn rota_do_get_rows_do_llama_cpp_ja_chega_em_ordem_token_major() {
        // `ggml_get_rows` writes the [1, used, tokens] result with slot as
        // the fastest-changing dimension. After the C++ reshape to
        // [used, tokens], the callback therefore receives
        // [token0/slot0, token0/slot1, token1/slot0, ...].
        let selected_experts = [10, 20, 11, 21, 12, 22];
        let weights = [0.1, 0.4, 0.2, 0.5, 0.3, 0.6];

        let (selected, normalized) =
            normalize_moe_route("qwen35moe", 7, &selected_experts, &weights, 3, 2).unwrap();

        assert_eq!(selected, selected_experts);
        assert_eq!(normalized, weights);
    }

    #[test]
    fn indices_de_expert_acima_de_127_preservam_i32() {
        let selected_experts = [132, 129, 38, 253];
        let weights = [0.4, 0.3, 0.2, 0.1];

        let (selected, normalized) =
            normalize_moe_route("qwen35moe", 1, &selected_experts, &weights, 1, 4).unwrap();

        assert_eq!(selected, selected_experts);
        assert_eq!(normalized, weights);
    }

    #[test]
    fn rota_nao_finita_identifica_tensor_token_slot_e_expert() {
        let error = normalize_moe_route("qwen35moe", 4, &[13, 27], &[f32::INFINITY, 0.5], 1, 2)
            .unwrap_err()
            .to_string();

        assert!(error.contains("ffn_moe_weights"));
        assert!(error.contains("camada 4"));
        assert!(error.contains("token 0"));
        assert!(error.contains("slot 0"));
        assert!(error.contains("expert 13"));
        assert!(error.contains("infinito"));
    }

    #[test]
    fn diagnostico_da_rota_registra_entrada_bruta_e_layout_de_token_zero() {
        let selected_experts = [10, 20, 11, 21, 12, 22];
        let weights = [0.1, 0.4, 0.2, 0.5, 0.3, 0.6];

        let diagnostic = collect_moe_route_diagnostic(&selected_experts, &weights, 3, 2);

        assert_eq!(diagnostic.llama_declared_route_elements, 6);
        assert_eq!(diagnostic.pom_expected_route_elements, 6);
        assert_eq!(diagnostic.raw_nan_count, 0);
        assert_eq!(diagnostic.raw_non_finite_count, 0);
        assert_eq!(
            diagnostic.token_zero_samples,
            vec![
                MoeRouteDiagnosticSample {
                    slot: 0,
                    source_offset_elements: 0,
                    expert: 10,
                    weight: 0.1,
                },
                MoeRouteDiagnosticSample {
                    slot: 1,
                    source_offset_elements: 1,
                    expert: 20,
                    weight: 0.4,
                },
            ]
        );
    }

    #[test]
    fn diagnostico_captura_primeira_chamada_e_rota_nao_finita_em_qualquer_camada() {
        assert!(route_diagnostic_should_emit(true, false));
        assert!(route_diagnostic_should_emit(false, true));
        assert!(!route_diagnostic_should_emit(false, false));
    }

    #[test]
    fn diagnostico_de_saida_preserva_argumentos_crus_e_motivo_do_retorno() {
        let diagnostic = build_moe_callback_exit_diagnostic(
            "null_callback_argument",
            0x11usize as *mut c_void,
            0x22usize as *const sys::llama_moe_ffn_descriptor,
            0x33usize as *const f32,
            0x44usize as *const i32,
            0x55usize as *const f32,
            7,
            4096,
            0x66usize as *mut f32,
        );

        assert_eq!(diagnostic.reason, "null_callback_argument");
        assert_eq!(
            diagnostic.raw_pointers,
            [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]
        );
        assert_eq!(diagnostic.n_tokens, 7);
        assert_eq!(diagnostic.hidden_width, 4096);
    }

    #[derive(Clone)]
    struct SharedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedLogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("writer mutex poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLogWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    #[ignore = "prova do callback requer POM_MOE_ROUTE_DIAGNOSTICS=1"]
    fn callback_entry_emite_warn_padrao_e_target_moe_no_mesmo_sink() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_ansi(false)
            .with_target(false)
            .with_writer(SharedLogWriter(bytes.clone()))
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let accepted = unsafe {
                moe_ffn_callback(
                    ptr::null_mut(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    1,
                    4096,
                    ptr::null_mut(),
                )
            };
            assert!(!accepted);
        });

        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(output.contains("callback_entry_default"), "{output}");
        assert!(output.contains("callback_entry raw_args=1"), "{output}");
        assert!(
            output.contains("callback_exit reason=null_callback_argument"),
            "{output}"
        );
    }

    #[test]
    #[ignore = "prova do dispatcher global requer POM_MOE_ROUTE_DIAGNOSTICS=1"]
    fn callback_global_emite_diagnostico_em_thread_externa_e_registra_gate() {
        std::env::set_var(MOE_ROUTE_DIAGNOSTICS_ENV, "1");
        let bytes = Arc::new(Mutex::new(Vec::new()));
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_ansi(false)
            .with_target(false)
            .with_writer(SharedLogWriter(bytes.clone()))
            .init();

        log_moe_route_diagnostics_configuration();
        log_moe_route_diagnostics_configuration();
        let join = std::thread::spawn(|| unsafe {
            moe_ffn_callback(
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                1,
                4096,
                ptr::null_mut(),
            )
        });
        assert!(!join
            .join()
            .expect("thread do callback não pode entrar em pânico"));

        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert_eq!(
            output
                .matches("POM_MOE_ROUTE_DIAGNOSTICS configuration")
                .count(),
            1,
            "{output}"
        );
        assert!(
            output.contains("POM_MOE_ROUTE_DIAGNOSTICS configuration"),
            "{output}"
        );
        assert!(output.contains("enabled=true"), "{output}");
        assert!(output.contains("callback_entry_default"), "{output}");
        assert!(output.contains("callback_entry raw_args=1"), "{output}");
        assert!(
            output.contains("callback_exit reason=null_callback_argument"),
            "{output}"
        );
    }
}

unsafe extern "C" fn capture_callback(
    t: *mut sys::ggml_tensor,
    ask: bool,
    user_data: *mut c_void,
) -> bool {
    if user_data.is_null() {
        return true;
    }
    // SAFETY: `user_data` é o ponteiro que `LlmEngine::load_with_cache` registrou, e ele
    // aponta para o `Box<Mutex<CaptureSlot>>` do mesmo `LlmEngine` que é dono
    // deste contexto. O Box vive enquanto o contexto viver (ver `Drop`).
    let slot = unsafe { &*(user_data as *const Mutex<CaptureSlot>) };
    let mut slot = match slot.lock() {
        Ok(g) => g,
        Err(_) => return true,
    };
    if slot.want.is_none() {
        return true;
    }

    let name = unsafe { sys::ggml_get_name(t) };
    if name.is_null() {
        return true;
    }
    let matches = slot.want.as_deref().is_some_and(|target| {
        unsafe { CStr::from_ptr(name) }
            .to_str()
            .is_ok_and(|name| capture_target_matches(name, target))
    });

    // The two phases mean different things, and the return value means something
    // different in each — see `ggml_backend_sched_compute_splits`:
    //
    //   ask = true   "will you want this tensor?" `false` lets ggml batch this
    //                node with the following ones, which is what we want for
    //                every node but ours.
    //   ask = false  the tensor is ready. Here `false` does NOT mean "not mine":
    //                it BREAKS the evaluation loop and abandons the rest of the
    //                graph.
    //
    // Returning `ask` for a non-matching tensor got both wrong at once: it
    // claimed every node, forcing them to be computed one at a time, and then
    // returned false on the first one — so the forward pass stopped at the very
    // first node while `llama_decode` still reported success. Nothing after the
    // input embedding was ever computed.
    if ask {
        return matches;
    }
    if !matches {
        return true; // keep going; this node was simply not ours
    }

    // Compute phase: copy the tensor bytes.
    let copy_started = Instant::now();
    let tensor_shape = unsafe { (*t).ne };
    let tensor_strides = unsafe { (*t).nb };
    let n_bytes = unsafe { sys::ggml_nbytes(t) } as usize;
    slot.last_shape = Some(tensor_shape);
    slot.last_strides = Some(tensor_strides);
    slot.last_nbytes = Some(n_bytes);
    let n_floats = n_bytes / 4;
    let mut raw = vec![0u8; n_bytes];
    unsafe {
        sys::ggml_backend_tensor_get(t, raw.as_mut_ptr() as *mut c_void, 0, n_bytes);
    }
    let floats: Vec<f32> = raw
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    if floats.len() == n_floats {
        if let Err(error) = record_capture(&mut slot, tensor_shape, tensor_strides, n_bytes, floats)
        {
            if slot.error.is_none() {
                slot.error = Some(error.to_string());
            }
        }
    }
    slot.copy_us += copy_started.elapsed().as_micros() as u64;
    true
}

/// Some graph variants name the residual immediately before the output head
/// `l_last-N`; older graphs and the POM capture API use `l_out-N` for the same
/// boundary. The target remains `l_out-N` so existing callers keep working.
fn capture_target_matches(name: &str, target: &CStr) -> bool {
    let Ok(target) = target.to_str() else {
        return false;
    };
    name == target
        || target
            .strip_prefix("l_out-")
            .is_some_and(|layer| name == format!("l_last-{layer}"))
}

/// Junta o `l_out` de mais um microlote ao residual deste decode.
///
/// **Acumula, não substitui.** Um decode não é um disparo: o llama.cpp parte o
/// lote em microlotes de `n_ubatch` e chama a captura uma vez por microlote,
/// cada uma com o `l_out` daquele pedaço.
///
/// Enquanto isto atribuía, o buffer terminava com o **último**
/// microlote e o resto do chunk ia embora em silêncio - com `fit_memory=true`
/// (padrão), que limita `n_ubatch` a 128, um chunk de 512 tokens perdia 3/4 e o
/// peer seguinte recusava a requisição com "got 655360 floats, expected
/// 2621440".
///
/// A ordem sai certa porque `split_simple` do llama.cpp fatia o lote em índices
/// crescentes e contíguos, que é exatamente a ordem das linhas que
/// `BatchLayout` usa para reencontrar cada item. O llama.cpp só escolhe
/// `split_simple` quando o cache tem um stream só
/// (`n_stream = kv_unified ? 1 : n_seq_max`); com vários streams ele usa
/// `split_equal`, que agrupa por sequência e trocaria o residual de cada item
/// sem erro nenhum. **É por isso que `kv_unified` é ligado incondicionalmente**
/// nos dois pontos que criam contexto - ver `kv_seq_budget`. Mexer nisso quebra
/// esta função em silêncio.
fn accumulate_capture(slot: &mut CaptureSlot, floats: Vec<f32>) {
    if let Some(expected) = slot.expected_floats {
        let current = slot.buf.as_ref().map_or(0, Vec::len);
        if current >= expected {
            // Gemma4 can evaluate the same named boundary more than once in a
            // decode. Once the requested batch is complete, later callbacks
            // are duplicate evaluations, not additional token rows.
            return;
        }
    }
    match slot.buf.as_mut() {
        Some(acumulado) => acumulado.extend_from_slice(&floats),
        None => slot.buf = Some(floats),
    }
}

fn residual_capture_layout(
    tensor_shape: [i64; 4],
    tensor_strides: [usize; 4],
    n_tokens: usize,
    n_embd: usize,
) -> Result<ResidualCaptureLayout> {
    if n_tokens == 0 {
        bail!("residual capture: quantidade de tokens vazia");
    }
    let dims = tensor_shape
        .into_iter()
        .map(|dim| {
            usize::try_from(dim)
                .map_err(|_| anyhow::anyhow!("residual capture: dimensão negativa {dim}"))
        })
        .collect::<Result<Vec<_>>>()?;
    if dims[0] != n_embd {
        bail!(
            "residual capture: dimensão 0 é {}, esperada n_embd {}",
            dims[0],
            n_embd
        );
    }

    let mut saw_token_axis = false;
    let mut saw_unsupported_trailing = false;
    let mut saw_invalid_stride = false;
    for token_axis in 1..dims.len() {
        if dims[token_axis] != n_tokens {
            continue;
        }
        saw_token_axis = true;
        if dims[token_axis + 1..].iter().any(|&dim| dim > 1) {
            saw_unsupported_trailing = true;
            continue;
        }
        let feature_width = dims[..token_axis]
            .iter()
            .try_fold(1usize, |width, &dimension| width.checked_mul(dimension))
            .ok_or_else(|| anyhow::anyhow!("residual capture: largura excede o limite"))?;
        let expected_stride = feature_width
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| anyhow::anyhow!("residual capture: stride excede o limite"))?;
        if tensor_strides[token_axis] != expected_stride {
            saw_invalid_stride = true;
            continue;
        }
        let mut stride = std::mem::size_of::<f32>();
        let mut contiguous = true;
        for axis in 0..=token_axis {
            if tensor_strides[axis] != stride {
                contiguous = false;
                break;
            }
            stride = stride
                .checked_mul(dims[axis])
                .ok_or_else(|| anyhow::anyhow!("residual capture: stride excede o limite"))?;
        }
        if !contiguous {
            saw_invalid_stride = true;
            continue;
        }
        // The scan is intentionally from low to high axes. When more than one
        // axis has the token count (most notably a one-token batch), the
        // lowest axis that satisfies the physical layout wins. This is
        // deterministic and avoids absorbing trailing singleton dimensions
        // into the canonical shape.
        return Ok(ResidualCaptureLayout {
            feature_shape: dims[..token_axis].to_vec(),
            token_axis,
        });
    }

    if saw_unsupported_trailing {
        bail!(
            "residual capture: shape {:?} tem dimensões posteriores ao eixo de tokens não suportadas",
            tensor_shape
        );
    }
    if saw_token_axis && saw_invalid_stride {
        bail!(
            "residual capture: shape {:?} não é contígua no eixo de tokens {}",
            tensor_shape,
            n_tokens
        );
    }
    bail!(
        "residual capture: shape {:?} não expõe o número de tokens {}",
        tensor_shape,
        n_tokens
    )
}

fn capture_microbatch_tokens(slot: &CaptureSlot) -> Result<usize> {
    // `expected_tokens` is the batch count supplied by the decode caller. The
    // callback sees only the tensor, so after the first capture the accumulated
    // feature rows tell us how many tokens already arrived; the next physical
    // microbatch is the remaining count capped by `n_ubatch`.
    if slot.expected_tokens == 0 {
        bail!("residual capture: quantidade esperada de tokens vazia");
    }
    if slot.max_microbatch_tokens == 0 {
        bail!("residual capture: limite de microlote vazio");
    }
    let captured_tokens = match &slot.layout {
        Some(layout) => {
            let feature_values = layout
                .feature_shape
                .iter()
                .try_fold(1usize, |values, &dimension| values.checked_mul(dimension))
                .ok_or_else(|| anyhow::anyhow!("residual capture: largura excede o limite"))?;
            if feature_values == 0 {
                bail!("residual capture: largura de features vazia");
            }
            let current = slot.buf.as_ref().map_or(0, Vec::len);
            if current % feature_values != 0 {
                bail!(
                    "residual capture: buffer acumulado de {} floats não fecha a largura de features {}",
                    current,
                    feature_values
                );
            }
            current / feature_values
        }
        None => 0,
    };
    let remaining = slot
        .expected_tokens
        .checked_sub(captured_tokens)
        .ok_or_else(|| {
            anyhow::anyhow!("residual capture: microlotes excederam o batch esperado")
        })?;
    Ok(remaining.min(slot.max_microbatch_tokens))
}

fn record_capture(
    slot: &mut CaptureSlot,
    tensor_shape: [i64; 4],
    tensor_strides: [usize; 4],
    n_bytes: usize,
    floats: Vec<f32>,
) -> Result<()> {
    if let Some(expected) = slot.expected_floats {
        let current = slot.buf.as_ref().map_or(0, Vec::len);
        if current >= expected {
            return Ok(());
        }
    }
    let microbatch_tokens = capture_microbatch_tokens(slot)?;
    let layout =
        residual_capture_layout(tensor_shape, tensor_strides, microbatch_tokens, slot.n_embd)?;
    let captured_bytes = floats
        .len()
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| anyhow::anyhow!("residual capture: tamanho excede o limite"))?;
    if captured_bytes != n_bytes {
        bail!(
            "residual capture: tensor shape {:?} declara {} bytes, mas a cópia tem {}",
            tensor_shape,
            n_bytes,
            captured_bytes
        );
    }
    if let Some(existing) = &slot.layout {
        if existing != &layout {
            bail!("residual capture: layout mudou entre microlotes ({existing:?} vs {layout:?})");
        }
    } else {
        let feature_values = layout
            .feature_shape
            .iter()
            .try_fold(1usize, |values, &dimension| values.checked_mul(dimension))
            .ok_or_else(|| anyhow::anyhow!("residual capture: largura excede o limite"))?;
        let expected = slot
            .expected_tokens
            .checked_mul(feature_values)
            .ok_or_else(|| anyhow::anyhow!("residual capture: tamanho esperado excede o limite"))?;
        slot.expected_floats = Some(expected);
        slot.layout = Some(layout);
    }
    let feature_shape = slot
        .layout
        .as_ref()
        .map(|layout| layout.feature_shape.as_slice())
        .ok_or_else(|| anyhow::anyhow!("residual capture: layout não definido"))?;
    let feature_values = feature_shape
        .iter()
        .try_fold(1usize, |values, &dimension| values.checked_mul(dimension))
        .ok_or_else(|| anyhow::anyhow!("residual capture: largura excede o limite"))?;
    if feature_values == 0 {
        bail!("residual capture: largura de features vazia");
    }
    let expected_microbatch = microbatch_tokens
        .checked_mul(feature_values)
        .ok_or_else(|| anyhow::anyhow!("residual capture: tamanho do microlote excede o limite"))?;
    if floats.len() != expected_microbatch {
        bail!(
            "residual capture: microlote tem {} floats, esperado {} para {} tokens e features {:?}",
            floats.len(),
            expected_microbatch,
            microbatch_tokens,
            feature_shape
        );
    }
    let expected = slot.expected_floats.unwrap_or(0);
    let current = slot.buf.as_ref().map_or(0, Vec::len);
    if current >= expected {
        return Ok(());
    }
    let Some(end) = current.checked_add(floats.len()) else {
        bail!("residual capture: tamanho do microlote excede o limite numérico");
    };
    if end > expected {
        bail!(
            "residual capture: microlote excede o batch esperado ({} + {} > {})",
            current,
            floats.len(),
            expected
        );
    }
    accumulate_capture(slot, floats);
    Ok(())
}

fn take_captured_residual(slot: &mut CaptureSlot) -> Result<CapturedResidual> {
    if let Some(error) = slot.error.take() {
        bail!(error);
    }
    let expected = slot
        .expected_floats
        .ok_or_else(|| anyhow::anyhow!("residual capture did not observe a tensor"))?;
    let values = slot
        .buf
        .take()
        .ok_or_else(|| anyhow::anyhow!("residual capture did not fire"))?;
    if values.len() != expected {
        bail!(
            "residual capture: got {} floats, expected {}",
            values.len(),
            expected
        );
    }
    let layout = slot
        .layout
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("residual capture did not record a layout"))?;
    let mut shape = Vec::with_capacity(layout.feature_shape.len() + 1);
    shape.push(slot.expected_tokens);
    shape.extend_from_slice(&layout.feature_shape);
    Ok(CapturedResidual { values, shape })
}

// ── Ponte de diagnóstico ─────────────────────────────────────────────────────

/// Chama uma função `llama_rs_*_json` e devolve o JSON já em `serde_json`.
///
/// Toda a família devolve `LLAMA_RS_STATUS_OK` mais uma string de `malloc` que
/// **tem que** voltar por `llama_rs_string_free` - `free` de outra alocação, ou
/// um `CString::from_raw`, é liberar com um alocador que não é o dono. Por isso
/// a liberação mora aqui e não em cada chamador.
///
/// # Safety
/// `f` tem que ser uma dessas funções, com os ponteiros do llama.cpp ainda
/// vivos.
unsafe fn json_from_wrapper(
    f: impl FnOnce(*mut *mut std::os::raw::c_char) -> sys::llama_rs_status,
) -> Option<serde_json::Value> {
    let mut raw: *mut std::os::raw::c_char = ptr::null_mut();
    let status = f(&mut raw);
    if status != sys::LLAMA_RS_STATUS_OK || raw.is_null() {
        warn!("diagnóstico do llama.cpp indisponível (status {status})");
        return None;
    }
    let texto = CStr::from_ptr(raw).to_string_lossy().into_owned();
    sys::llama_rs_string_free(raw);
    match serde_json::from_str(&texto) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("diagnóstico do llama.cpp veio com JSON inválido: {e}");
            None
        }
    }
}

fn model_metadata_string(model: *const sys::llama_model, key: &str) -> Option<String> {
    let key = CString::new(key).ok()?;
    let mut buf = vec![0u8; 32768];
    let n = unsafe {
        sys::llama_model_meta_val_str(model, key.as_ptr(), buf.as_mut_ptr() as *mut i8, buf.len())
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// O lado dominante dos buffers `context` + `compute` de um contexto MTP.
/// Buffers host pequenos de staging não transformam um contexto cujo KV e
/// grafo estão na VRAM em um recuo para RAM; quando a maioria fica no host, o
/// operador precisa ver RAM no painel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MtpAllocation {
    Vram,
    Ram,
}

impl MtpAllocation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Vram => "vram",
            Self::Ram => "ram",
        }
    }
}

pub(crate) fn mtp_allocation_from_context_memory(
    value: &serde_json::Value,
) -> Option<MtpAllocation> {
    let mut vram_bytes = 0u64;
    let mut ram_bytes = 0u64;
    for buffer in value.get("buffers")?.as_array()? {
        let bytes = buffer
            .get("context")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            .saturating_add(
                buffer
                    .get("compute")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            );
        if bytes == 0 {
            continue;
        }
        let is_host = buffer
            .get("host")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or_else(|| {
                buffer
                    .get("device")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|device| device.eq_ignore_ascii_case("CPU"))
            });
        if is_host {
            ram_bytes = ram_bytes.saturating_add(bytes);
        } else {
            vram_bytes = vram_bytes.saturating_add(bytes);
        }
    }
    match (vram_bytes, ram_bytes) {
        (0, 0) => None,
        (vram, ram) if ram > vram => Some(MtpAllocation::Ram),
        _ => Some(MtpAllocation::Vram),
    }
}

pub(crate) fn context_allocation(ctx: *const sys::llama_context) -> Option<MtpAllocation> {
    let value = unsafe { json_from_wrapper(|out| sys::llama_rs_context_memory_json(ctx, out)) }?;
    mtp_allocation_from_context_memory(&value)
}

// ── Configuration ─────────────────────────────────────────────────────────────

pub(crate) const MTP_NO_NEXTN_REASON: &str = "o modelo carregado não traz o bloco nextn";

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Capacidade total do cache KV. O runtime já multiplica a janela pública
    /// por sessão por `max_sessions` antes de montar esta configuração.
    pub n_ctx: u32,
    pub offload_kqv: bool,
    pub cache_type_k: KvCacheType,
    pub cache_type_v: KvCacheType,
    pub fit_memory: bool,
    /// VRAM que fica livre após a projeção do backend, por dispositivo.
    /// Processos atuais já ficam fora do snapshot de memória livre do backend;
    /// este campo é somente a reserva de segurança configurável.
    pub fit_memory_margin_mb: u32,
    pub n_batch: u32,
    pub n_gpu_layers: i32,
    pub n_threads: i32,
    pub max_sessions: u32,
    /// True for non-last peers that need to hand the residual to the next peer.
    pub need_embeddings: bool,
    /// Ligar a predição multi-token pelo bloco `nextn` do modelo, quando ele
    /// tiver um. Distinto da especulação por lookup (`find_draft`) - ver
    /// `crate::mtp`. Um modelo sem `nextn` não é erro de configuração: o
    /// contexto informa essa ausência para a telemetria e segue sem MTP.
    pub mtp: bool,
    /// Permite recuar o bloco `nextn` para pesos e cache na RAM se a tentativa
    /// com a VRAM falhar. Só é lido quando `mtp` está ligado.
    pub mtp_cpu_fallback_enabled: bool,
    /// Quantos snapshots de estado recorrente manter por sequência
    /// (`llama_context_params::n_rs_seq`), para permitir desfazer um
    /// rascunho especulativo recusado num modelo híbrido/recorrente
    /// (Qwen3.5+: `llm_arch_supports_rs_rollback` no llama.cpp). `0` é o
    /// padrão do llama.cpp - sem isto, `llama_memory_recurrent::seq_rm`
    /// nunca aceita um rollback parcial, e vale a pena pedir um valor maior
    /// que a maior largura de lote especulativo configurada (`speculative_k`,
    /// `mtp_draft_tokens + 1`); arquitetura sem suporte simplesmente zera o
    /// pedido de volta (`llama-context.cpp`), então pedir aqui não machuca
    /// modelo comum. Ver `infer_server::should_draft` e
    /// `gateway::rollback_sessions`, que são o resto do mecanismo.
    pub rs_seq_window: u32,
    /// Load routed MoE expert metadata without allocating expert weights in llama.cpp.
    pub moe_external_executor: bool,
    /// Per-layer ownership passed to the generic llama.cpp hook. Zero keeps
    /// the native experts, one delegates the whole layer to the device cache.
    pub moe_external_executor_layers: Vec<u8>,
}

pub(crate) fn apply_cache_types(
    cparams: &mut sys::llama_context_params,
    cache_type_k: KvCacheType,
    cache_type_v: KvCacheType,
) {
    cparams.type_k = cache_type_to_ggml(cache_type_k);
    cparams.type_v = cache_type_to_ggml(cache_type_v);
}

fn cache_type_to_ggml(cache_type: KvCacheType) -> sys::ggml_type {
    match cache_type {
        KvCacheType::F32 => sys::GGML_TYPE_F32,
        KvCacheType::F16 => sys::GGML_TYPE_F16,
        KvCacheType::Bf16 => sys::GGML_TYPE_BF16,
        KvCacheType::Q8_0 => sys::GGML_TYPE_Q8_0,
        KvCacheType::Q4_0 => sys::GGML_TYPE_Q4_0,
        KvCacheType::Q4_1 => sys::GGML_TYPE_Q4_1,
        KvCacheType::Iq4Nl => sys::GGML_TYPE_IQ4_NL,
        KvCacheType::Q5_0 => sys::GGML_TYPE_Q5_0,
        KvCacheType::Q5_1 => sys::GGML_TYPE_Q5_1,
    }
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// Os pesos, carregados uma vez e compartilhados por todos os contextos.
///
/// Separar isto do contexto é o que torna o paralelismo viável sem duplicar
/// memória: o que pesa são os pesos (gigabytes), e o llama.cpp os trata como
/// somente-leitura depois da carga - vários `llama_context` podem apontar para
/// o mesmo `llama_model`. O contexto carrega o cache KV; o `n_ctx` interno já
/// é a capacidade total necessária para todas as sequências.
pub struct LoadedModel {
    model: *mut sys::llama_model,
    vocab: *const sys::llama_vocab,
    pub n_embd: usize,
    pub n_vocab: usize,
    /// Whether this model instance has the optional `nextn` tensors loaded.
    /// `llama_model_n_layer_nextn` only reports metadata and remains positive
    /// when the caller deliberately loaded the target with `load_mtp=false`.
    mtp_weights_loaded: bool,
}

// SAFETY: depois da carga o `llama_model` é somente-leitura; é o próprio
// llama.cpp que permite vários contextos sobre um modelo.
unsafe impl Send for LoadedModel {}
unsafe impl Sync for LoadedModel {}

/// Primeiro palpite para o buffer do template de chat. Não é limite: quando o
/// llama.cpp informa que precisa de mais, `apply_chat_template` realoca com o
/// tamanho exato e renderiza de novo.
const CHAT_TEMPLATE_INITIAL_BUF: usize = 65536;

/// Quantas sequências de KV o contexto precisa reservar para `max_sessions`
/// sessões simultâneas.
///
/// **Duas por sessão**: uma guarda o prompt e ninguém escreve nela; a outra
/// recebe a geração do turno e é apagada inteira no fim (ver
/// `LlmEngine::kv_copy_seq` e `infer_server::InferState::open_generation`). O
/// par é fixo - a sequência de geração da sessão que ocupa `seq` é
/// `seq + max_sessions` -, então nunca falta slot de geração para uma sessão
/// que já tem slot de prompt.
///
/// Vem sempre acompanhado de `kv_unified = true`: sem ele o llama.cpp fatia
/// `n_ctx` por sequência (`n_ctx_seq = n_ctx / n_seq_max`) e dobrar as
/// sequências cortaria a janela de prompt pela metade. Unificado, as duas
/// sequências dividem as mesmas células, a janela continua sendo `n_ctx`
/// inteiro e a cópia prompt→geração não move tensor nenhum.
///
/// **O que isto custa, declarado.** Duas famílias de cache não compartilham
/// células e passam a reservar de verdade duas fatias por sessão:
///
/// - o estado recorrente (`llama_memory_recurrent`, dimensionado por
///   `n_seq_max`), porque estado de prompt e estado de geração são estados
///   diferentes - não há como um modelo Mamba/GDN reter o prompt sem guardar o
///   estado dele em separado;
/// - o cache de estados com múltiplos fluxos, que hoje ignora o pedido de
///   unificação (`unified_raw = false`) e mantém um stream por sequência.
///
/// Em ambos os casos o `fit_memory` (`llama_rs_fit_params`, que recebe estes
/// mesmos `cparams`) dimensiona o contexto já contando as duas sequências, e
/// `/health` publica `n_seq_max` para o operador conferir. A alternativa seria
/// oferecer o cache só a alguns modelos, que é exatamente o que a decisão de
/// produto recusou.
pub fn kv_seq_budget(max_sessions: u32) -> u32 {
    max_sessions.max(1).saturating_mul(2)
}

const DEFAULT_GPU_LAYERS: i32 = 99;
const FIT_MEMORY_MIN_MARGIN_BYTES: usize = 4 * 1024 * 1024;
const FIT_MIN_CONTEXT: u32 = 4096;
/// The CUDA graph and kernel workspaces scale with the microbatch, while the
/// public `n_batch` can stay large enough to accept a prompt chunk. A smaller
/// ubatch is the safe automatic fallback before llama.cpp reserves those
/// workspaces on a nearly-full GPU.
const FIT_MAX_UBATCH: u32 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FitDeviceMemory {
    total_bytes: usize,
    free_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FitMemorySnapshot {
    total_bytes: usize,
    free_bytes: usize,
    margin_bytes: usize,
}

fn fit_memory_configured_margin_bytes(configured_margin_mb: u32) -> usize {
    usize::try_from(u64::from(configured_margin_mb) * 1024 * 1024)
        .unwrap_or(usize::MAX)
        .max(FIT_MEMORY_MIN_MARGIN_BYTES)
}

/// Returns the amount that `common_fit_params` should leave unused on one
/// device. The backend's `free_bytes` is already the memory available to POM:
/// CUDA obtains it from `cudaMemGetInfo`, so allocations owned by other
/// processes are already excluded. The margin therefore covers only allocator
/// and late-workspace drift, plus a process that starts after this snapshot.
///
/// When the card has less free memory than the requested reserve, using the
/// whole free amount would make every positive allocation fail the fit. A
/// smaller reserve is still useful until the card cannot even provide the
/// minimum safety gap; at that point zero is the only honest target.
fn fit_memory_margin_bytes(memory: FitDeviceMemory, configured_margin_mb: u32) -> usize {
    let free_bytes = memory.free_bytes.min(memory.total_bytes);
    if free_bytes < FIT_MEMORY_MIN_MARGIN_BYTES {
        return 0;
    }
    free_bytes.min(fit_memory_configured_margin_bytes(configured_margin_mb))
}

fn fit_memory_margins(
    memories: &[FitDeviceMemory],
    n_devices: usize,
    configured_margin_mb: u32,
) -> Vec<usize> {
    let fallback = fit_memory_configured_margin_bytes(configured_margin_mb);
    (0..n_devices)
        .map(|index| {
            memories
                .get(index)
                .map(|memory| fit_memory_margin_bytes(*memory, configured_margin_mb))
                .unwrap_or(fallback)
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MtpFitMode {
    Disabled,
    Vram,
    Ram,
}

fn mtp_fit_mode_for_model(
    requested: MtpFitMode,
    has_nextn: bool,
    architecture: Option<&str>,
) -> (MtpFitMode, Option<String>) {
    if requested == MtpFitMode::Disabled || !has_nextn {
        return (mtp_mode_for_nextn_availability(requested, has_nextn), None);
    }

    if let Some(reason) = crate::mtp::mtp_graph_unavailable_reason(architecture) {
        return (MtpFitMode::Disabled, Some(reason));
    }

    (requested, None)
}

fn mtp_mode_for_nextn_availability(requested: MtpFitMode, has_nextn: bool) -> MtpFitMode {
    if !has_nextn {
        MtpFitMode::Disabled
    } else {
        requested
    }
}

/// O fit conjunto só pode habilitar o MTP se conservar o encaixe integral que
/// o modelo principal obteve sozinho. `common_fit_params` usa a mesma contagem
/// para as camadas principais e as camadas `nextn`; aceitar um resultado
/// reduzido aqui faria o sufixo MTP empurrar camadas principais para a CPU.
fn choose_mtp_fit_mode(
    requested: MtpFitMode,
    primary_gpu_layers: i32,
    mtp_gpu_layers: Option<i32>,
) -> MtpFitMode {
    match requested {
        MtpFitMode::Disabled => MtpFitMode::Disabled,
        MtpFitMode::Vram if primary_gpu_layers == -1 && mtp_gpu_layers == Some(-1) => {
            MtpFitMode::Vram
        }
        MtpFitMode::Vram | MtpFitMode::Ram => MtpFitMode::Ram,
    }
}

fn mtp_mode_without_gpu_fit(requested: MtpFitMode) -> MtpFitMode {
    match requested {
        MtpFitMode::Disabled => MtpFitMode::Disabled,
        MtpFitMode::Vram | MtpFitMode::Ram => MtpFitMode::Ram,
    }
}

fn fit_memory_margins_with_mtp(
    margins: &[usize],
    mtp_cost: &[usize],
    mode: MtpFitMode,
) -> Vec<usize> {
    margins
        .iter()
        .enumerate()
        .map(|(index, &margin)| {
            if mode == MtpFitMode::Vram {
                margin.saturating_add(mtp_cost.get(index).copied().unwrap_or(0))
            } else {
                margin
            }
        })
        .collect()
}

/// Calcula apenas o cache KV do contexto MTP, sem abrir o modelo nem montar
/// seu grafo. O espaço de compute é deliberadamente deixado para a tentativa
/// real de `llama_init_from_model`, que pode recuar para a RAM se não couber.
/// Como o bloco nextn fica no fim, sua reserva vai para o último dispositivo
/// na ordem slow-to-fast usada pelo fit.
fn estimate_mtp_vram_costs(
    info: &crate::gguf::GgufInfo,
    cfg: &EngineConfig,
    n_devices: usize,
) -> Option<Vec<usize>> {
    let mut costs = vec![0usize; n_devices];
    if !cfg.offload_kqv || n_devices == 0 {
        return Some(costs);
    }

    let block_count = info.block_count?;
    let last_layer = block_count.checked_sub(1)?;
    let nextn_layers = info.nextn_predict_layers.unwrap_or(1).max(1);
    let first_nextn_layer = block_count.checked_sub(nextn_layers)?;
    let geometry = info
        .kv_geometry_for_range(first_nextn_layer, last_layer)
        .ok()?;
    let bytes = crate::kv_budget::kv_bytes_for_layers(
        u64::from(cfg.n_ctx),
        u64::from(kv_seq_budget(cfg.max_sessions)),
        &geometry,
        cfg.cache_type_k,
        cfg.cache_type_v,
    )
    .ok()?;
    costs[n_devices - 1] = usize::try_from(bytes).ok()?;
    Some(costs)
}

fn fit_memory_snapshots(memories: &[FitDeviceMemory], margins: &[usize]) -> Vec<FitMemorySnapshot> {
    margins
        .iter()
        .enumerate()
        .map(|(index, &margin_bytes)| {
            let memory = memories.get(index).copied().unwrap_or(FitDeviceMemory {
                total_bytes: 0,
                free_bytes: 0,
            });
            FitMemorySnapshot {
                total_bytes: memory.total_bytes,
                free_bytes: memory.free_bytes,
                margin_bytes,
            }
        })
        .collect()
}

#[cfg(test)]
mod fit_memory_margin_tests {
    use super::*;

    const MIB: usize = 1024 * 1024;

    fn memory(total_mib: usize, free_mib: usize) -> FitDeviceMemory {
        FitDeviceMemory {
            total_bytes: total_mib * MIB,
            free_bytes: free_mib * MIB,
        }
    }

    #[test]
    fn sem_outros_processos_reserva_o_padrao_pequeno() {
        assert_eq!(
            fit_memory_margin_bytes(memory(16 * 1024, 16 * 1024), 256),
            256 * MIB
        );
    }

    #[test]
    fn processos_existentes_nao_sao_descontados_duas_vezes() {
        assert_eq!(
            fit_memory_margin_bytes(memory(16 * 1024, 4 * 1024), 256),
            256 * MIB
        );
    }

    #[test]
    fn memoria_livre_menor_que_o_pedido_reduz_a_reserva() {
        assert_eq!(
            fit_memory_margin_bytes(memory(16 * 1024, 64), 256),
            64 * MIB
        );
    }

    #[test]
    fn memoria_insuficiente_ate_para_o_minimo_nao_faz_underflow() {
        assert_eq!(fit_memory_margin_bytes(memory(16 * 1024, 2), 256), 0);
    }

    #[test]
    fn margens_sao_calculadas_independentemente_para_cada_dispositivo() {
        let margins = fit_memory_margins(
            &[
                memory(16 * 1024, 16 * 1024),
                memory(24 * 1024, 64),
                memory(8 * 1024, 2),
            ],
            3,
            256,
        );

        assert_eq!(margins, vec![256 * MIB, 64 * MIB, 0]);
    }

    #[test]
    fn snapshot_do_fit_preserva_memoria_e_orcamento_por_dispositivo() {
        let memories = vec![memory(16 * 1024, 15 * 1024), memory(24 * 1024, 20 * 1024)];
        let margins = fit_memory_margins(&memories, memories.len(), 256);

        assert_eq!(
            fit_memory_snapshots(&memories, &margins),
            vec![
                FitMemorySnapshot {
                    total_bytes: 16 * 1024 * MIB,
                    free_bytes: 15 * 1024 * MIB,
                    margin_bytes: 256 * MIB,
                },
                FitMemorySnapshot {
                    total_bytes: 24 * 1024 * MIB,
                    free_bytes: 20 * 1024 * MIB,
                    margin_bytes: 256 * MIB,
                },
            ]
        );
    }

    #[test]
    fn arquivo_sem_nextn_nao_pede_carga_nem_contexto_mtp() {
        assert_eq!(
            mtp_mode_for_nextn_availability(MtpFitMode::Vram, false),
            MtpFitMode::Disabled
        );
        assert_eq!(
            mtp_mode_for_nextn_availability(MtpFitMode::Ram, false),
            MtpFitMode::Disabled
        );
        assert_eq!(
            mtp_mode_for_nextn_availability(MtpFitMode::Vram, true),
            MtpFitMode::Vram
        );
    }

    #[test]
    fn arquitetura_com_nextn_sem_grafo_desliga_o_fit_mtp_com_motivo() {
        let (mode, reason) = mtp_fit_mode_for_model(MtpFitMode::Vram, true, Some("graniteswitch"));

        assert_eq!(mode, MtpFitMode::Disabled);
        assert_eq!(
            reason.as_deref(),
            Some("a arquitetura graniteswitch não possui grafo MTP suportado pelo llama.cpp")
        );
    }

    #[test]
    fn arquitetura_com_grafo_mtp_mantem_a_tentativa_em_vram() {
        assert_eq!(
            mtp_fit_mode_for_model(MtpFitMode::Vram, true, Some("qwen35moe")),
            (MtpFitMode::Vram, None)
        );
    }

    #[test]
    fn reserva_mtp_analitica_calcula_so_o_kv_e_coloca_no_ultimo_dispositivo() {
        let info = crate::gguf::GgufInfo {
            block_count: Some(41),
            nextn_predict_layers: Some(1),
            attention_head_count_kv: Some(vec![2]),
            attention_key_length: Some(vec![64]),
            attention_value_length: Some(vec![64]),
            ..Default::default()
        };
        let cfg = EngineConfig {
            n_ctx: 4096,
            max_sessions: 1,
            offload_kqv: true,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            fit_memory: true,
            fit_memory_margin_mb: 256,
            n_batch: 128,
            n_gpu_layers: 99,
            n_threads: 0,
            need_embeddings: false,
            mtp: true,
            mtp_cpu_fallback_enabled: true,
            rs_seq_window: 0,
            moe_external_executor: false,
            moe_external_executor_layers: Vec::new(),
        };

        assert_eq!(
            estimate_mtp_vram_costs(&info, &cfg, 2),
            Some(vec![0, 2 * 4096 * 2 * 64 * 2 * 2])
        );
    }

    #[test]
    fn pedido_mtp_reserva_o_custo_do_segundo_contexto_em_cada_dispositivo() {
        let margins = vec![256 * MIB, 256 * MIB];
        let mtp_cost = vec![727 * MIB, 311 * MIB];

        assert_eq!(
            fit_memory_margins_with_mtp(&margins, &mtp_cost, MtpFitMode::Vram),
            vec![983 * MIB, 567 * MIB]
        );
    }

    #[test]
    fn mtp_apertado_nao_pode_reduzir_camadas_principais() {
        // Reproduz a inversão de 27fda8b: o fit principal cabe inteiro, mas
        // o fit conjunto só cabe se remover camadas. Nesse caso o MTP cede.
        assert_eq!(
            choose_mtp_fit_mode(MtpFitMode::Vram, -1, Some(50)),
            MtpFitMode::Ram
        );
    }

    #[test]
    fn mtp_em_vram_so_e_aceito_quando_o_fit_conjunto_tambem_cabe() {
        assert_eq!(
            choose_mtp_fit_mode(MtpFitMode::Vram, -1, Some(-1)),
            MtpFitMode::Vram
        );
        assert_eq!(
            choose_mtp_fit_mode(MtpFitMode::Vram, -1, None),
            MtpFitMode::Ram
        );
    }

    #[test]
    fn principal_reduzido_nao_da_direito_ao_mtp_na_vram() {
        assert_eq!(
            choose_mtp_fit_mode(MtpFitMode::Vram, 50, Some(-1)),
            MtpFitMode::Ram
        );
    }

    #[test]
    fn mtp_desligado_mantem_a_distribuicao_do_fit_principal() {
        assert_eq!(
            choose_mtp_fit_mode(MtpFitMode::Disabled, 50, Some(50)),
            MtpFitMode::Disabled
        );
    }

    #[test]
    fn mtp_desmarcado_nao_reserva_orcamento() {
        let margins = vec![256 * MIB, 256 * MIB];
        let mtp_cost = vec![727 * MIB, 311 * MIB];

        assert_eq!(
            fit_memory_margins_with_mtp(&margins, &mtp_cost, MtpFitMode::Disabled),
            margins
        );
    }

    #[test]
    fn mtp_desmarcado_nao_ativa_o_preflight_da_arquitetura() {
        assert_eq!(
            mtp_fit_mode_for_model(MtpFitMode::Disabled, true, Some("graniteswitch")),
            (MtpFitMode::Disabled, None)
        );
    }
}

/// Parâmetros do modelo já ajustados pelo `common_fit_params`.
///
/// Os vetores ficam vivos até `llama_model_load_from_file` consumir os
/// ponteiros guardados em `llama_model_params`. Isso é necessário no caso de
/// mais de uma GPU local, em que o fit também escreve `tensor_split` e
/// `tensor_buft_overrides`.
struct FitPlan {
    config: EngineConfig,
    tensor_split: Vec<f32>,
    tensor_buft_overrides: Vec<sys::llama_model_tensor_buft_override>,
    /// A NULL-terminated device list, kept alive for the model load. It is
    /// empty when speed could not be ranked confidently, preserving llama's
    /// default device selection.
    devices: Vec<sys::ggml_backend_dev_t>,
    /// Whether the target model should load its optional `nextn` weights in
    /// VRAM. `Ram` keeps those weights out of the target so the CPU companion
    /// can own them without changing the primary placement.
    mtp_mode: MtpFitMode,
    /// Razão para manter o MTP desligado quando a metadata anuncia `nextn`,
    /// mas o fork não possui um grafo seguro para a arquitetura.
    mtp_unavailable_reason: Option<String>,
    applied: bool,
}

fn fit_can_adjust_layers(cfg: &EngineConfig) -> bool {
    cfg.fit_memory && cfg.n_gpu_layers == DEFAULT_GPU_LAYERS
}

fn empty_fit_plan(cfg: &EngineConfig) -> FitPlan {
    let mtp_mode = if cfg.mtp {
        MtpFitMode::Vram
    } else {
        MtpFitMode::Disabled
    };
    empty_fit_plan_with_mtp_mode(cfg, mtp_mode)
}

fn empty_fit_plan_with_mtp_mode(cfg: &EngineConfig, mtp_mode: MtpFitMode) -> FitPlan {
    empty_fit_plan_with_mtp_mode_and_reason(cfg, mtp_mode, None)
}

fn empty_fit_plan_with_mtp_mode_and_reason(
    cfg: &EngineConfig,
    mtp_mode: MtpFitMode,
    mtp_unavailable_reason: Option<String>,
) -> FitPlan {
    FitPlan {
        config: cfg.clone(),
        tensor_split: Vec::new(),
        tensor_buft_overrides: Vec::new(),
        devices: Vec::new(),
        mtp_mode,
        mtp_unavailable_reason,
        applied: false,
    }
}

/// O POM exporta o residual pelo callback, mas o llama.cpp também pode tentar
/// copiar o resultado de embeddings. Essa cópia usa `n_embd_out` como se fosse
/// a largura real do tensor; quando as duas larguras divergem, o pedido pode
/// ultrapassar `ggml_nbytes(tensor)` antes de o callback ser consumido.
fn generic_embeddings_compatible(need_embeddings: bool, n_embd: usize, n_embd_out: usize) -> bool {
    need_embeddings && n_embd == n_embd_out
}

fn backend_registry_name(device: sys::ggml_backend_dev_t) -> Option<String> {
    if device.is_null() {
        return None;
    }

    let registry = unsafe { sys::ggml_backend_dev_backend_reg(device) };
    if registry.is_null() {
        return None;
    }

    let name = unsafe { sys::ggml_backend_reg_name(registry) };
    if name.is_null() {
        return None;
    }

    unsafe { CStr::from_ptr(name).to_str().ok().map(str::to_owned) }
}

pub(crate) fn effective_n_ubatch(cfg: &EngineConfig) -> u32 {
    let n_batch = cfg.n_batch.max(1);
    if cfg.fit_memory {
        n_batch.min(FIT_MAX_UBATCH)
    } else {
        n_batch
    }
}

/// Aplica a política única de lote público e microlote de computação a cada
/// contexto que o motor cria, inclusive o contexto MTP.
pub(crate) fn apply_context_batch_params(
    cparams: &mut sys::llama_context_params,
    cfg: &EngineConfig,
) {
    cparams.n_batch = cfg.n_batch;
    cparams.n_ubatch = effective_n_ubatch(cfg);
}

fn fit_plan(model_path: &str, cfg: &EngineConfig, mtp_mode: MtpFitMode) -> Result<FitPlan> {
    let (mtp_mode, mtp_unavailable_reason) = if mtp_mode == MtpFitMode::Disabled {
        (MtpFitMode::Disabled, None)
    } else {
        let path = std::path::Path::new(model_path);
        let has_nextn = crate::gguf::has_nextn_tensors(path).unwrap_or(false);
        let architecture = crate::gguf::read_info(path)
            .ok()
            .and_then(|info| info.architecture);
        let (mode, reason) = mtp_fit_mode_for_model(mtp_mode, has_nextn, architecture.as_deref());
        if !has_nextn {
            tracing::info!(
                model = %model_path,
                "MTP indisponível: o arquivo não traz tensores nextn"
            );
        } else if let Some(reason) = reason.as_deref() {
            warn!(model = %model_path, reason, "MTP indisponível para a arquitetura do modelo");
        }
        (mode, reason)
    };

    if !fit_can_adjust_layers(cfg) {
        if cfg.fit_memory && cfg.n_gpu_layers != DEFAULT_GPU_LAYERS {
            warn!(
                "llama.cpp fit ignorado porque n_gpu_layers foi definido explicitamente como {}",
                cfg.n_gpu_layers
            );
        }
        return Ok(empty_fit_plan_with_mtp_mode_and_reason(
            cfg,
            mtp_mode_without_gpu_fit(mtp_mode),
            mtp_unavailable_reason,
        ));
    }

    let path = CString::new(model_path)
        .with_context(|| format!("invalid model path for llama.cpp fit: {model_path}"))?;
    // `common_fit_params` queries the backend devices and their free memory.
    // The call is idempotent and mirrors `LoadedModel::load`, which also keeps
    // the initialization close to every possible model-loading entry point.
    unsafe { sys::llama_backend_init() };
    let n_devices = unsafe { sys::llama_max_devices() }.max(1);
    let n_overrides = unsafe { sys::llama_max_tensor_buft_overrides() }.max(1);

    // Take the memory snapshot before the speed probes allocate their small
    // benchmark graph. This preserves the fit snapshot timing guarantee and
    // makes the measured order orthogonal to the memory budget.
    let mut available_devices = vec![ptr::null_mut(); n_devices];
    let available_count = unsafe {
        sys::llama_rs_get_devices(available_devices.as_mut_ptr(), available_devices.len())
    };
    available_devices.truncate(available_count.min(n_devices));
    let mut device_memories = vec![
        sys::llama_rs_device_memory {
            free_bytes: 0,
            total_bytes: 0,
        };
        n_devices
    ];
    let memory_count = if available_devices.is_empty() {
        // Keep the old wrapper path for an empty device registry so a missing
        // snapshot continues to take the existing safe fit fallback.
        let count = unsafe {
            sys::llama_rs_get_device_memory(device_memories.as_mut_ptr(), device_memories.len())
        };
        device_memories.truncate(count.min(n_devices));
        count
    } else {
        for (memory, &device) in device_memories.iter_mut().zip(&available_devices) {
            let mut free_bytes = 0;
            let mut total_bytes = 0;
            unsafe { sys::ggml_backend_dev_memory(device, &mut free_bytes, &mut total_bytes) };
            memory.free_bytes = free_bytes;
            memory.total_bytes = total_bytes;
        }
        device_memories.truncate(available_devices.len());
        available_devices.len()
    };

    let memory_snapshot_complete = device_memories
        .iter()
        .all(|memory| memory.total_bytes > 0 && memory.free_bytes <= memory.total_bytes);
    let speed_scores = if available_devices.len() > 1 && memory_snapshot_complete {
        available_devices
            .iter()
            .map(|&device| {
                let score = unsafe { sys::llama_rs_benchmark_device(device) };
                if score.is_finite() && score > 0.0 {
                    Some(f64::from(score))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let backend_names = available_devices
        .iter()
        .map(|&device| backend_registry_name(device))
        .collect::<Vec<_>>();
    let backend_name_refs = backend_names
        .iter()
        .map(Option::as_deref)
        .collect::<Vec<_>>();
    let ordered_devices = device_placement::confident_slow_to_fast_order_for_backends(
        &speed_scores,
        &backend_name_refs,
    )
    .filter(|order| {
        order
            .iter()
            .enumerate()
            .any(|(position, &original)| position != original)
    })
    .map(|order| {
        let mut devices = order
            .iter()
            .map(|&index| available_devices[index])
            .collect::<Vec<_>>();
        let reordered_memories = order
            .iter()
            .map(|&index| device_memories[index])
            .collect::<Vec<_>>();
        device_memories = reordered_memories;
        devices.push(ptr::null_mut());
        devices
    })
    .unwrap_or_default();
    if available_devices.len() > 1 {
        tracing::info!(
            device_backend_names = ?backend_names,
            device_speed_scores = ?speed_scores,
            speed_order_applied = !ordered_devices.is_empty(),
            "llama.cpp fit sondou a velocidade relativa dos dispositivos"
        );
    }
    let fit_memories = device_memories
        .iter()
        .map(|memory| FitDeviceMemory {
            total_bytes: memory.total_bytes,
            free_bytes: memory.free_bytes,
        })
        .collect::<Vec<_>>();
    let mut margins = fit_memory_margins(&fit_memories, n_devices, cfg.fit_memory_margin_mb);
    if memory_count != n_devices {
        warn!(
            reported_devices = memory_count,
            expected_devices = n_devices,
            "llama.cpp fit recebeu um snapshot parcial de memória"
        );
    }
    let snapshots = fit_memory_snapshots(&fit_memories, &margins);
    let process_gpu_memory_mb = crate::sysmon::current_process_gpu_memory_mb();
    tracing::info!(
        process_id = std::process::id(),
        process_gpu_memory_mb = process_gpu_memory_mb.unwrap_or(0.0),
        process_gpu_memory_known = process_gpu_memory_mb.is_some(),
        "llama.cpp fit memória do processo no ponto da sondagem"
    );
    for (device, snapshot) in snapshots.iter().enumerate() {
        tracing::info!(
            device,
            total_bytes = snapshot.total_bytes,
            free_bytes = snapshot.free_bytes,
            margin_bytes = snapshot.margin_bytes,
            budget_bytes = snapshot.free_bytes.saturating_sub(snapshot.margin_bytes),
            "llama.cpp fit memória e orçamento por dispositivo"
        );
    }
    let make_model_params = |load_mtp: bool| {
        let mut params = unsafe { sys::llama_model_default_params() };
        params.n_gpu_layers = -1;
        if !ordered_devices.is_empty() {
            // `ordered_devices` includes the NULL sentinel required by
            // llama.cpp. Keep it in FitPlan so the same order reaches the real
            // model load.
            params.devices = ordered_devices.as_ptr() as *mut sys::ggml_backend_dev_t;
        }
        params.load_mtp = load_mtp;
        params.moe_external_executor = cfg.moe_external_executor;
        if !cfg.moe_external_executor_layers.is_empty() {
            params.moe_external_executor_layers = cfg.moe_external_executor_layers.as_ptr();
            params.moe_external_executor_layer_count = cfg.moe_external_executor_layers.len();
        }
        params
    };
    let make_context_params = || {
        let mut params = unsafe { sys::llama_context_default_params() };
        params.n_ctx = cfg.n_ctx;
        apply_context_batch_params(&mut params, cfg);
        params.n_seq_max = kv_seq_budget(cfg.max_sessions);
        params.kv_unified = true;
        params.offload_kqv = cfg.offload_kqv;
        apply_cache_types(&mut params, cfg.cache_type_k, cfg.cache_type_v);
        params.embeddings = cfg.need_embeddings;
        params.n_rs_seq = cfg.rs_seq_window;
        params
    };

    // First fit only the work that every token must execute. The MTP pass below
    // is allowed to reserve its own context only after this plan fits intact.
    let mut primary_tensor_split = vec![0.0_f32; n_devices];
    let mut primary_tensor_buft_overrides = vec![
        sys::llama_model_tensor_buft_override {
            pattern: ptr::null(),
            buft: ptr::null_mut(),
        };
        n_overrides
    ];
    let mut primary_mparams = make_model_params(false);
    let mut primary_cparams = make_context_params();
    let primary_status = unsafe {
        sys::llama_rs_fit_params(
            path.as_ptr(),
            &mut primary_mparams,
            &mut primary_cparams,
            primary_tensor_split.as_mut_ptr(),
            primary_tensor_buft_overrides.as_mut_ptr(),
            margins.as_mut_ptr(),
            FIT_MIN_CONTEXT,
            sys::GGML_LOG_LEVEL_WARN,
        )
    };

    if primary_status != 0 {
        warn!(
            "llama.cpp fit não encontrou uma configuração automática (status {primary_status}); usando os parâmetros solicitados sem MTP na GPU"
        );
        return Ok(empty_fit_plan_with_mtp_mode_and_reason(
            cfg,
            mtp_mode_without_gpu_fit(mtp_mode),
            mtp_unavailable_reason,
        ));
    }

    let primary_gpu_layers = primary_mparams.n_gpu_layers;
    let mut selected_mtp_mode = choose_mtp_fit_mode(mtp_mode, primary_gpu_layers, None);
    let mut selected_n_gpu_layers = primary_mparams.n_gpu_layers;
    let mut selected_n_ctx = primary_cparams.n_ctx;
    let mut selected_tensor_split = primary_tensor_split;
    let mut selected_tensor_buft_overrides = primary_tensor_buft_overrides;

    if mtp_mode == MtpFitMode::Vram && primary_gpu_layers == -1 {
        let mut mtp_mparams = make_model_params(true);
        let mtp_cparams = make_context_params();
        let mut joint_margins = match crate::gguf::read_info(std::path::Path::new(model_path))
            .ok()
            .and_then(|info| estimate_mtp_vram_costs(&info, cfg, n_devices))
        {
            Some(mtp_cost) => {
                tracing::info!(
                    mtp_vram_cost_estimate = ?mtp_cost,
                    "llama.cpp fit reservou o KV estimado do contexto MTP por dispositivo"
                );
                fit_memory_margins_with_mtp(&margins, &mtp_cost, MtpFitMode::Vram)
            }
            None => {
                warn!(
                    "não foi possível estimar o KV do contexto MTP; o grafo não será sondado e o recuo para RAM ficará disponível se a GPU falhar"
                );
                margins.clone()
            }
        };
        let mut mtp_tensor_split = vec![0.0_f32; n_devices];
        let mut mtp_tensor_buft_overrides = vec![
            sys::llama_model_tensor_buft_override {
                pattern: ptr::null(),
                buft: ptr::null_mut(),
            };
            n_overrides
        ];
        let mut mtp_cparams = mtp_cparams;
        let mtp_status = unsafe {
            sys::llama_rs_fit_params(
                path.as_ptr(),
                &mut mtp_mparams,
                &mut mtp_cparams,
                mtp_tensor_split.as_mut_ptr(),
                mtp_tensor_buft_overrides.as_mut_ptr(),
                joint_margins.as_mut_ptr(),
                FIT_MIN_CONTEXT,
                sys::GGML_LOG_LEVEL_WARN,
            )
        };
        let joint_gpu_layers = (mtp_status == 0).then_some(mtp_mparams.n_gpu_layers);
        selected_mtp_mode = choose_mtp_fit_mode(mtp_mode, primary_gpu_layers, joint_gpu_layers);
        if selected_mtp_mode == MtpFitMode::Vram {
            selected_n_gpu_layers = mtp_mparams.n_gpu_layers;
            selected_n_ctx = mtp_cparams.n_ctx;
            selected_tensor_split = mtp_tensor_split;
            selected_tensor_buft_overrides = mtp_tensor_buft_overrides;
        } else {
            tracing::info!(
                primary_gpu_layers,
                joint_gpu_layers = ?joint_gpu_layers,
                "llama.cpp fit recusou a reserva MTP para preservar as camadas principais"
            );
        }
    } else if mtp_mode == MtpFitMode::Vram {
        tracing::info!(
            primary_gpu_layers,
            "llama.cpp fit manteve o MTP fora da VRAM porque o encaixe principal já precisou reduzir camadas"
        );
    }

    let mut effective = cfg.clone();
    effective.n_gpu_layers = selected_n_gpu_layers;
    effective.n_ctx = selected_n_ctx;

    tracing::info!(
        requested_gpu_layers = cfg.n_gpu_layers,
        effective_gpu_layers = effective.n_gpu_layers,
        requested_context = cfg.n_ctx,
        effective_context = effective.n_ctx,
        mtp_fit_mode = ?selected_mtp_mode,
        "llama.cpp fit ajustou os parâmetros de memória"
    );

    Ok(FitPlan {
        config: effective,
        tensor_split: selected_tensor_split,
        tensor_buft_overrides: selected_tensor_buft_overrides,
        devices: ordered_devices,
        mtp_mode: selected_mtp_mode,
        mtp_unavailable_reason,
        applied: true,
    })
}

impl LoadedModel {
    pub(crate) fn model_ptr(&self) -> *mut sys::llama_model {
        self.model
    }

    pub(crate) fn has_nextn_weights(&self) -> bool {
        self.mtp_weights_loaded
    }

    pub(crate) fn model_architecture(&self) -> Option<String> {
        model_metadata_string(self.model, "general.architecture")
    }

    /// Devolve os pesos à GPU agora, em vez de esperar o `Drop`.
    ///
    /// Idempotente: zera o ponteiro, então o `Drop` que vier depois não tem o
    /// que liberar. Só `LlmEngine::unload` chama, e só quando é o último dono.
    fn free_now(&mut self) {
        unsafe {
            if !self.model.is_null() {
                sys::llama_model_free(self.model);
                self.model = std::ptr::null_mut();
                self.vocab = std::ptr::null();
            }
        }
    }
}

impl Drop for LoadedModel {
    fn drop(&mut self) {
        self.free_now();
    }
}

/// Os parâmetros de contexto que **não** têm getter no `llama.h`.
///
/// Tudo o mais (`n_ctx`, `n_batch`, `n_ubatch`, `n_seq_max`, threads, Flash
/// Attention) é lido do contexto vivo, porque o contexto é a verdade: o
/// caminho de fallback de KV em RAM reconstrói o contexto com outra
/// configuração, e ecoar o `EngineConfig` mostraria parâmetros que este nó não
/// está usando.
#[derive(Debug, Clone, Copy)]
struct ContextParamsSnapshot {
    kv_unified: bool,
    embeddings: bool,
    n_rs_seq: u32,
}

pub struct LlmEngine {
    /// Os pesos. `Arc` porque o último contexto a morrer é quem os libera.
    loaded: Arc<LoadedModel>,
    ctx: *mut sys::llama_context,
    pub n_embd: usize,
    pub n_embd_inp: usize,
    pub n_embd_out: usize,
    pub n_vocab: usize,
    /// Whether the residual capture callback was successfully installed.
    capture_ok: bool,
    /// Alvo e buffer da captura deste contexto. `Box` porque o endereço é
    /// registrado no llama.cpp em `cb_eval_user_data` e não pode mudar.
    capture: Box<Mutex<CaptureSlot>>,
    offload_kqv: bool,
    cache_type_k: KvCacheType,
    cache_type_v: KvCacheType,
    fit_memory_applied: bool,
    effective_n_gpu_layers: i32,
    effective_n_ubatch: u32,
    /// O que foi pedido ao `llama_new_context_with_model` e não pode ser
    /// consultado de volta. Ver `ContextParamsSnapshot`.
    cparams_snapshot: ContextParamsSnapshot,
    /// Whether llama.cpp loaded this model in metadata-only external-MoE mode.
    /// This is part of the engine identity because a dense/native context and
    /// an external-executor context cannot share the same loaded model.
    moe_external_executor: bool,
    /// A cabeça de rascunho `nextn`, quando o modelo traz uma e o operador
    /// ligou o MTP. `None` é o caso comum: modelo sem MTP, fatia não-final de
    /// um modelo com MTP, ou interruptor desligado.
    mtp: Option<crate::mtp::MtpDrafter>,
    /// Motivo operacional pelo qual o MTP pedido não está disponível.
    mtp_unavailable_reason: Option<String>,
    /// Falha de criação inicial do contexto MTP, distinta de modelo sem
    /// `nextn`, para acionar exatamente uma tentativa de recuo para RAM.
    mtp_initial_context_error: Option<String>,
    /// Onde o contexto MTP pronto ficou, medido nos buffers reais.
    mtp_allocation: Option<MtpAllocation>,
    moe_callback: Option<Arc<MoeCallbackState>>,
}

/// Pesos mantidos entre gerações do servidor para que uma reconfiguração de
/// contexto não faça outra leitura do GGUF nem outra carga de VRAM.
#[derive(Default)]
pub(crate) struct ModelCache {
    entry: Mutex<Option<CachedModel>>,
}

#[derive(Clone)]
struct CachedModel {
    model_path: String,
    loaded: Arc<LoadedModel>,
    fit_memory_applied: bool,
    effective_n_gpu_layers: i32,
    /// Whether the cached target actually carries `nextn` weights. This is
    /// distinct from the operator request: a RAM fallback keeps the target
    /// model MTP-free.
    mtp_weights_loaded: bool,
    /// A CPU companion is held by the engine, not by this cache entry. Do not
    /// reuse this target model after that fallback or the next reconstruction
    /// would need a companion that the cache does not own.
    mtp_fallback_used: bool,
    moe_external_executor: bool,
    moe_external_executor_layers: Vec<u8>,
}

impl ModelCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn get(
        &self,
        model_path: &str,
        mtp_weights_loaded: bool,
        mtp_fallback_used: bool,
        moe_external_executor: bool,
        moe_external_executor_layers: &[u8],
    ) -> Option<CachedModel> {
        let entry = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        entry
            .as_ref()
            .filter(|cached| {
                cached.model_path == model_path
                    && cached.mtp_weights_loaded == mtp_weights_loaded
                    && cached.mtp_fallback_used == mtp_fallback_used
                    && cached.moe_external_executor == moe_external_executor
                    && cached.moe_external_executor_layers == moe_external_executor_layers
            })
            .cloned()
    }

    fn put(
        &self,
        model_path: &str,
        loaded: Arc<LoadedModel>,
        fit_memory_applied: bool,
        effective_n_gpu_layers: i32,
        mtp_weights_loaded: bool,
        mtp_fallback_used: bool,
        moe_external_executor: bool,
        moe_external_executor_layers: Vec<u8>,
    ) {
        let mut entry = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        *entry = Some(CachedModel {
            model_path: model_path.to_string(),
            loaded,
            fit_memory_applied,
            effective_n_gpu_layers,
            mtp_weights_loaded,
            mtp_fallback_used,
            moe_external_executor,
            moe_external_executor_layers,
        });
    }

    pub(crate) fn clear(&self) {
        let mut entry = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        *entry = None;
    }

    /// A cached target without the requested MTP mode is not reusable on its
    /// own: the CPU-only companion belongs to the engine that just ended, and
    /// a target without nextn cannot satisfy a new MTP request. Drop that
    /// target before the next attempt, or the old weights would remain alive
    /// while the new copy is loaded.
    fn clear_mtp_fallback(&self) -> bool {
        let mut entry = self.entry.lock().unwrap_or_else(|e| e.into_inner());
        let should_clear = entry
            .as_ref()
            .is_some_and(|cached| !cached.mtp_weights_loaded || cached.mtp_fallback_used);
        if should_clear {
            *entry = None;
        }
        should_clear
    }
}

/// A diagnostic boundary around `llama_decode`. `backend_completion_us` is
/// measured with the public `llama_synchronize` API and therefore includes all
/// queued backend work and output transfers that must complete. It is not
/// labelled as pure kernel time.
#[derive(Debug, Clone, Default)]
pub struct DecodeTimings {
    pub enqueue_us: u64,
    pub backend_completion_us: u64,
    pub residual_capture_d2h_sync_us: u64,
    pub moe_forward_us: u64,
    pub moe_executors: Vec<crate::moe::runtime::MoeExecutorKind>,
    pub moe_executor: Option<crate::moe::runtime::MoeExecutorTelemetry>,
}

/// **Ligado por padrão** desde 2026-08-13; `POM_PROFILE_SYNC=0` desliga.
///
/// O `llama_synchronize` é uma barreira de medição, e a suspeita de que ela
/// custasse tempo foi o motivo de nascer desligada. Medido em
/// `logits_access_versus_backend_completion` (fatia final de 16 camadas, Metal,
/// 16 tokens por rodada, duas rodadas intercaladas de cada modo):
///
/// | | total por token | conclusão | acesso aos logits |
/// |---|---|---|---|
/// | desligado | 33,28 / 34,27 ms | 0 | 32,42 / 33,49 ms |
/// | ligado | 33,06 / 32,27 ms | 31,55 / 30,62 ms | 0 |
///
/// O total não muda (o modo ligado ficou até marginalmente mais rápido, dentro
/// do ruído): a barreira **não cria espera, só a nomeia**. Sem ela a espera
/// existia igual, cobrada no primeiro toque de CPU nos logits, e a telemetria
/// dizia que o gargalo era "acesso aos logits" quando era a GPU terminando.
/// Foi essa conta errada que a Mudança 6 do spec de 2026-08-12 pedia para
/// fechar.
fn profile_backend_sync_value(value: Option<&str>) -> bool {
    value
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

pub fn profile_backend_sync_enabled() -> bool {
    profile_backend_sync_value(std::env::var("POM_PROFILE_SYNC").ok().as_deref())
}

// SAFETY: We serialise all GPU calls behind a `Mutex<LlmEngine>` in the HTTP
// server, so these are never accessed concurrently from multiple threads.
unsafe impl Send for LlmEngine {}
unsafe impl Sync for LlmEngine {}

impl LlmEngine {
    /// Devolve **agora** a VRAM deste motor: contexto, rascunho MTP e, se este
    /// for o último contexto sobre os pesos, os pesos.
    ///
    /// Existe porque esperar o `Drop` amarrava a liberação da memória ao tempo
    /// de vida das conexões: o `Router` do servidor de inferência carrega o
    /// `Arc<Mutex<InferState>>`, e cada conexão viva segura uma cópia dele. Uma
    /// conexão em keep-alive parada — o pool do gateway deixa várias — mantinha
    /// a contagem acima de zero para sempre, e o `undeploy` voltava sem liberar
    /// nada: o modelo seguinte não cabia. Quem chama garante que ninguém mais
    /// vai usar o motor (ver o encerramento em `infer_server::run`).
    ///
    /// Idempotente; depois dela o motor não decodifica mais nada.
    pub fn unload(&mut self) {
        // Primeiro o rascunho: o contexto dele também é VRAM.
        self.mtp = None;
        self.mtp_allocation = None;
        unsafe {
            if !self.ctx.is_null() {
                sys::llama_set_moe_ffn_callback(self.ctx, None, ptr::null_mut());
                sys::llama_free(self.ctx);
                self.ctx = std::ptr::null_mut();
            }
        }
        self.moe_callback = None;
        // Os pesos só saem se este era o último contexto sobre eles; com um
        // irmão vivo, `get_mut` devolve `None` e o `Drop` do último resolve.
        if let Some(loaded) = Arc::get_mut(&mut self.loaded) {
            loaded.free_now();
        }
    }

    pub fn set_moe_runtime(&mut self, runtime: Arc<crate::moe::runtime::MoeRuntime>) -> Result<()> {
        if self.moe_callback.is_some() {
            bail!("hook MoE já está instalado neste contexto");
        }
        let state = Arc::new(MoeCallbackState {
            runtime,
            telemetry: Mutex::new(MoeCallbackTelemetry::default()),
            error: Mutex::new(None),
            route_diagnostic_emitted: AtomicBool::new(false),
        });
        unsafe {
            sys::llama_set_moe_ffn_callback(
                self.ctx,
                Some(moe_ffn_callback),
                Arc::as_ptr(&state) as *mut c_void,
            );
        }
        self.moe_callback = Some(state);
        Ok(())
    }

    pub fn moe_hook_enabled(&self) -> bool {
        self.moe_callback.is_some()
    }

    pub fn moe_external_executor_enabled(&self) -> bool {
        self.moe_external_executor
    }

    fn begin_moe_callback(&self) {
        if let Some(state) = self.moe_callback.as_ref() {
            state.begin();
        }
    }

    fn ensure_moe_executor_ready(&self) -> Result<()> {
        if self.moe_external_executor && self.moe_callback.is_none() {
            bail!(
                "executor externo de MoE opt-in sem callback; recusa para evitar fallback nativo silencioso"
            );
        }
        Ok(())
    }

    fn finish_moe_callback(&self, timings: &mut DecodeTimings) -> Result<()> {
        let Some(state) = self.moe_callback.as_ref() else {
            return Ok(());
        };
        let telemetry = state.take()?;
        if telemetry.forward_us == 0 {
            bail!("hook MoE instalado, mas nenhum bloco MoE foi executado no llama.cpp; recusa para evitar fallback nativo silencioso");
        }
        timings.moe_forward_us = telemetry.forward_us;
        timings.moe_executors = telemetry.executors;
        timings.moe_executor = Some(state.runtime.executor_telemetry());
        Ok(())
    }
}

impl Drop for LlmEngine {
    fn drop(&mut self) {
        // Só o contexto: os pesos são de `LoadedModel`, liberados quando o
        // último contexto que os usa sai de cena. Liberar o modelo aqui, com um
        // irmão ainda vivo, seria use-after-free.
        unsafe {
            if !self.ctx.is_null() {
                sys::llama_set_moe_ffn_callback(self.ctx, None, ptr::null_mut());
                sys::llama_free(self.ctx);
            }
        }
        self.moe_callback = None;
    }
}

impl LoadedModel {
    /// Lê os pesos do disco. Uma vez por arquivo, não por contexto.
    fn load(model_path: &str, plan: &FitPlan) -> Result<Arc<Self>> {
        Self::load_with_mtp(model_path, plan, plan.mtp_mode == MtpFitMode::Vram)
    }

    /// Lê os pesos com a opção MTP explícita. O recuo para RAM usa isso para
    /// retirar os pesos `nextn` que a primeira tentativa deixou na VRAM do
    /// modelo alvo, já que o rascunho passa a usar o modelo CPU-only irmão.
    fn load_with_mtp(model_path: &str, plan: &FitPlan, load_mtp: bool) -> Result<Arc<Self>> {
        log_moe_route_diagnostics_configuration();
        // llama_backend_init is idempotent; call it on every load.
        unsafe { sys::llama_backend_init() };

        let path = CString::new(model_path)
            .with_context(|| format!("invalid model path: {model_path}"))?;

        let mut mparams = unsafe { sys::llama_model_default_params() };
        mparams.n_gpu_layers = plan.config.n_gpu_layers;
        if !plan.devices.is_empty() {
            mparams.devices = plan.devices.as_ptr() as *mut sys::ggml_backend_dev_t;
        }
        mparams.load_mtp = load_mtp;
        mparams.moe_external_executor = plan.config.moe_external_executor;
        if !plan.config.moe_external_executor_layers.is_empty() {
            mparams.moe_external_executor_layers =
                plan.config.moe_external_executor_layers.as_ptr();
            mparams.moe_external_executor_layer_count =
                plan.config.moe_external_executor_layers.len();
        }
        if !plan.tensor_split.is_empty() {
            mparams.tensor_split = plan.tensor_split.as_ptr();
        }
        if !plan.tensor_buft_overrides.is_empty() {
            mparams.tensor_buft_overrides = plan.tensor_buft_overrides.as_ptr();
        }

        let model = unsafe { sys::llama_model_load_from_file(path.as_ptr(), mparams) };
        if model.is_null() {
            bail!("llama_model_load_from_file failed for {model_path}");
        }

        let vocab = unsafe { sys::llama_model_get_vocab(model) };
        let n_embd = unsafe { sys::llama_n_embd(model) } as usize;
        let n_vocab = unsafe { sys::llama_n_vocab(vocab) } as usize;
        let mtp_weights_loaded = load_mtp
            && crate::gguf::has_nextn_tensors(std::path::Path::new(model_path)).unwrap_or(false);

        Ok(Arc::new(LoadedModel {
            model,
            vocab,
            n_embd,
            n_vocab,
            mtp_weights_loaded,
        }))
    }

    /// Carrega um segundo modelo sem pesos na GPU para o contexto MTP de
    /// recuo. Um override isolado dos tensores `nextn` não basta: o scheduler
    /// continua vendo o dispositivo da camada e pode manter o compute na GPU,
    /// além de alguns modelos usarem o output head compartilhado. O modelo
    /// CPU-only deixa todo o grafo MTP, inclusive esse head, no host sem
    /// alterar a colocação do contexto alvo.
    fn load_mtp_cpu(model_path: &str, cfg: &EngineConfig) -> Result<Arc<Self>> {
        let mut cpu_cfg = cfg.clone();
        cpu_cfg.n_gpu_layers = 0;
        // The companion has no external executor attached to it. Loading the
        // native expert weights is necessary because MtpDrafter only runs a
        // context and does not install the target engine's MoE callback.
        cpu_cfg.moe_external_executor = false;
        cpu_cfg.moe_external_executor_layers.clear();
        let plan = empty_fit_plan(&cpu_cfg);
        Self::load_with_mtp(model_path, &plan, true)
    }
}

impl LlmEngine {
    /// Escolhe o modelo que pode respaldar o contexto MTP.
    ///
    /// Um companheiro presente é obrigatório no recuo para RAM: o alvo pode
    /// ter sido carregado deliberadamente sem `nextn`. A seleção devolve
    /// `None` antes de qualquer chamada que reserve o grafo MTP quando nem o
    /// alvo nem o companheiro carregam esses pesos.
    fn mtp_model_for_attach<'a>(
        target: &'a LoadedModel,
        companion: Option<&'a LoadedModel>,
    ) -> Option<&'a LoadedModel> {
        let candidate = companion.unwrap_or(target);
        candidate.has_nextn_weights().then_some(candidate)
    }

    /// Cria um contexto e, quando permitido pelo runtime, reaproveita os
    /// pesos carregados por uma geração anterior.
    pub(crate) fn load_with_cache(
        model_path: &str,
        cfg: &EngineConfig,
        cache: &ModelCache,
        reuse_weights: bool,
    ) -> Result<Self> {
        let mtp_weights_required = cfg.mtp
            && crate::gguf::has_nextn_tensors(std::path::Path::new(model_path)).unwrap_or(true);
        if reuse_weights {
            if let Some(cached) = cache.get(
                model_path,
                mtp_weights_required,
                false,
                cfg.moe_external_executor,
                &cfg.moe_external_executor_layers,
            ) {
                tracing::info!(
                    model = %model_path,
                    "reutilizando pesos carregados; reconstruindo apenas o contexto"
                );
                let cached_engine = Self::from_loaded(
                    cached.loaded,
                    cfg,
                    cached.fit_memory_applied,
                    cached.effective_n_gpu_layers,
                )?;
                if !cfg.mtp || cached_engine.mtp_initial_context_error.is_none() {
                    return Ok(cached_engine);
                }
                warn!(
                    "pesos MTP em cache não conseguiram criar o contexto na VRAM; recarregando para tentar a RAM"
                );
                // The cache still owns an Arc to the failed target model. Drop
                // it before loading another copy or the fresh fit can observe
                // the old model's VRAM as occupied.
                cache.clear();
            }
            if cfg.mtp && cache.clear_mtp_fallback() {
                warn!(
                    "descartando pesos MTP em cache após recuo para RAM; o acompanhante CPU não é reutilizável"
                );
            }
        }

        let vram_plan = fit_plan(
            model_path,
            cfg,
            if cfg.mtp {
                MtpFitMode::Vram
            } else {
                MtpFitMode::Disabled
            },
        )?;
        let vram_loaded = LoadedModel::load(model_path, &vram_plan)?;
        let mut loaded = Some(vram_loaded.clone());
        let mut engine = Self::from_loaded(
            vram_loaded,
            &vram_plan.config,
            vram_plan.applied,
            vram_plan.config.n_gpu_layers,
        )?;
        if let Some(reason) = vram_plan.mtp_unavailable_reason.clone() {
            // The plan deliberately loaded the target without nextn after
            // rejecting its architecture. Preserve that more useful reason
            // instead of exposing the later, generic no-nextn consequence.
            engine.mtp_unavailable_reason = Some(reason);
        }
        let mut mtp_fallback_used = false;

        let mtp_vram_error = engine.mtp_initial_context_error.clone();
        let mtp_requires_ram =
            cfg.mtp && (vram_plan.mtp_mode == MtpFitMode::Ram || mtp_vram_error.is_some());
        if mtp_requires_ram {
            let vram_reason = mtp_vram_error.clone().map_or_else(
                || {
                    "MTP não foi colocado na VRAM para preservar as camadas e o contexto do modelo principal"
                        .to_string()
                },
                |error| format!("falha ao criar o contexto MTP na VRAM: {error}"),
            );
            if cfg.mtp_cpu_fallback_enabled {
                drop(engine);

                let ram_result = (|| -> Result<(Self, Arc<LoadedModel>)> {
                    let mtp_loaded = LoadedModel::load_mtp_cpu(model_path, &vram_plan.config)?;
                    let target_loaded = if vram_plan.mtp_mode == MtpFitMode::Ram {
                        // The primary-only fit already kept nextn out of the
                        // target. Reuse it so the fallback cannot reintroduce
                        // the optional weights in VRAM.
                        loaded
                            .take()
                            .expect("o plano RAM deve manter o alvo carregado")
                    } else {
                        // The target's nextn weights were loaded in VRAM for
                        // the first attempt. Release that model before loading
                        // the target again without MTP weights, otherwise the
                        // RAM fallback would still pay for the failed GPU copy.
                        loaded.take();
                        LoadedModel::load_with_mtp(model_path, &vram_plan, false)?
                    };
                    let ram_engine = Self::from_loaded_with_mtp_offload(
                        target_loaded.clone(),
                        &vram_plan.config,
                        vram_plan.applied,
                        vram_plan.config.n_gpu_layers,
                        Some(mtp_loaded),
                        false,
                    )?;
                    Ok((ram_engine, target_loaded))
                })();

                match ram_result {
                    Ok((ram_engine, target_loaded)) => {
                        engine = ram_engine;
                        loaded = Some(target_loaded);
                        mtp_fallback_used = true;
                        if !engine.mtp_ready() {
                            let ram_error =
                                engine.mtp_initial_context_error.clone().unwrap_or_else(|| {
                                    "o contexto MTP não ficou pronto na RAM".to_string()
                                });
                            engine.mtp_unavailable_reason =
                                Some(format!("{vram_reason}; RAM: {ram_error}"));
                        }
                    }
                    Err(ram_error) => {
                        warn!(
                            "recuo do MTP para RAM falhou: {ram_error:#}; restaurando o carregamento na VRAM"
                        );
                        let target_loaded = match loaded.take() {
                            Some(target_loaded) => target_loaded,
                            None => LoadedModel::load(model_path, &vram_plan)?,
                        };
                        engine = Self::from_loaded(
                            target_loaded.clone(),
                            &vram_plan.config,
                            vram_plan.applied,
                            vram_plan.config.n_gpu_layers,
                        )?;
                        loaded = Some(target_loaded);
                        engine.mtp_unavailable_reason =
                            Some(format!("{vram_reason}; RAM: {ram_error:#}"));
                    }
                }
            } else {
                engine.mtp_unavailable_reason = Some(format!(
                    "{vram_reason}; o recuo para a RAM foi recusado pelo operador"
                ));
            }
        }

        let loaded = loaded.expect("o modelo alvo deve permanecer carregado");
        let mtp_weights_loaded = loaded.has_nextn_weights();
        cache.put(
            model_path,
            loaded,
            engine.fit_memory_applied,
            engine.effective_n_gpu_layers,
            mtp_weights_loaded,
            mtp_fallback_used,
            engine.moe_external_executor,
            cfg.moe_external_executor_layers.clone(),
        );
        Ok(engine)
    }

    fn from_loaded(
        loaded: Arc<LoadedModel>,
        cfg: &EngineConfig,
        fit_memory_applied: bool,
        effective_n_gpu_layers: i32,
    ) -> Result<Self> {
        Self::from_loaded_with_mtp_offload(
            loaded,
            cfg,
            fit_memory_applied,
            effective_n_gpu_layers,
            None,
            cfg.offload_kqv,
        )
    }

    fn from_loaded_with_mtp_offload(
        loaded: Arc<LoadedModel>,
        cfg: &EngineConfig,
        fit_memory_applied: bool,
        effective_n_gpu_layers: i32,
        mtp_loaded: Option<Arc<LoadedModel>>,
        mtp_offload_kqv: bool,
    ) -> Result<Self> {
        match Self::with_model_and_mtp_offload(
            loaded.clone(),
            cfg,
            mtp_loaded.clone(),
            mtp_offload_kqv,
        ) {
            Ok(mut engine) => {
                engine.fit_memory_applied = fit_memory_applied;
                engine.effective_n_gpu_layers = effective_n_gpu_layers;
                engine.effective_n_ubatch = effective_n_ubatch(cfg);
                Ok(engine)
            }
            Err(gpu_kv_error) => {
                let Some(cpu_kv_cfg) = Self::cpu_kv_fallback(cfg) else {
                    return Err(gpu_kv_error);
                };

                warn!(
                    "llama context failed with KV cache on GPU: {gpu_kv_error}; retrying with KV cache in host RAM while keeping {} GPU layers",
                    effective_n_gpu_layers
                );
                let mut engine = Self::with_model_and_mtp_offload(
                    loaded,
                    &cpu_kv_cfg,
                    mtp_loaded,
                    false,
                )
                .with_context(|| {
                    format!(
                        "llama context failed with GPU KV and host-RAM KV fallback; requested {} GPU layers",
                        effective_n_gpu_layers
                    )
                })?;
                engine.fit_memory_applied = fit_memory_applied;
                engine.effective_n_gpu_layers = effective_n_gpu_layers;
                engine.effective_n_ubatch = effective_n_ubatch(&cpu_kv_cfg);
                Ok(engine)
            }
        }
    }

    fn cpu_kv_fallback(cfg: &EngineConfig) -> Option<EngineConfig> {
        cfg.offload_kqv.then(|| EngineConfig {
            offload_kqv: false,
            ..cfg.clone()
        })
    }

    /// N contextos sobre um modelo só, um por sessão simultânea.
    ///
    /// O `EngineConfig.n_ctx` é a capacidade total. Cada contexto recebe a
    /// janela por sessão, `total_n_ctx / n`, e cada sessão mantém o tamanho
    /// informado pelo operador. O que muda é que cada um tem o seu
    /// `llama_context`,
    /// e dois decodes podem correr ao mesmo tempo em vez de se enfileirarem no
    /// mutex - que era 194 ms por token na medição de 2026-08-14.
    ///
    /// Os pesos são lidos do disco uma vez.
    ///
    /// Ainda sem chamador: o servidor de inferência carrega **um** contexto
    /// (`load`). O pool fica aqui, com os testes que sustentam a promessa de
    /// memória, para quando o decode concorrente for ligado.
    #[allow(dead_code)]
    pub fn pool(model_path: &str, cfg: &EngineConfig, n: usize) -> Result<Vec<Self>> {
        let n = n.max(1);
        let plan = fit_plan(
            model_path,
            cfg,
            if cfg.mtp {
                MtpFitMode::Vram
            } else {
                MtpFitMode::Disabled
            },
        )?;
        let loaded = LoadedModel::load(model_path, &plan)?;
        let mtp_unavailable_reason = plan.mtp_unavailable_reason.clone();
        let mtp_loaded =
            if cfg.mtp && plan.mtp_mode == MtpFitMode::Ram && cfg.mtp_cpu_fallback_enabled {
                match LoadedModel::load_mtp_cpu(model_path, &plan.config) {
                    Ok(loaded) => Some(loaded),
                    Err(error) => {
                        warn!("recuo do MTP para RAM falhou no pool: {error:#}");
                        None
                    }
                }
            } else {
                None
            };
        let por_contexto = per_context_config(&plan.config, n);
        (0..n)
            .map(|_| {
                let mut engine = if mtp_loaded.is_some() {
                    Self::with_model_and_mtp_offload(
                        loaded.clone(),
                        &por_contexto,
                        mtp_loaded.clone(),
                        false,
                    )?
                } else {
                    Self::with_model(loaded.clone(), &por_contexto)?
                };
                engine.fit_memory_applied = plan.applied;
                engine.effective_n_gpu_layers = plan.config.n_gpu_layers;
                engine.effective_n_ubatch = effective_n_ubatch(&por_contexto);
                if let Some(reason) = mtp_unavailable_reason.clone() {
                    engine.mtp_unavailable_reason = Some(reason);
                }
                Ok(engine)
            })
            .collect()
    }

    /// Um contexto novo sobre pesos já carregados.
    ///
    /// É por aqui que se monta mais de um contexto sobre o mesmo modelo, que é
    /// o que permite dois decodes ao mesmo tempo neste nó.
    pub fn with_model(loaded: Arc<LoadedModel>, cfg: &EngineConfig) -> Result<Self> {
        Self::with_model_and_mtp_offload(loaded, cfg, None, cfg.offload_kqv)
    }

    fn with_model_and_mtp_offload(
        loaded: Arc<LoadedModel>,
        cfg: &EngineConfig,
        mtp_loaded: Option<Arc<LoadedModel>>,
        mtp_offload_kqv: bool,
    ) -> Result<Self> {
        let model = loaded.model;
        let n_embd = loaded.n_embd;
        let n_embd_inp = usize::try_from(unsafe { sys::llama_model_n_embd_inp(model) })
            .map_err(|_| anyhow::anyhow!("llama_model_n_embd_inp retornou valor inválido"))?;
        if n_embd_inp == 0 {
            bail!("llama_model_n_embd_inp retornou largura vazia");
        }
        let n_embd_out = unsafe { sys::llama_model_n_embd_out(model) } as usize;
        let n_vocab = loaded.n_vocab;

        // ── Context params ───────────────────────────────────────────────────
        let mut cparams = unsafe { sys::llama_context_default_params() };
        cparams.n_ctx = cfg.n_ctx;
        apply_context_batch_params(&mut cparams, cfg);
        cparams.n_seq_max = kv_seq_budget(cfg.max_sessions);
        cparams.kv_unified = true;
        cparams.offload_kqv = cfg.offload_kqv;
        apply_cache_types(&mut cparams, cfg.cache_type_k, cfg.cache_type_v);
        cparams.embeddings = generic_embeddings_compatible(cfg.need_embeddings, n_embd, n_embd_out);
        cparams.n_rs_seq = cfg.rs_seq_window;
        if cfg.n_threads > 0 {
            cparams.n_threads = cfg.n_threads;
            cparams.n_threads_batch = cfg.n_threads;
        }

        // ── Residual capture callback ────────────────────────────────────────
        let capture: Box<Mutex<CaptureSlot>> = Box::new(Mutex::new(CaptureSlot {
            want: None,
            buf: None,
            expected_tokens: 0,
            max_microbatch_tokens: effective_n_ubatch(cfg) as usize,
            n_embd,
            expected_floats: None,
            layout: None,
            error: None,
            last_shape: None,
            last_strides: None,
            last_nbytes: None,
            copy_us: 0,
        }));
        let capture_ok = if cfg.need_embeddings {
            // Target tensor: l_out-<last_layer_index> of this slice.
            let n_layer = unsafe { sys::llama_model_n_layer(model) };
            let tensor_name =
                CString::new(format!("l_out-{}", n_layer - 1)).expect("tensor name is valid ASCII");
            capture.lock().unwrap().want = Some(tensor_name);
            cparams.cb_eval = Some(capture_callback);
            // O endereço do Box, não de uma global: é isto que separa a captura
            // de um contexto da do vizinho quando os dois decodificam juntos.
            cparams.cb_eval_user_data = &*capture as *const Mutex<CaptureSlot> as *mut c_void;
            true
        } else {
            false
        };

        // ── Create context ───────────────────────────────────────────────────
        // Tirado aqui, com os `cparams` que de fato vão para o llama.cpp: é o
        // único ponto em que o pedido e o contexto criado são a mesma coisa.
        let cparams_snapshot = ContextParamsSnapshot {
            kv_unified: cparams.kv_unified,
            embeddings: cparams.embeddings,
            n_rs_seq: cparams.n_rs_seq,
        };
        let ctx = unsafe { sys::llama_new_context_with_model(model, cparams) };
        if ctx.is_null() {
            bail!(
                "llama_new_context_with_model failed (n_seq_max={})",
                cfg.max_sessions
            );
        }

        // Ensure causal attention is on — needed when embeddings mode is active.
        unsafe { sys::llama_set_causal_attn(ctx, true) };

        let actual_n_ctx = unsafe { sys::llama_n_ctx(ctx) };
        // Cache unificado: as sequências dividem as mesmas células, então a
        // janela por sequência é o contexto inteiro - ver `kv_seq_budget`.
        let n_ctx_seq = actual_n_ctx;
        tracing::info!(
            "llama context ready: n_seq_max={} n_ctx={} n_ctx_seq={} capture={}",
            kv_seq_budget(cfg.max_sessions),
            actual_n_ctx,
            n_ctx_seq,
            if capture_ok {
                format!("l_out-{}", unsafe { sys::llama_model_n_layer(model) } - 1)
            } else {
                "disabled".to_string()
            }
        );

        // O contexto de rascunho MTP vem depois do alvo de propósito: ele
        // precisa do `ctx` já criado para ligar a saída do estado oculto nele.
        let (mtp, mtp_unavailable_reason, mtp_initial_context_error) = if cfg.mtp {
            if let Some(model_mtp) = Self::mtp_model_for_attach(&loaded, mtp_loaded.as_deref()) {
                let preflight_reason = if !model_mtp.has_nextn_weights()
                    || crate::mtp::nextn_layers(model_mtp.model_ptr()) <= 0
                {
                    Some(MTP_NO_NEXTN_REASON.to_string())
                } else {
                    crate::mtp::mtp_graph_unavailable_reason(
                        model_mtp.model_architecture().as_deref(),
                    )
                };

                if let Some(reason) = preflight_reason {
                    tracing::info!(reason, "MTP indisponível antes da criação do contexto");
                    (None, Some(reason), None)
                } else {
                    let mtp_model = mtp_loaded.clone();
                    match crate::mtp::MtpDrafter::attach(
                        model_mtp,
                        ctx,
                        cfg,
                        n_embd_out,
                        n_vocab,
                        mtp_model,
                        mtp_offload_kqv,
                    ) {
                        Ok(Some(d)) => (Some(d), None, None),
                        Ok(None) => {
                            tracing::info!(
                                "MTP pedido, mas este modelo não traz bloco nextn \
                                 - seguindo sem MTP"
                            );
                            (None, Some(MTP_NO_NEXTN_REASON.to_string()), None)
                        }
                        // Falhar aqui derrubaria um nó que serve bem sem MTP. A
                        // aceleração é opcional; a inferência não.
                        Err(e) => {
                            let reason = e.to_string();
                            warn!("MTP indisponível neste nó: {reason}");
                            (None, Some(reason.clone()), Some(reason))
                        }
                    }
                }
            } else {
                tracing::info!(
                    "MTP pedido, mas o modelo selecionado não carregou pesos nextn \
                     - seguindo sem MTP"
                );
                (None, Some(MTP_NO_NEXTN_REASON.to_string()), None)
            }
        } else {
            (None, None, None)
        };
        let mtp_allocation = mtp.as_ref().map(|drafter| drafter.allocation());

        Ok(LlmEngine {
            loaded,
            ctx,
            n_embd,
            n_embd_inp,
            n_embd_out,
            n_vocab,
            capture_ok,
            capture,
            offload_kqv: cfg.offload_kqv,
            cache_type_k: cfg.cache_type_k,
            cache_type_v: cfg.cache_type_v,
            fit_memory_applied: false,
            effective_n_gpu_layers: cfg.n_gpu_layers,
            effective_n_ubatch: effective_n_ubatch(cfg),
            cparams_snapshot,
            moe_external_executor: cfg.moe_external_executor,
            mtp,
            mtp_unavailable_reason,
            mtp_initial_context_error,
            mtp_allocation,
            moe_callback: None,
        })
    }

    // ── Predição multi-token (MTP) ────────────────────────────────────────────

    /// Este contexto consegue rascunhar pelo bloco `nextn`?
    ///
    /// É a pergunta que decide se o nó anuncia `speculate/v1-mtp` e se o
    /// gateway pede rascunho MTP. Falsa sempre que o modelo não tem o bloco -
    /// o que inclui **toda fatia não-final** de um pipeline, porque o `nextn`
    /// mora na última camada.
    pub fn mtp_ready(&self) -> bool {
        self.mtp.is_some()
    }

    /// Explica por que o MTP pedido não está anexado, quando o motor sabe o
    /// motivo. `None` significa que o MTP não foi pedido ou está pronto.
    pub fn mtp_unavailable_reason(&self) -> Option<&str> {
        self.mtp_unavailable_reason.as_deref()
    }

    /// Onde os buffers relevantes do contexto MTP pronto ficaram de fato.
    pub fn mtp_allocation(&self) -> Option<&'static str> {
        self.mtp_allocation.map(MtpAllocation::as_str)
    }

    /// Rascunha até `k` tokens com a cabeça `nextn`. Vazio quando não há
    /// cabeça — o chamador então decodifica só o token confirmado, que é o
    /// comportamento de sempre.
    pub fn mtp_draft(
        &mut self,
        id_last: i32,
        n_past: u32,
        seq_id: u32,
        k: u32,
        p_min: f32,
    ) -> Vec<i32> {
        match self.mtp.as_mut() {
            Some(d) => d.draft(id_last, n_past, seq_id, k, p_min),
            None => Vec::new(),
        }
    }

    /// Diz à cabeça quantos tokens **do rascunho** a verificação aceitou.
    pub fn mtp_accept(&mut self, accepted: usize) {
        if let Some(d) = self.mtp.as_mut() {
            d.accept(accepted);
        }
    }

    pub fn n_ctx(&self) -> u32 {
        unsafe { sys::llama_n_ctx(self.ctx) }
    }

    pub fn offload_kqv(&self) -> bool {
        // The setting is immutable for this context and carried in the engine
        // configuration only during construction. Keep a copy visible through
        // `/health` rather than guessing from GPU utilisation.
        self.offload_kqv
    }

    pub fn cache_type_k(&self) -> KvCacheType {
        self.cache_type_k
    }

    pub fn cache_type_v(&self) -> KvCacheType {
        self.cache_type_v
    }

    /// Quantos blocos esta fatia carregou.
    pub fn n_layer(&self) -> u32 {
        unsafe { sys::llama_model_n_layer(self.loaded.model) as u32 }
    }

    /// Em qual bloco desta fatia a próxima decodificação deve ser capturada.
    ///
    /// `None` volta ao normal: captura no último bloco. Com `Some(k)` a captura
    /// passa a mirar `l_out-k`. O callback copia o residual e deixa o restante
    /// do grafo continuar; isso é necessário para que cabeças obrigatórias
    /// possam ser executadas sem contaminar o residual exportado.
    ///
    /// Vale só para a próxima decodificação porque quem a chama já segura o
    /// mesmo lock que serializa as decodificações deste contexto.
    pub fn set_capture_layer(&self, block_index: Option<u32>) -> Result<()> {
        if !self.capture_ok {
            bail!("esta fatia não exporta residual: não há captura para reposicionar");
        }
        let last = self.n_layer().saturating_sub(1);
        let index = block_index.unwrap_or(last);
        if index > last {
            bail!("bloco {index} não existe nesta fatia (último é {last})");
        }
        let name = CString::new(format!("l_out-{index}"))
            .map_err(|e| anyhow::anyhow!("nome de tensor inválido: {e}"))?;
        self.capture.lock().unwrap().want = Some(name);
        Ok(())
    }

    pub fn fit_memory_applied(&self) -> bool {
        self.fit_memory_applied
    }

    pub fn effective_n_gpu_layers(&self) -> i32 {
        self.effective_n_gpu_layers
    }

    pub fn effective_n_ubatch(&self) -> u32 {
        self.effective_n_ubatch
    }

    /// Onde cada camada realmente ficou, do próprio llama.cpp.
    ///
    /// `None` quando a ponte falha, e aí o `/health` omite o campo em vez de
    /// mostrar um mapa inventado: dizer "não sei" é a única resposta honesta
    /// possível, porque nada em `EngineConfig` substitui o buffer real de um
    /// tensor. Ver `llama_rs_model_placement_json` no `wrapper_common.cpp`.
    pub fn placement(&self) -> Option<serde_json::Value> {
        unsafe {
            json_from_wrapper(|out| sys::llama_rs_model_placement_json(self.loaded.model, out))
        }
    }

    /// O Flash Attention resolvido deste contexto, ou `None` se a ponte falhar.
    ///
    /// O par é `(ligado, decidido_pela_sonda)`. O padrão do llama.cpp é
    /// `AUTO`, então o pedido não responde a pergunta: quem responde é
    /// `get_cparams()` depois que o contexto subiu. Ver
    /// `llama_rs_context_flash_attn` no `wrapper_common.cpp`.
    pub fn flash_attn(&self) -> Option<(bool, bool)> {
        let mut enabled = false;
        let mut auto = false;
        let status = unsafe { sys::llama_rs_context_flash_attn(self.ctx, &mut enabled, &mut auto) };
        (status == sys::LLAMA_RS_STATUS_OK).then_some((enabled, auto))
    }

    /// Com que parâmetros de llama.cpp este contexto está de pé.
    ///
    /// Serve ao operador que quer comparar o nó com uma linha de comando do
    /// `llama-cli`. Os valores vêm do contexto vivo sempre que existe getter,
    /// e não do `EngineConfig` - ver `ContextParamsSnapshot` para o porquê.
    pub fn load_params(&self) -> serde_json::Value {
        let flash = self.flash_attn();
        serde_json::json!({
            "n_ctx": self.n_ctx(),
            "n_ctx_seq": unsafe { sys::llama_n_ctx_seq(self.ctx) },
            "n_batch": unsafe { sys::llama_n_batch(self.ctx) },
            "n_ubatch": unsafe { sys::llama_n_ubatch(self.ctx) },
            "n_seq_max": unsafe { sys::llama_n_seq_max(self.ctx) },
            "n_threads": unsafe { sys::llama_n_threads(self.ctx) },
            "n_threads_batch": unsafe { sys::llama_n_threads_batch(self.ctx) },
            // `null` quando a ponte de diagnóstico não respondeu: a interface
            // diz que não sabe, em vez de repetir o pedido como se fosse a
            // decisão.
            "flash_attn": flash.map(|(enabled, _)| enabled),
            "flash_attn_auto": flash.map(|(_, auto)| auto),
            "kv_unified": self.cparams_snapshot.kv_unified,
            "embeddings": self.cparams_snapshot.embeddings,
            "n_rs_seq": self.cparams_snapshot.n_rs_seq,
            "offload_kqv": self.offload_kqv,
            "cache_type_k": self.cache_type_k.as_str(),
            "cache_type_v": self.cache_type_v.as_str(),
            "n_gpu_layers": self.effective_n_gpu_layers,
            "fit_memory_applied": self.fit_memory_applied,
        })
    }

    /// A memória deste contexto por buffer: pesos, cache KV e cálculo.
    ///
    /// É o que responde onde o cache KV ficou de verdade. `offload_kqv()` é o
    /// que foi pedido; numa GPU cheia o llama.cpp recarrega com ele desligado
    /// (ver `runtime.rs`), e só a repartição por buffer mostra isso.
    pub fn context_memory(&self) -> Option<serde_json::Value> {
        unsafe { json_from_wrapper(|out| sys::llama_rs_context_memory_json(self.ctx, out)) }
    }

    /// Compute buffer CUDA medido pelo próprio llama.cpp para este contexto.
    ///
    /// O valor alimenta o orçamento do cache residente de experts. Se a ponte
    /// de memória não reportar um buffer CUDA completo, devolve `None` para que
    /// o caminho CUDA seja recusado nominalmente.
    pub fn cuda_compute_bytes(&self) -> Option<u64> {
        crate::moe::admission::cuda_compute_bytes_from_llama_memory(&self.context_memory()?)
    }

    pub fn eos_token(&self) -> i32 {
        unsafe { sys::llama_vocab_eos(self.loaded.vocab) }
    }

    /// Returns true for the model EOS or for a complete turn terminator named
    /// by the GGUF chat template. A Gemma4 turn is not necessarily the model
    /// EOS, so checking only `eos_token()` lets the next turn leak into the
    /// response.
    pub fn is_generation_stop(&self, recent: &[i32], next: i32) -> bool {
        if next == self.eos_token() {
            return true;
        }
        let Some(template) = self.chat_template() else {
            return false;
        };
        if !template.contains("<turn|>") {
            return false;
        }
        let turn_end = self.tokenize("<turn|>", false, true);
        if turn_end.is_empty() {
            return false;
        }
        let mut candidate = Vec::with_capacity(recent.len() + 1);
        candidate.extend_from_slice(recent);
        candidate.push(next);
        token_sequence_ends_with(&candidate, &turn_end)
    }

    // ── Tokenization ─────────────────────────────────────────────────────────

    pub fn tokenize(&self, text: &str, add_special: bool, parse_special: bool) -> Vec<i32> {
        let c_text = match CString::new(text) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        // Sizing call. With `n_tokens_max = 0` llama.cpp cannot write anything,
        // so it reports the space it *would* need as a NEGATIVE number - the
        // documented contract in llama.h ("returns a negative number on failure
        // - the number of tokens that would have been returned"). Reading that
        // as an error made every prompt tokenize to nothing, and the head peer
        // rejected chunk 0 with "não tem tokens" before any hidden state ever
        // reached the second peer.
        let n = unsafe {
            sys::llama_tokenize(
                self.loaded.vocab,
                c_text.as_ptr(),
                text.len() as i32,
                ptr::null_mut(),
                0,
                add_special,
                parse_special,
            )
        };
        // INT32_MIN means the count overflowed; anything else negative is the
        // capacity we have to allocate.
        if n == 0 || n == i32::MIN {
            return vec![];
        }
        let cap = n.unsigned_abs() as usize;
        let mut tokens = vec![0i32; cap];
        let written = unsafe {
            sys::llama_tokenize(
                self.loaded.vocab,
                c_text.as_ptr(),
                text.len() as i32,
                tokens.as_mut_ptr(),
                cap as i32,
                add_special,
                parse_special,
            )
        };
        if written <= 0 {
            return vec![];
        }
        tokens.truncate(written as usize);
        tokens
    }

    /// Devolve os bytes crus da peça do vocabulário.
    ///
    /// Uma peça pode terminar no meio de um codepoint UTF-8. A conversão para
    /// `String` fica deliberadamente fora deste limite: o caminho de streaming
    /// precisa alimentar um `Utf8Assembler` que vive na sessão da requisição.
    pub fn token_to_piece_bytes(&self, token: i32, special: bool) -> Vec<u8> {
        let mut buf = vec![0u8; 256];
        let n = unsafe {
            sys::llama_token_to_piece(
                self.loaded.vocab,
                token,
                buf.as_mut_ptr() as *mut i8,
                buf.len() as i32,
                0,
                special,
            )
        };
        if n <= 0 {
            return Vec::new();
        }
        buf.truncate(n as usize);
        buf
    }

    // ── Chat template ─────────────────────────────────────────────────────────

    /// The `tokenizer.chat_template` from the model's GGUF metadata, or None.
    pub fn chat_template(&self) -> Option<String> {
        self.model_metadata_string("tokenizer.chat_template")
    }

    /// A arquitetura declarada no GGUF, sem inferência pelo nome do arquivo.
    pub fn model_architecture(&self) -> Option<String> {
        self.model_metadata_string("general.architecture")
    }

    fn model_metadata_string(&self, key: &str) -> Option<String> {
        model_metadata_string(self.loaded.model, key)
    }

    /// Apply the model's built-in Jinja2 chat template.
    ///
    /// Gemma4 receives an explicit renderer because its `enable_thinking`
    /// variable cannot be passed through the current llama.cpp C API. Other
    /// templates must succeed in llama.cpp; a failure is returned rather than
    /// silently changing the model's control tokens.
    pub fn apply_chat_template(
        &self,
        messages: &[(&str, &str)],
        add_generation_prompt: bool,
        no_thinking: bool,
    ) -> Result<String> {
        let sys_msg = "You are a helpful assistant.";
        let full_messages: Vec<(&str, &str)> = std::iter::once(("system", sys_msg))
            .chain(messages.iter().copied())
            .collect();

        let tmpl = self.chat_template();
        let architecture = self.model_architecture();
        if architecture
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("gemma4"))
        {
            let template = tmpl.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Gemma4 exige tokenizer.chat_template no GGUF; renderer não inventado"
                )
            })?;
            let bos = unsafe { sys::llama_vocab_bos(self.loaded.vocab) };
            let bos = String::from_utf8_lossy(&self.token_to_piece_bytes(bos, true)).into_owned();
            if bos.is_empty() {
                bail!("Gemma4 exige token BOS no vocabulário GGUF");
            }
            return render_gemma4_template(
                template,
                &bos,
                messages,
                add_generation_prompt,
                no_thinking,
            );
        }

        // Build the llama_chat_message array.
        let c_msgs: Vec<(CString, CString)> = full_messages
            .iter()
            .filter_map(|(role, content)| {
                Some((CString::new(*role).ok()?, CString::new(*content).ok()?))
            })
            .collect();
        let raw_msgs: Vec<sys::llama_chat_message> = c_msgs
            .iter()
            .map(|(role, content)| sys::llama_chat_message {
                role: role.as_ptr(),
                content: content.as_ptr(),
            })
            .collect();

        let tmpl_ptr = tmpl
            .as_ref()
            .and_then(|s| CString::new(s.as_str()).ok())
            .map(|c| c.into_raw() as *const i8)
            .unwrap_or(ptr::null());

        // `llama_chat_apply_template` devolve **sempre** o tamanho completo do
        // texto renderizado e copia no máximo `length` bytes (`strncpy` em
        // `llama.cpp`). Um buffer fixo, portanto, não é um limite: é um corte
        // silencioso pelo fim. Com 65536 bytes fixos, dois prompts distintos de
        // ~126 KB rendiam exatamente os mesmos 12941 tokens - a última mensagem
        // do usuário simplesmente não chegava ao modelo, e o prefixo do turno
        // seguinte ficava idêntico ao do anterior. Pedir de novo com o tamanho
        // informado é o contrato: a segunda chamada cabe por construção.
        let mut out_buf = vec![0u8; CHAT_TEMPLATE_INITIAL_BUF];
        let mut n = unsafe {
            sys::llama_chat_apply_template(
                tmpl_ptr,
                raw_msgs.as_ptr(),
                raw_msgs.len(),
                add_generation_prompt,
                out_buf.as_mut_ptr() as *mut i8,
                out_buf.len() as i32,
            )
        };
        if n > 0 && (n as usize) > out_buf.len() {
            out_buf = vec![0u8; n as usize];
            n = unsafe {
                sys::llama_chat_apply_template(
                    tmpl_ptr,
                    raw_msgs.as_ptr(),
                    raw_msgs.len(),
                    add_generation_prompt,
                    out_buf.as_mut_ptr() as *mut i8,
                    out_buf.len() as i32,
                )
            };
        }

        // Reclaim tmpl_ptr if we allocated one.
        if !tmpl_ptr.is_null() {
            unsafe { drop(CString::from_raw(tmpl_ptr as *mut i8)) };
        }

        let mut text = if n > 0 {
            let escrito = (n as usize).min(out_buf.len());
            if escrito < n as usize {
                bail!(
                    "llama_chat_apply_template pediu {n} bytes e o buffer tem {}; \
                     renderizar o prompt cortado entregaria a conversa pela metade",
                    out_buf.len()
                );
            }
            out_buf.truncate(escrito);
            String::from_utf8_lossy(&out_buf).into_owned()
        } else {
            bail!(
                "llama_chat_apply_template falhou (rc={n}) para arquitetura {}; renderer explícito indisponível",
                architecture.as_deref().unwrap_or("desconhecida")
            );
        };

        if no_thinking {
            if let Some(ref tmpl_str) = tmpl {
                if tmpl_str.contains("<think>") {
                    text.push_str("<think>\n\n</think>\n");
                }
            }
        }
        Ok(text)
    }

    // ── Batch decode ─────────────────────────────────────────────────────────

    /// Decodes multiple independent token sequences in one llama batch.
    ///
    /// Each descriptor owns a physical KV sequence. The method deliberately
    /// does not sample or mutate session history: the scheduler does that
    /// after it maps logits back to the logical item.
    pub fn decode_tokens_batch(
        &mut self,
        entries: &[TokenBatchEntry],
        need_residual: bool,
    ) -> Result<(BatchDecodeOutput, DecodeTimings)> {
        self.ensure_moe_executor_ready()?;
        if entries.is_empty() {
            bail!("batch de tokens vazio");
        }
        let descriptors: Vec<BatchEntry> = entries.iter().map(|e| e.descriptor.clone()).collect();
        let layout = BatchLayout::new(&descriptors)?;
        for entry in entries {
            if entry.tokens.len() != entry.descriptor.tokens {
                bail!(
                    "item {} declara {} tokens, recebeu {}",
                    entry.descriptor.item_id,
                    entry.descriptor.tokens,
                    entry.tokens.len()
                );
            }
        }

        let n = i32::try_from(layout.total_tokens())
            .map_err(|_| anyhow::anyhow!("batch excede o limite de tokens do llama.cpp"))?;
        let enqueue_started = Instant::now();
        self.reset_capture_slot(layout.total_tokens());
        let mut batch = unsafe { sys::llama_batch_init(n, 0, 1) };
        batch.n_tokens = n;
        let mut row = 0usize;
        unsafe {
            for entry in entries {
                for (offset, &token) in entry.tokens.iter().enumerate() {
                    *batch.token.add(row) = token;
                    *batch.pos.add(row) = (entry.descriptor.pos + offset as u32) as i32;
                    *batch.n_seq_id.add(row) = 1;
                    *(*batch.seq_id.add(row)).add(0) = entry.descriptor.seq_id as i32;
                    *batch.logits.add(row) =
                        if entry.descriptor.want_logits && offset + 1 == entry.tokens.len() {
                            1
                        } else {
                            0
                        };
                    row += 1;
                }
            }
        }
        self.begin_moe_callback();
        let rc = unsafe { sys::llama_decode(self.ctx, batch) };
        unsafe { sys::llama_batch_free(batch) };
        if rc != 0 {
            bail!("llama_decode (token batch) failed rc={rc}");
        }
        self.finish_batch(layout, need_residual, enqueue_started)
    }

    /// Decodes multiple hidden-state sequences in one llama batch.
    pub fn decode_hidden_batch(
        &mut self,
        entries: &[HiddenBatchEntry],
        need_residual: bool,
    ) -> Result<(BatchDecodeOutput, DecodeTimings)> {
        self.ensure_moe_executor_ready()?;
        if entries.is_empty() {
            bail!("batch de hidden states vazio");
        }
        let descriptors: Vec<BatchEntry> = entries.iter().map(|e| e.descriptor.clone()).collect();
        let layout = BatchLayout::new(&descriptors)?;
        let mut feature_shape: Option<&[usize]> = None;
        let mut input_width = None;
        for entry in entries {
            let entry_width = validate_hidden_tensor(
                entry.hidden.len(),
                &entry.shape,
                entry.descriptor.tokens,
                self.n_embd,
                self.n_embd_inp,
                self.n_embd_out,
                &format!("item {}", entry.descriptor.item_id),
            )?;
            if let Some(expected_width) = input_width {
                if expected_width != entry_width {
                    bail!(
                        "item {} tem largura {}, mas o batch usa {}",
                        entry.descriptor.item_id,
                        entry_width,
                        expected_width
                    );
                }
            } else {
                input_width = Some(entry_width);
            }
            if let Some(expected_shape) = feature_shape {
                if *expected_shape != entry.shape[1..] {
                    bail!(
                        "item {} tem layout {:?}, mas o batch usa {:?}",
                        entry.descriptor.item_id,
                        &entry.shape[1..],
                        expected_shape
                    );
                }
            } else {
                feature_shape = Some(&entry.shape[1..]);
            }
        }

        let n = i32::try_from(layout.total_tokens())
            .map_err(|_| anyhow::anyhow!("batch excede o limite de tokens do llama.cpp"))?;
        let input_width = input_width.expect("entries não vazio garante largura de entrada");
        let input_width_i32 = i32::try_from(input_width)
            .map_err(|_| anyhow::anyhow!("largura de entrada excede o limite do llama.cpp"))?;
        let enqueue_started = Instant::now();
        self.reset_capture_slot(layout.total_tokens());
        let mut batch = unsafe { sys::llama_batch_init(n, input_width_i32, 1) };
        batch.n_tokens = n;
        batch.token = ptr::null_mut();
        let mut row = 0usize;
        unsafe {
            for entry in entries {
                let start = row * input_width;
                let end = start + entry.hidden.len();
                std::ptr::copy_nonoverlapping(
                    entry.hidden.as_ptr(),
                    batch.embd.add(start),
                    end - start,
                );
                for offset in 0..entry.descriptor.tokens {
                    *batch.pos.add(row) = (entry.descriptor.pos + offset as u32) as i32;
                    *batch.n_seq_id.add(row) = 1;
                    *(*batch.seq_id.add(row)).add(0) = entry.descriptor.seq_id as i32;
                    *batch.logits.add(row) =
                        if entry.descriptor.want_logits && offset + 1 == entry.descriptor.tokens {
                            1
                        } else {
                            0
                        };
                    row += 1;
                }
            }
        }
        self.begin_moe_callback();
        let rc = unsafe { sys::llama_decode(self.ctx, batch) };
        unsafe { sys::llama_batch_free(batch) };
        if rc != 0 {
            bail!("llama_decode (hidden batch) failed rc={rc}");
        }
        self.finish_batch(layout, need_residual, enqueue_started)
    }

    fn reset_capture_slot(&self, n_tokens: usize) {
        let mut capture = self.capture.lock().unwrap();
        capture.buf = None;
        capture.expected_tokens = n_tokens;
        capture.expected_floats = None;
        capture.layout = None;
        capture.error = None;
        capture.last_shape = None;
        capture.last_strides = None;
        capture.last_nbytes = None;
        capture.copy_us = 0;
    }

    fn finish_batch(
        &self,
        layout: BatchLayout,
        need_residual: bool,
        enqueue_started: Instant,
    ) -> Result<(BatchDecodeOutput, DecodeTimings)> {
        crate::tp::set_expected_reduces(
            2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64,
        );
        if crate::tp::is_active() {
            let reduces = crate::tp::reduces_in_current_decode();
            let expected = 2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64;
            if reduces != expected {
                bail!("coletiva TP reduziu {reduces} vezes neste batch, esperado {expected}");
            }
            if let Some(motivo) = crate::tp::take_failure() {
                bail!("coletiva de tensor parallelism falhou: {motivo}");
            }
        }
        let residual = if need_residual {
            Some(self.take_residual(layout.total_tokens())?)
        } else {
            None
        };
        let residual_shape = residual
            .as_ref()
            .map(|captured| captured.shape.clone())
            .unwrap_or_else(|| vec![0, self.n_embd]);
        let residual = residual.map(|captured| captured.values);
        let mut timings = DecodeTimings {
            enqueue_us: enqueue_started.elapsed().as_micros() as u64,
            residual_capture_d2h_sync_us: self.capture.lock().unwrap().copy_us,
            ..Default::default()
        };
        if profile_backend_sync_enabled() {
            let completion_started = Instant::now();
            unsafe { sys::llama_synchronize(self.ctx) };
            timings.backend_completion_us = completion_started.elapsed().as_micros() as u64;
        }
        self.finish_moe_callback(&mut timings)?;
        Ok((
            BatchDecodeOutput {
                layout,
                residual,
                residual_shape,
                n_embd: self.n_embd,
            },
            timings,
        ))
    }

    /// Decode a batch of token IDs. `logits_all=true` means every position
    /// gets logits; `false` means only the last one.
    pub fn decode_tokens(
        &mut self,
        tokens: &[i32],
        pos_offset: u32,
        seq_id: u32,
        logits_all: bool,
    ) -> Result<DecodeTimings> {
        self.ensure_moe_executor_ready()?;
        // Zera o buffer, e não só o cronômetro: agora que a captura
        // acumula microlote a microlote, sobra do decode anterior seria
        // residual de outra requisição concatenado ao desta.
        self.reset_capture_slot(tokens.len());
        let enqueue_started = Instant::now();
        let n = tokens.len() as i32;
        let mut batch = unsafe { sys::llama_batch_init(n, 0, 1) };
        batch.n_tokens = n;

        unsafe {
            for (i, &tok) in tokens.iter().enumerate() {
                *batch.token.add(i) = tok;
                *batch.pos.add(i) = (pos_offset as i32) + i as i32;
                *batch.n_seq_id.add(i) = 1;
                *(*batch.seq_id.add(i)).add(0) = seq_id as i32;
                *batch.logits.add(i) = if logits_all || i == tokens.len() - 1 {
                    1
                } else {
                    0
                };
            }
        }

        self.begin_moe_callback();
        let tp_antes = crate::tp::reduce_count();
        self.begin_moe_callback();
        let rc = unsafe { sys::llama_decode(self.ctx, batch) };
        unsafe { sys::llama_batch_free(batch) };
        if rc != 0 {
            bail!("llama_decode failed rc={rc}");
        }
        crate::tp::set_expected_reduces(
            2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64,
        );
        // Coletiva ligada e nenhum reduce no decode inteiro: o op do fork não
        // entrou no grafo. O rank rodou com meia largura e sem soma nenhuma, e
        // a resposta é falsa - o modo de falha de 18/08, que não tinha sintoma
        // além do texto sem sentido.
        // Quantos reduces este decode disparou. São **2 por camada**: a projeção
        // de saída da atenção e o `ffn_down`, as duas row-parallel. Um modelo de
        // 32 camadas tem que somar 64.
        //
        // Isto é rede de proteção, não curiosidade. O fork tinha o reduce em uma
        // única sobrecarga de `build_attn` (a `attn_no_cache`), então todo
        // modelo com cache KV somava só o FFN: 32 em vez de 64. E o sintoma é o
        // disfarce mais perigoso possível - a resposta sai gramatical e fluente,
        // só o conteúdo é vazio. Não parece erro de cálculo, parece modelo ruim,
        // e custou uma tarde inteira de 18/08 para ser identificado.
        //
        // Aviso, não erro: arquitetura híbrida pode legitimamente ter camada sem
        // atenção ou sem FFN, e recusar a resposta nesse caso seria pior que o
        // aviso. Zero reduces continua sendo erro (logo abaixo).
        if crate::tp::is_active() {
            let reduces = crate::tp::reduces_in_current_decode();
            let esperado = 2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64;
            if reduces != esperado {
                tracing::warn!(
                    "coletiva TP reduziu {reduces} vezes neste decode, esperado {esperado} \
                     (2 por camada): parte do grafo está rodando sem redução, e a \
                     resposta sai fluente e sem sentido"
                );
            } else {
                tracing::debug!("decode fechou com {reduces} reduces (2 por camada)");
            }
        }
        if crate::tp::is_active() && crate::tp::reduce_count() == tp_antes {
            bail!(
                "coletiva de tensor parallelism ligada mas nenhum reduce rodou \
                 neste decode: o op do llama.cpp não entrou no grafo"
            );
        }
        // Coletiva de tensor parallelism: o op roda **dentro** do decode, num
        // callback `extern "C"` que não tem como devolver erro. Quem falhou
        // anotou o motivo, e é aqui que ele vira erro da requisição.
        //
        // Isto não é zelo: um all-reduce que falha não piora a resposta, ele a
        // torna falsa. O rank fica com a própria parcial - metade das cabeças e
        // metade do FFN - e a geração sai plausível e sem sentido, sem nenhum
        // sinal de erro (18/08). Responder errado é pior que não responder.
        if let Some(motivo) = crate::tp::take_failure() {
            bail!("coletiva de tensor parallelism falhou: {motivo}");
        }

        // A cabeça `nextn` tem que ver **todo** lote que o alvo decodifica -
        // prompt incluído -, senão o cache dela fica atrás do tronco e o par
        // `(h_p, x_{p+1})` que ela consome deixa de existir. Ficar aqui, e não
        // em cada chamador, é o que impede um caminho novo de decodificação de
        // esquecer o passo. Um erro aqui derruba o rascunho, não a resposta: a
        // cabeça sai de cena e a geração segue um token por rodada.
        if self.mtp.is_some() {
            let ctx = self.ctx;
            let mut drafter = self.mtp.take();
            if let Some(d) = drafter.as_mut() {
                if let Err(e) = d.observe(ctx, tokens, pos_offset, seq_id) {
                    let reason = format!("MTP desligado nesta sessão: {e}");
                    warn!("{reason}");
                    self.mtp_unavailable_reason = Some(reason);
                    self.mtp_allocation = None;
                    drafter = None;
                }
            }
            self.mtp = drafter;
        }

        let mut timings = DecodeTimings {
            enqueue_us: enqueue_started.elapsed().as_micros() as u64,
            residual_capture_d2h_sync_us: self.capture.lock().unwrap().copy_us,
            ..Default::default()
        };
        if profile_backend_sync_enabled() {
            let completion_started = Instant::now();
            unsafe { sys::llama_synchronize(self.ctx) };
            timings.backend_completion_us = completion_started.elapsed().as_micros() as u64;
        }
        self.finish_moe_callback(&mut timings)?;
        Ok(timings)
    }

    /// Decode a batch of embedding vectors (hidden states from the previous peer).
    ///
    /// `tokens` são os ids do lote que a cauda decodificou — o caller que os
    /// tem entrega; sem eles a cabeça `nextn` não se mantém em dia (ver o
    /// bloco de `observe` no corpo).
    pub fn decode_hidden(
        &mut self,
        hidden: &[f32],
        shape: &TensorShape,
        n_tokens: usize,
        pos_offset: u32,
        seq_id: u32,
        logits_all: bool,
        tokens: Option<&[i32]>,
    ) -> Result<DecodeTimings> {
        self.ensure_moe_executor_ready()?;
        // Zera o buffer, e não só o cronômetro: agora que a captura
        // acumula microlote a microlote, sobra do decode anterior seria
        // residual de outra requisição concatenado ao desta.
        self.reset_capture_slot(n_tokens);
        let enqueue_started = Instant::now();
        let input_width = validate_hidden_tensor(
            hidden.len(),
            shape,
            n_tokens,
            self.n_embd,
            self.n_embd_inp,
            self.n_embd_out,
            "hidden state",
        )?;
        let n = i32::try_from(n_tokens)
            .map_err(|_| anyhow::anyhow!("hidden state excede o limite de tokens do llama.cpp"))?;
        let input_width_i32 = i32::try_from(input_width)
            .map_err(|_| anyhow::anyhow!("largura de entrada excede o limite do llama.cpp"))?;
        let mut batch = unsafe { sys::llama_batch_init(n, input_width_i32, 1) };
        batch.n_tokens = n;
        batch.token = ptr::null_mut(); // signals embedding mode

        unsafe {
            // Copy the flat embedding data into the batch's embd buffer.
            let embd_ptr = batch.embd;
            std::ptr::copy_nonoverlapping(hidden.as_ptr(), embd_ptr, hidden.len());

            for i in 0..n_tokens {
                *batch.pos.add(i) = (pos_offset as i32) + i as i32;
                *batch.n_seq_id.add(i) = 1;
                *(*batch.seq_id.add(i)).add(0) = seq_id as i32;
                *batch.logits.add(i) = if logits_all || i == n_tokens - 1 {
                    1
                } else {
                    0
                };
            }
        }

        let rc = unsafe { sys::llama_decode(self.ctx, batch) };
        unsafe { sys::llama_batch_free(batch) };
        if rc != 0 {
            bail!("llama_decode (hidden) failed rc={rc}");
        }
        crate::tp::set_expected_reduces(
            2 * unsafe { sys::llama_model_n_layer(self.loaded.model) } as u64,
        );
        // Ver a nota gêmea em `decode_tokens`: a coletiva falha por callback e
        // só pode virar erro aqui fora.
        if let Some(motivo) = crate::tp::take_failure() {
            bail!("coletiva de tensor parallelism falhou: {motivo}");
        }

        // A cabeça `nextn` tem que ver **todo** lote que o alvo decodifica,
        // senão o cache dela fica atrás do tronco e o par `(h_p, x_{p+1})` que
        // ela consome deixa de existir. Ficar aqui, e não em cada chamador, é
        // o que impede um caminho novo de decodificação de esquecer o passo.
        // Um erro aqui derruba o rascunho, não a resposta: a cabeça sai de
        // cena e a geração segue um token por rodada.
        //
        // Diferente do `decode_tokens`, um lote de estado oculto não carrega os
        // ids dos tokens embutidos — quem os tem é o chamador (a cauda de um
        // pipeline, que conhece a composição do lote). `tokens` traz esses ids;
        // quando ninguém os oferece (peer velho que não encaminha o campo), a
        // cabeça não tem como se manter em dia e desligar com aviso é preferível
        // a rascunhar a partir de um cache que ficou para trás.
        if let Some(tokens) = tokens {
            if self.mtp.is_some() {
                let ctx = self.ctx;
                let mut drafter = self.mtp.take();
                if let Some(d) = drafter.as_mut() {
                    if let Err(e) = d.observe(ctx, tokens, pos_offset, seq_id) {
                        let reason = format!("MTP desligado nesta sessão: {e}");
                        warn!("{reason}");
                        self.mtp_unavailable_reason = Some(reason);
                        self.mtp_allocation = None;
                        drafter = None;
                    }
                }
                self.mtp = drafter;
            }
        } else if self.mtp.is_some() {
            let reason = "MTP desligado: estado oculto recebido sem ids dos tokens".to_string();
            warn!(
                "{reason} (peer velho no pipeline?), e a cabeça nextn precisa deles para acompanhar o tronco"
            );
            self.mtp_unavailable_reason = Some(reason);
            self.mtp_allocation = None;
            self.mtp = None;
        }
        let mut timings = DecodeTimings {
            enqueue_us: enqueue_started.elapsed().as_micros() as u64,
            residual_capture_d2h_sync_us: self.capture.lock().unwrap().copy_us,
            ..Default::default()
        };
        if profile_backend_sync_enabled() {
            let completion_started = Instant::now();
            unsafe { sys::llama_synchronize(self.ctx) };
            timings.backend_completion_us = completion_started.elapsed().as_micros() as u64;
        }
        self.finish_moe_callback(&mut timings)?;
        Ok(timings)
    }

    // ── Residual extraction ───────────────────────────────────────────────────

    /// Consume the residual captured during the last `decode_*` call.
    ///
    /// Must be called exactly once per decode when `need_embeddings` is true.
    pub fn take_residual(&self, n_tokens: usize) -> Result<CapturedResidual> {
        if self.capture_ok {
            let mut capture = self.capture.lock().unwrap();
            let captured = take_captured_residual(&mut capture)?;
            if captured.shape.first().copied() != Some(n_tokens) {
                bail!(
                    "residual capture: captured {} tokens, expected {} (shape {:?}, last tensor shape {:?}, strides {:?}, nbytes {:?})",
                    captured.shape.first().copied().unwrap_or(0),
                    n_tokens,
                    captured.shape,
                    capture.last_shape,
                    capture.last_strides,
                    capture.last_nbytes
                );
            }
            Ok(captured)
        } else {
            // Fallback: post-output_norm embeddings. Only correct when the
            // slice's output_norm is all-ones (shallow cuts are wrong with this).
            let mut out = Vec::with_capacity(n_tokens * self.n_embd);
            for i in 0..n_tokens {
                let ptr = unsafe { sys::llama_get_embeddings_ith(self.ctx, i as i32) };
                if ptr.is_null() {
                    bail!("llama_get_embeddings_ith returned NULL for i={i}");
                }
                unsafe {
                    out.extend_from_slice(std::slice::from_raw_parts(ptr, self.n_embd));
                }
            }
            Ok(CapturedResidual {
                values: out,
                shape: vec![n_tokens, self.n_embd],
            })
        }
    }

    // ── Logits ───────────────────────────────────────────────────────────────

    pub fn logits_ith(&self, i: i32) -> &[f32] {
        let ptr = unsafe { sys::llama_get_logits_ith(self.ctx, i) };
        if ptr.is_null() {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(ptr, self.n_vocab) }
    }

    // ── KV cache management ───────────────────────────────────────────────────

    /// Descarta posições do cache, e diz se conseguiu.
    ///
    /// O booleano não é decoração: num modelo híbrido de atenção linear (o
    /// `gated_delta_net` do Qwen3-Next, o Ornith) o estado recorrente é uma
    /// soma acumulada, não uma tabela de onde se apaga uma linha, e o
    /// llama.cpp responde `false` sem remover nada. Descartar esse retorno era
    /// o que fazia o `n_past` do nó recuar enquanto o cache ficava parado -
    /// e a rodada seguinte morria com "inconsistent sequence positions",
    /// quatro tokens depois de a especulação começar.
    #[must_use]
    pub fn kv_remove(&self, seq_id: u32, from: i32, to: i32) -> bool {
        // Use the newer llama_get_memory + llama_memory_seq_rm API.
        unsafe {
            let mem = sys::llama_get_memory(self.ctx);
            if mem.is_null() {
                return false;
            }
            sys::llama_memory_seq_rm(mem, seq_id as i32, from, to)
        }
    }

    /// Compartilha o KV de `src` com `dst`, sequência inteira.
    ///
    /// É o que permite **não gerar dentro da sequência do prompt**: o prompt
    /// fica numa sequência que ninguém estende, a geração corre numa cópia, e
    /// no fim do turno a cópia some inteira - remoção total, que todo motor
    /// aceita, inclusive os recorrentes que recusam remoção parcial (ver
    /// `kv_remove`). Sem isto o reuso de prefixo dependeria de o modelo aceitar
    /// recuar o cache, e essa é justamente a dependência que o desenho remove.
    ///
    /// Custo: com o cache unificado (`kv_unified`, ver `llama_context_params`)
    /// as duas sequências dividem as mesmas células e a cópia é só metadado -
    /// nenhum tensor é duplicado. O llama.cpp faz cópia-na-escrita quando a
    /// geração escreve por cima de uma célula compartilhada, inclusive no
    /// estado recorrente (`llama_memory_recurrent::find_slot`).
    pub fn kv_copy_seq(&self, src: u32, dst: u32) {
        if src == dst {
            return;
        }
        unsafe {
            let mem = sys::llama_get_memory(self.ctx);
            if mem.is_null() {
                return;
            }
            sys::llama_memory_seq_cp(mem, src as i32, dst as i32, -1, -1);
        }
    }

    /// `llama_memory_can_shift` - se o llama.cpp consegue reindexar (RoPE) as
    /// posições já cacheadas, o "K-shift" que o llama.cpp de referência usa
    /// para `--ctx-shift`/geração infinita quando o contexto enche. A cauda
    /// usa este valor como guarda antes de produzir o próximo `next_draft` em
    /// modelos híbridos/recorrentes; a remoção efetiva continua sendo
    /// `kv_remove` (`llama_memory_seq_rm`), uma operação separada.
    pub fn kv_can_shift(&self) -> bool {
        unsafe {
            let mem = sys::llama_get_memory(self.ctx);
            !mem.is_null() && sys::llama_memory_can_shift(mem)
        }
    }
}

/// Monta peças de token em texto válido entre chamadas.
///
/// O vocabulário devolve bytes crus por token: um token BYTE é um byte só
/// (`llama-vocab.cpp::token_to_piece`, ramo `LLAMA_TOKEN_ATTR_BYTE`) e um
/// token normal pode terminar no meio de um codepoint. Converter cada peça
/// com `from_utf8_lossy` por chamada nascia U+FFFD independente a cada
/// fragmento e rasgava a palavra no delta do chat. O acumulador guarda o
/// rabicho incompleto e só emite caractere completo; `flush` drena o resto
/// no fim do stream.
pub(crate) struct Utf8Assembler {
    pending: Vec<u8>,
}

impl Utf8Assembler {
    pub(crate) fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Acrescenta os bytes da peça e devolve o texto completo até aqui.
    ///
    /// Sequência incompleta no fim fica pendente para a peça seguinte;
    /// sequência inválida de verdade vira um único U+FFFD na hora, a mesma
    /// semântica de um `from_utf8_lossy` sobre o stream inteiro.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.push_str(s);
                    self.pending.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        out.push_str(
                            std::str::from_utf8(&self.pending[..valid])
                                .expect("prefixo validado por from_utf8"),
                        );
                    }
                    match e.error_len() {
                        // Incompleta: o caractere pode completar na próxima peça.
                        None => {
                            self.pending.drain(..valid);
                            break;
                        }
                        // Inválida: não vai completar nunca; repõe e segue.
                        Some(n) => {
                            self.pending.drain(..valid + n);
                            out.push('\u{fffd}');
                        }
                    }
                }
            }
        }
        out
    }

    /// Fim do stream: drena o rabicho que não virou caractere completo.
    pub(crate) fn flush(&mut self) -> String {
        let out = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        out
    }
}

/// A configuração de um contexto do pool.
///
/// Separada para poder ser conferida sem GPU: é ela que sustenta a promessa de
/// que o pool não custa memória a mais. Como `cfg.n_ctx` é a capacidade total,
/// cada contexto recebe `cfg.n_ctx / n`, e `max_sessions = 1` porque agora cada
/// contexto atende uma sessão.
fn per_context_config(cfg: &EngineConfig, n: usize) -> EngineConfig {
    EngineConfig {
        n_ctx: cfg.n_ctx / n.max(1) as u32,
        max_sessions: 1,
        ..cfg.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchEntry {
    pub item_id: String,
    pub seq_id: u32,
    pub pos: u32,
    pub tokens: usize,
    pub want_logits: bool,
}

impl BatchEntry {
    #[cfg(test)]
    fn test(item_id: &str, seq_id: u32, pos: u32, tokens: usize, want_logits: bool) -> Self {
        Self {
            item_id: item_id.into(),
            seq_id,
            pos,
            tokens,
            want_logits,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TokenBatchEntry {
    pub descriptor: BatchEntry,
    pub tokens: Vec<i32>,
}

#[derive(Debug, Clone)]
pub struct HiddenBatchEntry {
    pub descriptor: BatchEntry,
    pub hidden: Vec<f32>,
    pub shape: TensorShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRange {
    pub rows: Range<usize>,
    pub logits_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchLayout {
    ranges: HashMap<String, BatchRange>,
    total_tokens: usize,
}

impl BatchLayout {
    pub fn new(entries: &[BatchEntry]) -> Result<Self> {
        if entries.is_empty() {
            bail!("batch sem entradas");
        }

        let mut ranges = HashMap::with_capacity(entries.len());
        let mut seq_ids = HashSet::with_capacity(entries.len());
        let mut cursor = 0usize;
        for entry in entries {
            if entry.item_id.is_empty() {
                bail!("batch item sem item_id");
            }
            if ranges.contains_key(&entry.item_id) {
                bail!("item_id duplicado: {}", entry.item_id);
            }
            if !seq_ids.insert(entry.seq_id) {
                bail!("seq_id duplicado: {}", entry.seq_id);
            }
            if entry.tokens == 0 {
                bail!("item {} sem tokens", entry.item_id);
            }
            let end = cursor
                .checked_add(entry.tokens)
                .ok_or_else(|| anyhow::anyhow!("batch excede o limite numérico"))?;
            let logits_index = entry.want_logits.then_some(end - 1);
            ranges.insert(
                entry.item_id.clone(),
                BatchRange {
                    rows: cursor..end,
                    logits_index,
                },
            );
            cursor = end;
        }

        Ok(Self {
            ranges,
            total_tokens: cursor,
        })
    }

    pub fn total_tokens(&self) -> usize {
        self.total_tokens
    }

    pub fn range(&self, item_id: &str) -> Option<&BatchRange> {
        self.ranges.get(item_id)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchDecodeOutput {
    pub layout: BatchLayout,
    residual: Option<Vec<f32>>,
    residual_shape: TensorShape,
    n_embd: usize,
}

impl BatchDecodeOutput {
    #[cfg(test)]
    fn test(layout: BatchLayout, residual: Vec<f32>, n_embd: usize) -> Self {
        Self {
            layout,
            residual: Some(residual),
            residual_shape: vec![0, n_embd],
            n_embd,
        }
    }

    pub fn residual_for(&self, item_id: &str, n_embd: usize) -> Result<(Vec<f32>, TensorShape)> {
        let range = self
            .layout
            .range(item_id)
            .ok_or_else(|| anyhow::anyhow!("item_id ausente no resultado: {item_id}"))?;
        let residual = self
            .residual
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("batch não capturou residual"))?;
        if n_embd != self.n_embd {
            bail!(
                "n_embd solicitado {} diverge do batch {}",
                n_embd,
                self.n_embd
            );
        }
        let feature_width = self
            .residual_shape
            .get(1..)
            .ok_or_else(|| anyhow::anyhow!("shape do residual ausente no resultado"))?
            .iter()
            .try_fold(1usize, |width, &dimension| width.checked_mul(dimension))
            .ok_or_else(|| anyhow::anyhow!("largura de residual excede o limite numérico"))?;
        let start = range
            .rows
            .start
            .checked_mul(feature_width)
            .ok_or_else(|| anyhow::anyhow!("offset de residual excede o limite numérico"))?;
        let end = range
            .rows
            .end
            .checked_mul(feature_width)
            .ok_or_else(|| anyhow::anyhow!("fim de residual excede o limite numérico"))?;
        let values = residual
            .get(start..end)
            .ok_or_else(|| anyhow::anyhow!("residual incompleto para item {item_id}"))?
            .to_vec();
        let mut shape = self.residual_shape.clone();
        shape[0] = range.rows.len();
        Ok((values, shape))
    }
}

#[cfg(test)]
mod batch_layout_tests {
    use super::*;

    #[test]
    fn layout_marca_a_posicao_global_de_logits_de_cada_item() {
        let entries = vec![
            BatchEntry::test("a", 0, 10, 3, true),
            BatchEntry::test("b", 1, 4, 2, true),
            BatchEntry::test("c", 2, 8, 1, false),
        ];
        let layout = BatchLayout::new(&entries).unwrap();

        assert_eq!(layout.total_tokens(), 6);
        assert_eq!(layout.range("a").unwrap().rows, 0..3);
        assert_eq!(layout.range("a").unwrap().logits_index, Some(2));
        assert_eq!(layout.range("b").unwrap().rows, 3..5);
        assert_eq!(layout.range("b").unwrap().logits_index, Some(4));
        assert_eq!(layout.range("c").unwrap().rows, 5..6);
        assert_eq!(layout.range("c").unwrap().logits_index, None);
    }

    #[test]
    fn layout_recusa_item_duplicado_ou_sem_tokens() {
        let duplicate = vec![
            BatchEntry::test("a", 0, 0, 1, false),
            BatchEntry::test("a", 1, 0, 1, false),
        ];
        assert!(BatchLayout::new(&duplicate)
            .unwrap_err()
            .to_string()
            .contains("item_id duplicado"));

        let empty = vec![BatchEntry::test("empty", 0, 0, 0, false)];
        assert!(BatchLayout::new(&empty)
            .unwrap_err()
            .to_string()
            .contains("sem tokens"));
    }

    #[test]
    fn saida_do_batch_devolve_residual_somente_do_item_pedido() {
        let entries = vec![
            BatchEntry::test("a", 0, 0, 2, false),
            BatchEntry::test("b", 1, 0, 1, false),
        ];
        let layout = BatchLayout::new(&entries).unwrap();
        let output = BatchDecodeOutput::test(layout, vec![1.0, 2.0, 3.0], 1);

        assert_eq!(
            output.residual_for("a", 1).unwrap(),
            (vec![1.0, 2.0], vec![2, 1])
        );
        assert_eq!(
            output.residual_for("b", 1).unwrap(),
            (vec![3.0], vec![1, 1])
        );
        assert!(output.residual_for("missing", 1).is_err());
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    fn cfg() -> EngineConfig {
        EngineConfig {
            n_ctx: 120_320,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: true,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 512,
            n_gpu_layers: 99,
            n_threads: 0,
            max_sessions: 2,
            need_embeddings: true,
            mtp: false,
            mtp_cpu_fallback_enabled: true,
            rs_seq_window: 0,
            moe_external_executor: false,
            moe_external_executor_layers: Vec::new(),
        }
    }

    #[test]
    fn gpu_kv_failure_fallback_keeps_the_requested_gpu_layers() {
        let cfg = cfg();
        let fallback = LlmEngine::cpu_kv_fallback(&cfg).expect("GPU KV was enabled");

        assert!(!fallback.offload_kqv);
        assert_eq!(fallback.n_gpu_layers, cfg.n_gpu_layers);
        assert_eq!(fallback.n_ctx, cfg.n_ctx);
    }

    #[test]
    fn cpu_kv_configuration_does_not_retry() {
        let mut cfg = cfg();
        cfg.offload_kqv = false;

        assert!(LlmEngine::cpu_kv_fallback(&cfg).is_none());
    }

    #[test]
    fn fit_only_auto_adjusts_the_default_gpu_layer_setting() {
        assert!(fit_can_adjust_layers(&cfg()));

        let mut explicit_layers = cfg();
        explicit_layers.n_gpu_layers = 20;
        assert!(!fit_can_adjust_layers(&explicit_layers));

        let mut disabled = cfg();
        disabled.fit_memory = false;
        assert!(!fit_can_adjust_layers(&disabled));
    }

    /// A regressão de 2026-08-21: prompt vindo de um cliente OpenAI externo
    /// morria com "got 655360 floats, expected 2621440 (512x5120)".
    ///
    /// 655360 = 128 x 5120, e 128 era o `FIT_MAX_UBATCH` da altura - hoje ele
    /// vale 512, por escolha do operador. O chunk de 512 tokens virava quatro
    /// microlotes, a captura substituia a cada um, e sobrava so o ultimo
    /// quarto. Pela UI passava despercebido porque prompt curto cabe num
    /// microlote so - e por isso o teste abaixo usa mais de um, sem depender
    /// do valor concreto da constante.
    #[test]
    fn capture_accumulates_every_microbatch_of_one_decode() {
        let mut slot = CaptureSlot {
            want: None,
            buf: None,
            expected_tokens: 0,
            max_microbatch_tokens: 0,
            n_embd: 0,
            expected_floats: None,
            layout: None,
            error: None,
            last_shape: None,
            last_strides: None,
            last_nbytes: None,
            copy_us: 0,
        };

        let n_embd = 4;
        let ubatch = 2;
        let tokens = 6; // tres microlotes, como um chunk maior que `n_ubatch`
        for micro in 0..(tokens / ubatch) {
            let inicio = (micro * ubatch * n_embd) as f32;
            let pedaco: Vec<f32> = (0..(ubatch * n_embd)).map(|i| inicio + i as f32).collect();
            accumulate_capture(&mut slot, pedaco);
        }

        let capturado = slot.buf.expect("a captura tem que ter disparado");
        assert_eq!(
            capturado.len(),
            tokens * n_embd,
            "o residual do chunk inteiro, nao o do ultimo microlote"
        );
        let esperado: Vec<f32> = (0..(tokens * n_embd)).map(|i| i as f32).collect();
        assert_eq!(
            capturado, esperado,
            "os microlotes entram na ordem em que o split_simple os produz"
        );
    }

    /// Sem isto, o residual de uma requisicao entraria concatenado no da
    /// proxima - o modo de falha que a acumulacao introduziria se o buffer
    /// nao fosse zerado a cada decode.
    #[test]
    fn a_new_decode_starts_from_an_empty_capture() {
        let mut slot = CaptureSlot {
            want: None,
            buf: None,
            expected_tokens: 0,
            max_microbatch_tokens: 0,
            n_embd: 0,
            expected_floats: None,
            layout: None,
            error: None,
            last_shape: None,
            last_strides: None,
            last_nbytes: None,
            copy_us: 0,
        };
        accumulate_capture(&mut slot, vec![1.0, 2.0]);
        slot.buf = None; // o que `reset_capture_slot` faz antes de cada decode
        accumulate_capture(&mut slot, vec![3.0, 4.0]);
        assert_eq!(slot.buf.expect("captura"), vec![3.0, 4.0]);
    }

    #[test]
    fn capture_ignores_duplicate_named_boundary_after_expected_rows() {
        let mut slot = CaptureSlot {
            want: None,
            buf: None,
            expected_tokens: 1,
            max_microbatch_tokens: 1,
            n_embd: 4,
            expected_floats: Some(4),
            layout: None,
            error: None,
            last_shape: None,
            last_strides: None,
            last_nbytes: None,
            copy_us: 0,
        };

        accumulate_capture(&mut slot, vec![1.0, 2.0, 3.0, 4.0]);
        accumulate_capture(&mut slot, vec![5.0, 6.0, 7.0, 8.0]);

        assert_eq!(slot.buf.as_deref(), Some(&[1.0, 2.0, 3.0, 4.0][..]));
    }

    fn capture_slot_for_test(
        expected_tokens: usize,
        n_embd: usize,
        max_microbatch_tokens: usize,
    ) -> CaptureSlot {
        CaptureSlot {
            want: None,
            buf: None,
            expected_tokens,
            max_microbatch_tokens,
            n_embd,
            expected_floats: None,
            layout: None,
            error: None,
            last_shape: None,
            last_strides: None,
            last_nbytes: None,
            copy_us: 0,
        }
    }

    #[test]
    fn residual_capture_without_extra_dimension_keeps_the_current_wire_shape() {
        let mut slot = capture_slot_for_test(2, 4, 4);
        record_capture(
            &mut slot,
            [4, 2, 1, 1],
            [4, 16, 32, 32],
            2 * 4 * 4,
            (0..8).map(|value| value as f32).collect(),
        )
        .unwrap();

        let captured = take_captured_residual(&mut slot).unwrap();
        assert_eq!(captured.shape, vec![2, 4]);
        assert_eq!(
            captured.values,
            (0..8).map(|value| value as f32).collect::<Vec<_>>()
        );
    }

    #[test]
    fn residual_capture_accepts_the_classic_ggml_shape_with_trailing_singletons() {
        let mut slot = capture_slot_for_test(3, 4096, 4096);
        let values = (0..(3 * 4096))
            .map(|value| value as f32)
            .collect::<Vec<_>>();
        record_capture(
            &mut slot,
            [4096, 3, 1, 1],
            [4, 16_384, 49_152, 49_152],
            values.len() * 4,
            values.clone(),
        )
        .unwrap();

        let captured = take_captured_residual(&mut slot).unwrap();
        assert_eq!(captured.shape, vec![3, 4096]);
        assert_eq!(captured.values, values);
    }

    #[test]
    fn residual_capture_accepts_a_model_without_hyperconnection_streams() {
        let mut slot = capture_slot_for_test(4, 8, 8);
        let values = (0..32).map(|value| value as f32).collect::<Vec<_>>();
        record_capture(
            &mut slot,
            [8, 4, 1, 1],
            [4, 32, 128, 128],
            values.len() * 4,
            values.clone(),
        )
        .unwrap();

        let captured = take_captured_residual(&mut slot).unwrap();
        assert_eq!(captured.shape, vec![4, 8]);
        assert_eq!(captured.values, values);
    }

    #[test]
    fn residual_capture_preserves_extra_stream_dimensions() {
        let mut slot = capture_slot_for_test(11, 4096, 4096);
        let values = (0..(11 * 4096 * 4))
            .map(|value| value as f32)
            .collect::<Vec<_>>();
        record_capture(
            &mut slot,
            [4096, 4, 11, 1],
            [4, 16_384, 65_536, 720_896],
            values.len() * 4,
            values.clone(),
        )
        .unwrap();

        let captured = take_captured_residual(&mut slot).unwrap();
        assert_eq!(captured.shape, vec![11, 4096, 4]);
        assert_eq!(captured.values, values);
    }

    #[test]
    fn residual_capture_accumulates_extra_dimensions_in_microbatch_order() {
        let mut slot = capture_slot_for_test(5, 4, 2);
        record_capture(
            &mut slot,
            [4, 2, 2, 1],
            [4, 16, 32, 64],
            2 * 8 * 4,
            (0..16).map(|value| value as f32).collect(),
        )
        .unwrap();
        record_capture(
            &mut slot,
            [4, 2, 2, 1],
            [4, 16, 32, 64],
            2 * 8 * 4,
            (16..32).map(|value| value as f32).collect(),
        )
        .unwrap();
        record_capture(
            &mut slot,
            [4, 2, 1, 1],
            [4, 16, 32, 32],
            8 * 4,
            (32..40).map(|value| value as f32).collect(),
        )
        .unwrap();

        let captured = take_captured_residual(&mut slot).unwrap();
        assert_eq!(captured.shape, vec![5, 4, 2]);
        let expected = (0..40).map(|value| value as f32).collect::<Vec<_>>();
        assert_eq!(captured.values, expected);
    }

    #[test]
    fn residual_capture_names_unsupported_trailing_dimensions() {
        let mut slot = capture_slot_for_test(2, 4, 8);
        let error = record_capture(
            &mut slot,
            [4, 2, 2, 3],
            [4, 16, 32, 64],
            2 * 3 * 8 * 4,
            vec![0.0; 2 * 3 * 8],
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("dimensões posteriores"),
            "erro inesperado: {error}"
        );
    }

    #[test]
    fn residual_capture_preserves_an_internal_singleton_before_tokens() {
        let mut slot = capture_slot_for_test(2, 4, 4);
        record_capture(
            &mut slot,
            [4, 1, 2, 1],
            [4, 16, 16, 32],
            2 * 4 * 4,
            vec![0.0; 2 * 4],
        )
        .unwrap();
        let captured = take_captured_residual(&mut slot).unwrap();
        assert_eq!(captured.shape, vec![2, 4, 1]);
        assert_eq!(captured.values, vec![0.0; 2 * 4]);
    }

    #[test]
    fn residual_capture_does_not_use_n_embd_in_to_find_the_token_axis() {
        // The surrounding model may have n_embd=4 and n_embd_inp=8. The
        // residual is still identified from its physical shape and token
        // count, not from that input-only metadata.
        let mut slot = capture_slot_for_test(3, 4, 3);
        let values = (0..12).map(|value| value as f32).collect::<Vec<_>>();
        record_capture(
            &mut slot,
            [4, 3, 1, 1],
            [4, 16, 48, 48],
            values.len() * 4,
            values.clone(),
        )
        .unwrap();

        let captured = take_captured_residual(&mut slot).unwrap();
        assert_eq!(captured.shape, vec![3, 4]);
        assert_eq!(captured.values, values);
    }

    #[test]
    fn residual_capture_resolves_ambiguous_single_token_axis_deterministically() {
        let mut slot = capture_slot_for_test(1, 4, 1);
        record_capture(
            &mut slot,
            [4, 1, 1, 1],
            [4, 16, 16, 16],
            4 * 4,
            vec![1.0, 2.0, 3.0, 4.0],
        )
        .unwrap();

        let captured = take_captured_residual(&mut slot).unwrap();
        assert_eq!(captured.shape, vec![1, 4]);
        assert_eq!(captured.values, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn fit_mode_caps_compute_microbatch_without_changing_prompt_batch() {
        let cfg = cfg();
        assert_eq!(cfg.n_batch, 512);
        assert_eq!(effective_n_ubatch(&cfg), 128);

        let mut no_fit = cfg;
        no_fit.fit_memory = false;
        assert_eq!(effective_n_ubatch(&no_fit), 512);

        let mut small_batch = no_fit;
        small_batch.n_batch = 64;
        small_batch.fit_memory = true;
        assert_eq!(effective_n_ubatch(&small_batch), 64);
    }

    #[test]
    fn context_batch_params_cap_a_large_mtp_batch_with_fit_enabled() {
        let mut cfg = cfg();
        cfg.mtp = true;
        cfg.n_batch = 4096;

        let mut params = unsafe { sys::llama_context_default_params() };
        apply_context_batch_params(&mut params, &cfg);

        assert_eq!(params.n_batch, 4096);
        // 512 fixo de proposito: e a escolha do operador para FIT_MAX_UBATCH e
        // ja foi revertida para 128 uma vez. Comparar com a constante deixaria
        // o teste passar na mesma; o literal faz esta mudanca ficar vermelha.
        assert_eq!(params.n_ubatch, 512);
    }

    #[test]
    fn context_batch_params_follow_the_requested_batch_when_fit_is_disabled() {
        let mut cfg = cfg();
        cfg.mtp = true;
        cfg.fit_memory = false;
        cfg.n_batch = 4096;

        let mut params = unsafe { sys::llama_context_default_params() };
        apply_context_batch_params(&mut params, &cfg);

        assert_eq!(params.n_batch, 4096);
        assert_eq!(params.n_ubatch, 4096);
    }

    #[test]
    fn mtp_configuration_requests_loading_nextn_weights() {
        let mut cfg = cfg();
        assert_eq!(empty_fit_plan(&cfg).mtp_mode, MtpFitMode::Disabled);

        cfg.mtp = true;
        assert_eq!(empty_fit_plan(&cfg).mtp_mode, MtpFitMode::Vram);
    }

    /// O pool nao pode custar memoria a mais: o cache de cada contexto e o
    /// mesmo pedaco que a sessao ja tinha. Numeros da ksia em 2026-08-14, onde
    /// um contexto de 120.320 atendia 2 sessoes de 60.160.
    #[test]
    fn each_context_gets_the_slice_a_session_already_had() {
        let por_contexto = per_context_config(&cfg(), 2);
        assert_eq!(por_contexto.n_ctx, 60_160);
        assert_eq!(por_contexto.n_ctx * 2, cfg().n_ctx, "o total nao cresce");
    }

    /// Cada contexto atende uma sessao; quem divide agora e o pool.
    #[test]
    fn a_pooled_context_serves_one_session() {
        assert_eq!(per_context_config(&cfg(), 4).max_sessions, 1);
    }

    /// O resto da configuracao e do no, nao do contexto, e nao pode divergir
    /// entre irmaos.
    #[test]
    fn everything_else_is_carried_over_unchanged() {
        let p = per_context_config(&cfg(), 3);
        assert_eq!(p.n_batch, 512);
        assert_eq!(p.n_gpu_layers, 99);
        assert!(p.offload_kqv);
        assert!(p.need_embeddings);
    }

    /// Zero contextos nao existe, e dividir por zero menos ainda.
    #[test]
    fn a_pool_of_zero_is_treated_as_one() {
        assert_eq!(per_context_config(&cfg(), 0).n_ctx, cfg().n_ctx);
    }
}

// ── Sampling ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SamplingConfig {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: i32,
    pub repeat_penalty: f32,
    pub repeat_last_n: usize,
}

/// CPU-only sampling boundaries. GPU/backend completion is measured before
/// this function, so a long CPU sampler cannot hide a pending CUDA stream.
///
/// Measured in **microseconds**. Milliseconds truncated every stage that cost
/// less than 1 ms to zero, which after this rewrite is every stage — the whole
/// sampler now costs less than one of the old millisecond ticks.
/// O acesso aos logits e a conversão do token em texto ficam de fora: quem os
/// mede é o chamador (`infer_server::NodeTimings`), porque acontecem antes e
/// depois desta função. Campo que ninguém preenche vira zero na tela e some da
/// soma sem avisar.
#[derive(Debug, Clone, Default)]
pub struct SamplingTimings {
    pub logits_clone_us: u64,
    pub repetition_penalty_us: u64,
    pub softmax_us: u64,
    pub top_k_us: u64,
    pub top_p_us: u64,
    pub selection_us: u64,
}

static SAMPLER_RNG: Mutex<u64> = Mutex::new(0x853c49e6748fea9b);

#[cfg(test)]
static SAMPLER_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Temperature + top-k + top-p + repetition-penalty sampling.
///
/// Returns the sampled token id. When `temperature <= 0` this is greedy argmax.
pub fn sample_token_profiled(
    logits: &[f32],
    recent: &[i32],
    cfg: &SamplingConfig,
) -> (i32, SamplingTimings) {
    let mut timings = SamplingTimings::default();
    if logits.is_empty() {
        return (0, timings);
    }
    let kept = sampling_candidates_profiled(logits, recent, cfg, &mut timings);
    if cfg.temperature <= 0.0 {
        let selection_started = Instant::now();
        let token = kept[0].0 as i32;
        timings.selection_us = selection_started.elapsed().as_micros() as u64;
        return (token, timings);
    }
    // ── Multinomial draw ─────────────────────────────────────────────────────
    //
    // The candidates are normalized and in ascending id order, so this is the
    // only place that draws from the distribution.
    let selection_started = Instant::now();
    let rng_val: f32 = {
        // A simple LCG, to avoid pulling in `rand`.
        let mut r = SAMPLER_RNG.lock().unwrap();
        next_sampler_rng_value(&mut r)
    };

    let mut cumsum = 0.0f32;
    let mut best = (0usize, f32::NEG_INFINITY);
    for (i, p) in &kept {
        cumsum += *p;
        if rng_val <= cumsum {
            timings.selection_us = selection_started.elapsed().as_micros() as u64;
            return (*i as i32, timings);
        }
        // Rounding can leave `rng_val` just past the final cumulative sum; the
        // most likely token is the answer then, matching the old fallback.
        if *p >= best.1 {
            best = (*i, *p);
        }
    }
    timings.selection_us = selection_started.elapsed().as_micros() as u64;
    (best.0 as i32, timings)
}

fn next_sampler_rng_value(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mantissa = ((*state >> 33) as u32) & 0x007f_ffff;
    let bits = mantissa | 0x3f800000;
    f32::from_bits(bits) - 1.0
}

fn sampling_candidates_profiled(
    logits: &[f32],
    recent: &[i32],
    cfg: &SamplingConfig,
    timings: &mut SamplingTimings,
) -> Vec<(usize, f32)> {
    let clone_started = Instant::now();
    let mut adjusted = logits.to_vec();
    timings.logits_clone_us = clone_started.elapsed().as_micros() as u64;

    let repetition_started = Instant::now();
    apply_repetition_penalty(&mut adjusted, recent, cfg);
    timings.repetition_penalty_us = repetition_started.elapsed().as_micros() as u64;

    if cfg.temperature <= 0.0 {
        return vec![(argmax(&adjusted) as usize, 1.0)];
    }

    // Selecting the k largest raw logits is O(V): positive-temperature softmax
    // preserves their order, and index breaks a tie without widening the set.
    let top_k_started = Instant::now();
    let ids = top_k_ids(&adjusted, cfg.top_k);
    timings.top_k_us = top_k_started.elapsed().as_micros() as u64;

    let softmax_started = Instant::now();
    let max_l = ids
        .iter()
        .map(|&i| adjusted[i])
        .fold(f32::NEG_INFINITY, f32::max);
    let mut candidates = ids
        .into_iter()
        .map(|i| (i, ((adjusted[i] - max_l) / cfg.temperature).exp()))
        .collect::<Vec<_>>();
    normalise_candidates(&mut candidates, &adjusted);
    timings.softmax_us = softmax_started.elapsed().as_micros() as u64;

    let top_p_started = Instant::now();
    if cfg.top_p < 1.0 {
        let mut ranked = candidates.clone();
        ranked.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let target = cfg.top_p.max(0.0);
        let mut mass = 0.0;
        let mut keep = 0;
        for (_, probability) in &ranked {
            mass += *probability;
            keep += 1;
            if mass >= target {
                break;
            }
        }
        ranked.truncate(keep);
        ranked.sort_unstable_by_key(|(id, _)| *id);
        candidates = ranked;
        normalise_candidates(&mut candidates, &adjusted);
    }
    timings.top_p_us = top_p_started.elapsed().as_micros() as u64;
    candidates
}

#[cfg(test)]
fn sampling_candidates(logits: &[f32], recent: &[i32], cfg: &SamplingConfig) -> Vec<(usize, f32)> {
    sampling_candidates_profiled(logits, recent, cfg, &mut SamplingTimings::default())
}

fn apply_repetition_penalty(logits: &mut [f32], recent: &[i32], cfg: &SamplingConfig) {
    if cfg.repeat_penalty == 1.0 || recent.is_empty() {
        return;
    }
    let window_start = recent.len().saturating_sub(cfg.repeat_last_n);
    let window: std::collections::HashSet<i32> = recent[window_start..].iter().copied().collect();
    for tok in window {
        let token = tok as usize;
        if token < logits.len() {
            logits[token] = if logits[token] > 0.0 {
                logits[token] / cfg.repeat_penalty
            } else {
                logits[token] * cfg.repeat_penalty
            };
        }
    }
}

fn top_k_ids(logits: &[f32], top_k: i32) -> Vec<usize> {
    if top_k <= 0 || (top_k as usize) >= logits.len() {
        return (0..logits.len()).collect();
    }

    let k = top_k as usize;
    let mut ids: Vec<usize> = (0..logits.len()).collect();
    ids.select_nth_unstable_by(k - 1, |&a, &b| {
        logits[b].total_cmp(&logits[a]).then_with(|| a.cmp(&b))
    });
    ids.truncate(k);
    ids.sort_unstable();
    ids
}

fn normalise_candidates(candidates: &mut Vec<(usize, f32)>, adjusted: &[f32]) {
    let total = candidates
        .iter()
        .map(|(_, probability)| *probability)
        .sum::<f32>();
    if total > 0.0 {
        for (_, probability) in candidates {
            *probability /= total;
        }
    } else {
        candidates.clear();
        candidates.push((argmax(adjusted) as usize, 1.0));
    }
}

/// Index of the largest value, matching `max_by`'s "last one wins" on ties.
fn argmax(values: &[f32]) -> i32 {
    values
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(i, _)| i as i32)
        .unwrap_or(0)
}

/// Atalho sem a decomposição de tempos. A produção chama
/// `sample_token_profiled` direto, porque alimenta a telemetria por hop; este
/// embrulho existe para os testes que só querem o token.
#[cfg(test)]
pub fn sample_token(logits: &[f32], recent: &[i32], cfg: &SamplingConfig) -> i32 {
    sample_token_profiled(logits, recent, cfg).0
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn template_marker<'a>(template: &str, marker: &'a str) -> Result<&'a str> {
    if template.contains(marker) {
        Ok(marker)
    } else {
        bail!("GGUF Gemma4 chat_template não contém o marcador {marker:?}")
    }
}

/// Render the Gemma4 template shape stored in the GGUF.
///
/// llama.cpp's public C helper accepts the Jinja source but has no argument
/// for `enable_thinking`. This renderer only supports the exact control-flow
/// markers it finds in the model's own template and refuses a drifted
/// template instead of substituting a Qwen-like prompt.
fn render_gemma4_template(
    template: &str,
    bos: &str,
    messages: &[(&str, &str)],
    add_generation_prompt: bool,
    no_thinking: bool,
) -> Result<String> {
    let turn = template_marker(template, "<|turn>")?;
    let turn_end = template_marker(template, "<turn|>")?;
    let think = template_marker(template, "<|think|>")?;
    let channel = template_marker(template, "<|channel>")?;
    let channel_end = template_marker(template, "<channel|>")?;
    let enable_thinking = !no_thinking;

    let mut out = String::from(bos);
    let mut loop_messages = messages;
    let has_system_block = enable_thinking
        || messages
            .first()
            .is_some_and(|(role, _)| *role == "system" || *role == "developer");
    if has_system_block {
        out.push_str(turn);
        out.push_str("system\n");
        if enable_thinking {
            out.push_str(think);
            out.push('\n');
        }
        if let Some((role, content)) = messages.first() {
            if *role == "system" || *role == "developer" {
                out.push_str(content.trim());
                loop_messages = &messages[1..];
            }
        }
        out.push_str(turn_end);
        out.push('\n');
    }

    for (role, content) in loop_messages {
        let role = if *role == "assistant" { "model" } else { role };
        out.push_str(turn);
        out.push_str(role);
        out.push('\n');
        out.push_str(content.trim());
        out.push_str(turn_end);
        out.push('\n');
    }

    if add_generation_prompt {
        out.push_str(turn);
        out.push_str("model\n");
        if !enable_thinking {
            out.push_str(channel);
            out.push_str("thought\n");
            out.push_str(channel_end);
        }
    }
    Ok(out)
}

fn token_sequence_ends_with(tokens: &[i32], suffix: &[i32]) -> bool {
    !suffix.is_empty() && tokens.len() >= suffix.len() && tokens.ends_with(suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receptor_aceita_hidden_state_com_fluxos_expandidos() {
        let shape = vec![11, 4096, 4];
        let values_len = 11 * 4096 * 4;

        validate_hidden_tensor(values_len, &shape, 11, 4096, 4096, 16_384, "hidden state")
            .expect("a largura expandida do eixo de fluxos deve ser aceita");
    }

    #[test]
    fn receptor_aceita_hidden_state_sem_fluxos() {
        let shape = vec![3, 8];

        validate_hidden_tensor(3 * 8, &shape, 3, 8, 8, 32, "hidden state")
            .expect("o modelo sem fluxos deve manter a validação atual");
    }

    #[test]
    fn receptor_mantem_singletons_de_um_modelo_sem_fluxos() {
        let shape = vec![3, 8, 1];

        validate_hidden_tensor(3 * 8, &shape, 3, 8, 8, 32, "hidden state")
            .expect("dimensões singleton não devem virar um eixo de fluxo");
    }

    #[test]
    fn receptor_recusa_largura_de_hidden_state_incompativel_nomeando_shape() {
        let shape = vec![11, 4096, 3];

        let error = validate_hidden_tensor(
            11 * 4096 * 3,
            &shape,
            11,
            4096,
            4096,
            16_384,
            "hidden state",
        )
        .expect_err("uma largura de fluxo incompatível deve continuar recusada")
        .to_string();

        assert!(error.contains("[11, 4096, 3]"), "erro inesperado: {error}");
        assert!(
            error.contains("largura de entrada 12288"),
            "erro inesperado: {error}"
        );
    }

    #[test]
    fn residual_do_emissor_via_wire_e_aceito_pelo_receptor() {
        let mut slot = CaptureSlot {
            want: None,
            buf: None,
            expected_tokens: 11,
            max_microbatch_tokens: 11,
            n_embd: 4096,
            expected_floats: None,
            layout: None,
            error: None,
            last_shape: None,
            last_strides: None,
            last_nbytes: None,
            copy_us: 0,
        };
        let values = (0..(11 * 4096 * 4))
            .map(|value| value as f32)
            .collect::<Vec<_>>();
        record_capture(
            &mut slot,
            [4096, 4, 11, 1],
            [4, 16_384, 65_536, 720_896],
            values.len() * 4,
            values,
        )
        .unwrap();
        let emitted = take_captured_residual(&mut slot).unwrap();

        let frame = crate::wire::encode(
            &serde_json::json!({"session_id": "round-trip"}),
            Some((&emitted.values, emitted.shape.clone())),
            crate::wire::WireDtype::F32,
        )
        .unwrap();
        let (_, received) = crate::wire::decode(&frame).unwrap();
        let (received_values, received_shape) = received.unwrap();

        let width = validate_hidden_tensor(
            received_values.len(),
            &received_shape,
            11,
            4096,
            4096,
            16_384,
            "hidden state",
        )
        .expect("o receptor deve aceitar exatamente a forma emitida");
        assert_eq!(received_shape, vec![11, 4096, 4]);
        assert_eq!(width, 16_384);
    }

    #[test]
    fn capture_target_accepts_an_alternate_last_layer_alias() {
        let target = CString::new("l_out-19").unwrap();
        assert!(capture_target_matches("l_out-19", &target));
        assert!(capture_target_matches("l_last-19", &target));
        assert!(!capture_target_matches("l_last-18", &target));
        assert!(!capture_target_matches("output-19", &target));
    }

    #[test]
    fn output_width_mismatch_disables_unsafe_generic_embedding_copy() {
        assert!(!generic_embeddings_compatible(true, 4096, 16384));
        assert!(generic_embeddings_compatible(true, 4096, 4096));
        assert!(!generic_embeddings_compatible(false, 4096, 4096));
    }

    const GEMMA4_TEMPLATE_FIXTURE: &str = r#"
        {{- bos_token -}}
        {%- if (enable_thinking is defined and enable_thinking) or messages[0]['role'] in ['system', 'developer'] -%}
            {{- '<|turn>system\n' -}}
            {%- if enable_thinking is defined and enable_thinking -%}
                {{- '<|think|>\n' -}}
            {%- endif -%}
            {{- '<turn|>\n' -}}
        {%- endif -%}
        {%- for message in messages -%}
            {{- '<|turn>' + ('model' if message['role'] == 'assistant' else message['role']) + '\n' -}}
            {{- message['content'] | trim -}}
            {{- '<turn|>\n' -}}
        {%- endfor -%}
        {%- if add_generation_prompt -%}
            {{- '<|turn>model\n' -}}
            {%- if not enable_thinking | default(false) -%}
                {{- '<|channel>thought\n<channel|>' -}}
            {%- endif -%}
        {%- endif -%}
    "#;

    #[test]
    fn profile_sync_defaults_on_and_accepts_explicit_off_values() {
        assert!(profile_backend_sync_value(None));
        assert!(profile_backend_sync_value(Some("1")));
        assert!(!profile_backend_sync_value(Some("0")));
        assert!(!profile_backend_sync_value(Some("false")));
    }

    #[test]
    fn cache_types_map_to_the_llama_context_types() {
        assert_eq!(cache_type_to_ggml(KvCacheType::F16), sys::GGML_TYPE_F16);
        assert_eq!(cache_type_to_ggml(KvCacheType::Q8_0), sys::GGML_TYPE_Q8_0);
        assert_eq!(
            cache_type_to_ggml(KvCacheType::Iq4Nl),
            sys::GGML_TYPE_IQ4_NL
        );
    }

    #[test]
    fn model_cache_retem_handle_ate_clear() {
        let cache = ModelCache::new();
        let loaded = Arc::new(LoadedModel {
            model: std::ptr::null_mut(),
            vocab: std::ptr::null(),
            n_embd: 1,
            n_vocab: 1,
            mtp_weights_loaded: false,
        });
        cache.put(
            "model.gguf",
            loaded.clone(),
            true,
            7,
            false,
            false,
            false,
            Vec::new(),
        );

        let first = cache
            .get("model.gguf", false, false, false, &[])
            .unwrap()
            .loaded;
        let second = cache
            .get("model.gguf", false, false, false, &[])
            .unwrap()
            .loaded;
        assert!(Arc::ptr_eq(&first, &second));
        assert!(cache.get("model.gguf", false, true, false, &[]).is_none());
        assert!(cache.get("other.gguf", false, false, false, &[]).is_none());
        assert!(cache.get("model.gguf", true, false, false, &[]).is_none());

        drop((first, second));
        cache.clear();
        assert!(cache.get("model.gguf", false, false, false, &[]).is_none());
        assert_eq!(Arc::strong_count(&loaded), 1);
    }

    #[test]
    fn cache_descarta_alvo_depois_do_recuo_mtp_para_ram() {
        let cache = ModelCache::new();
        let loaded = Arc::new(LoadedModel {
            model: std::ptr::null_mut(),
            vocab: std::ptr::null(),
            n_embd: 1,
            n_vocab: 1,
            mtp_weights_loaded: false,
        });
        cache.put(
            "model.gguf",
            loaded.clone(),
            true,
            7,
            false,
            true,
            false,
            Vec::new(),
        );

        assert!(cache.clear_mtp_fallback());
        assert!(cache.get("model.gguf", true, true, false, &[]).is_none());
        assert!(!cache.clear_mtp_fallback());
        assert_eq!(Arc::strong_count(&loaded), 1);
    }

    #[test]
    fn cache_descarta_alvo_sem_mtp_antes_de_uma_nova_tentativa() {
        let cache = ModelCache::new();
        let loaded = Arc::new(LoadedModel {
            model: std::ptr::null_mut(),
            vocab: std::ptr::null(),
            n_embd: 1,
            n_vocab: 1,
            mtp_weights_loaded: false,
        });
        cache.put(
            "model.gguf",
            loaded.clone(),
            true,
            7,
            false,
            false,
            false,
            Vec::new(),
        );

        assert!(cache.get("model.gguf", true, false, false, &[]).is_none());
        assert!(cache.clear_mtp_fallback());
        assert_eq!(Arc::strong_count(&loaded), 1);
    }

    #[test]
    fn recuo_mtp_seleciona_companheiro_com_nextn_e_nunca_alvo_sem_pesos() {
        let target = LoadedModel {
            model: std::ptr::null_mut(),
            vocab: std::ptr::null(),
            n_embd: 1,
            n_vocab: 1,
            mtp_weights_loaded: false,
        };
        let companion = LoadedModel {
            model: std::ptr::null_mut(),
            vocab: std::ptr::null(),
            n_embd: 1,
            n_vocab: 1,
            mtp_weights_loaded: true,
        };

        let selected = LlmEngine::mtp_model_for_attach(&target, Some(&companion));
        assert!(selected.is_some_and(|model| std::ptr::eq(model, &companion)));
        assert!(LlmEngine::mtp_model_for_attach(&target, None).is_none());
    }

    /// Testes do comportamento vigente do sampler: conjunto de candidatos,
    /// limites determinísticos e sorteio. Não são provas de equivalência com
    /// implementações anteriores, porque top-p e desempates evoluíram.
    mod sampler_behaviour {
        use super::*;

        fn cfg(temperature: f32, top_k: i32, top_p: f32) -> SamplingConfig {
            SamplingConfig {
                temperature,
                top_p,
                top_k,
                repeat_penalty: 1.0,
                repeat_last_n: 0,
            }
        }

        fn candidate_ids(logits: &[f32], c: &SamplingConfig) -> Vec<i32> {
            sampling_candidates(logits, &[], c)
                .into_iter()
                .map(|(id, _)| id as i32)
                .collect()
        }

        /// Zero temperature is argmax, and argmax is the highest logit.
        #[test]
        fn zero_temperature_is_argmax() {
            let logits = [0.1, 9.0, 0.3, 2.0];
            assert_eq!(sample_token(&logits, &[], &cfg(0.0, 40, 1.0)), 1);
        }

        /// With top-p disabled, top-k has to be the only thing narrowing the
        /// field: no token outside the k highest logits may ever be drawn.
        #[test]
        fn top_k_confines_the_draw_to_the_k_highest_logits() {
            let _sampling = SAMPLER_TEST_LOCK.lock().unwrap();
            // Indices 3 and 1 hold the two highest logits.
            let logits = [0.0, 5.0, 1.0, 9.0, 2.0, 0.5];
            let c = cfg(1.0, 2, 1.0);

            for _ in 0..500 {
                let picked = sample_token(&logits, &[], &c);
                assert!(picked == 1 || picked == 3, "saiu fora do top-2: {picked}");
            }
        }

        #[test]
        fn top_k_does_not_expand_past_k_on_a_tie() {
            let logits = [7.0, 7.0, 7.0, 1.0];
            assert_eq!(candidate_ids(&logits, &cfg(1.0, 1, 1.0)).len(), 1);
        }

        /// The repetition penalty divides positive logits, which can move a
        /// token out of the running entirely.
        #[test]
        fn the_repetition_penalty_can_push_a_token_out_of_the_top_k() {
            let logits = [10.0, 9.0, 0.0];
            let penalised = SamplingConfig {
                repeat_penalty: 4.0,
                repeat_last_n: 4,
                ..cfg(0.0, 40, 1.0)
            };
            // Index 0 falls to 2.5 and index 1 wins.
            assert_eq!(sample_token(&logits, &[0], &penalised), 1);
        }

        /// The measurement behind the rewrite, on a real Qwen vocabulary.
        ///
        /// Ignored because a wall-clock assertion belongs nowhere near CI, and
        /// because the debug profile makes the number meaningless. Run it the
        /// way the claim was made:
        ///
        /// ```text
        /// cargo test -p node --release sampler_cost_on_a_real_vocabulary -- --ignored --nocapture
        /// ```
        #[test]
        #[ignore = "medição de tempo: rode à mão, em --release"]
        fn sampler_cost_on_a_real_vocabulary() {
            const V: usize = 151_936; // Qwen3
            let logits: Vec<f32> = (0..V).map(|i| ((i % 977) as f32) * 0.01).collect();
            let c = cfg(0.7, 40, 1.0);

            let started = Instant::now();
            const ROUNDS: u32 = 50;
            for _ in 0..ROUNDS {
                std::hint::black_box(sample_token(&logits, &[], &c));
            }
            let per_token = started.elapsed() / ROUNDS;
            println!("amostragem: {per_token:?}/token com vocabulário de {V}");

            // The full-sort version cost ~30 ms/token here. Anything in that
            // range means the O(V) selection is not being taken.
            assert!(
                per_token < std::time::Duration::from_millis(5),
                "amostragem em {per_token:?}/token — o sort completo voltou?"
            );
        }

        #[test]
        fn top_p_keeps_the_minimum_normalised_nucleus() {
            let logits = [4.0, 3.0, 2.0, 1.0, -1.0];
            assert_eq!(candidate_ids(&logits, &cfg(1.0, 40, 0.8)), vec![0, 1]);
        }

        #[test]
        fn top_p_near_one_keeps_the_tail_and_small_top_p_does_not() {
            let logits = [4.0, 3.0, 2.0, 1.0, -1.0];
            assert_eq!(
                candidate_ids(&logits, &cfg(1.0, 40, 0.999)),
                vec![0, 1, 2, 3, 4]
            );
            assert_eq!(candidate_ids(&logits, &cfg(1.0, 40, 0.2)), vec![0]);
        }

        #[test]
        fn temperature_changes_the_nucleus_candidates() {
            let logits = [4.0, 3.0, 2.0, 1.0, 0.0];
            assert_eq!(candidate_ids(&logits, &cfg(0.5, 0, 0.9)), vec![0, 1]);
            assert_eq!(candidate_ids(&logits, &cfg(2.0, 0, 0.9)), vec![0, 1, 2, 3]);
        }

        #[test]
        fn non_positive_temperature_remains_deterministic_argmax() {
            let logits = [0.1, 9.0, 0.3, 2.0];
            for temperature in [0.0, -1.0] {
                assert_eq!(sample_token(&logits, &[], &cfg(temperature, 1, 0.01)), 1);
            }
        }

        #[test]
        fn non_positive_temperature_does_not_advance_the_lcg() {
            let _sampling = SAMPLER_TEST_LOCK.lock().unwrap();
            const SEED: u64 = 0x853c49e6748fea9b;
            let stochastic = cfg(1.0, 0, 1.0);
            let deterministic = cfg(0.0, 0, 1.0);
            let logits = [2.0, 0.0];

            *SAMPLER_RNG.lock().unwrap() = SEED;
            let expected = sample_token(&logits, &[], &stochastic);

            *SAMPLER_RNG.lock().unwrap() = SEED;
            assert_eq!(sample_token(&logits, &[], &deterministic), 0);
            assert_eq!(sample_token(&logits, &[], &stochastic), expected);
        }

        #[test]
        fn sampler_lcg_values_are_finite_unit_interval() {
            let mut state = 0x853c49e6748fea9b;
            for _ in 0..1024 {
                let value = next_sampler_rng_value(&mut state);
                assert!(value.is_finite(), "LCG devolveu {value:?}");
                assert!((0.0..1.0).contains(&value), "LCG devolveu {value:?}");
            }
        }

        #[test]
        fn top_k_then_top_p_composes_as_count_then_mass() {
            let logits = [10.0, 9.0, 8.0, 7.0, 6.0];
            assert_eq!(candidate_ids(&logits, &cfg(1.0, 3, 0.95)), vec![0, 1, 2]);
        }

        #[test]
        fn top_p_regression_preserves_multiple_candidates_instead_of_greedy() {
            let logits = [1.0, 8.0, 7.9, 7.8];
            assert_eq!(candidate_ids(&logits, &cfg(1.0, 40, 0.9)), vec![1, 2, 3]);
        }
    }

    /// The two boundaries that belong to the *caller* stay at zero here.
    ///
    /// Reading the logits out of llama.cpp and turning a token id into text are
    /// measured by `infer_server`, on either side of this call. If the sampler
    /// ever started reporting them, a cost would be counted twice.
    /// The capture target and buffer are process-global — the module doc
    /// explains why that is sound in production, where one engine exists per
    /// process. Tests run in parallel threads, so two engines would exist at
    /// once and whichever loaded last would decide what the other captures.
    /// This lock restores the one-engine-at-a-time property the design assumes.
    static ENGINE_LOCK: Mutex<()> = Mutex::new(());

    /// How many layers the tests offload. Real peers run with everything on the
    /// GPU, and the capture behaves differently there, so this has to be a knob.
    fn test_gpu_layers() -> i32 {
        std::env::var("POM_TEST_GPU_LAYERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    fn mtp_attach_refuses_target_without_loaded_nextn(max_sessions: u32) {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("POM_MTP_MODEL") else {
            eprintln!("skipping: set POM_MTP_MODEL to a .gguf to run this");
            return;
        };
        let target_cfg = EngineConfig {
            n_ctx: 4096,
            offload_kqv: false,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: false,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 4,
            n_gpu_layers: 0,
            n_threads: 2,
            max_sessions,
            need_embeddings: false,
            mtp: false,
            mtp_cpu_fallback_enabled: true,
            rs_seq_window: 0,
            moe_external_executor: false,
            moe_external_executor_layers: Vec::new(),
        };
        let target_plan = empty_fit_plan_with_mtp_mode(&target_cfg, MtpFitMode::Disabled);
        let target_loaded = LoadedModel::load_with_mtp(&path, &target_plan, false)
            .expect("o alvo sem pesos nextn deve carregar");
        let target = LlmEngine::with_model(target_loaded.clone(), &target_cfg)
            .expect("o contexto principal deve carregar");

        let mtp_cfg = EngineConfig {
            mtp: true,
            ..target_cfg
        };
        drop(target);
        let engine = LlmEngine::with_model(target_loaded, &mtp_cfg)
            .expect("o alvo sem nextn deve continuar servindo sem abortar");
        assert!(!engine.mtp_ready());
        assert_eq!(engine.mtp_unavailable_reason(), Some(MTP_NO_NEXTN_REASON));

        let direct = crate::mtp::MtpDrafter::attach(
            &engine.loaded,
            engine.ctx,
            &mtp_cfg,
            engine.n_embd_out,
            engine.n_vocab,
            None,
            false,
        )
        .expect("a guarda interna deve recusar sem abortar");
        assert!(direct.is_none());
    }

    #[test]
    #[ignore = "requires a real Qwen3.5 MTP GGUF"]
    fn mtp_attach_on_target_without_nextn_is_refused_for_one_session() {
        mtp_attach_refuses_target_without_loaded_nextn(1);
    }

    #[test]
    #[ignore = "requires a real Qwen3.5 MTP GGUF"]
    fn mtp_attach_on_target_without_nextn_is_refused_for_two_sessions() {
        mtp_attach_refuses_target_without_loaded_nextn(2);
    }

    /// Sonda ao vivo do caminho de recuo: um modelo irmão CPU-only deve deixar
    /// o contexto MTP na RAM nos próprios buffers. Ela não mede desempenho e
    /// fica ignorada porque requer um GGUF com cabeça MTP e um backend
    /// compilado.
    #[test]
    #[ignore]
    fn mtp_cpu_companion_keeps_nextn_context_in_host_ram() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("POM_MTP_MODEL") else {
            eprintln!("skipping: set POM_MTP_MODEL to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 4096,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: true,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 512,
            n_gpu_layers: DEFAULT_GPU_LAYERS,
            n_threads: 0,
            max_sessions: 1,
            need_embeddings: false,
            mtp: true,
            mtp_cpu_fallback_enabled: true,
            rs_seq_window: 0,
            moe_external_executor: false,
            moe_external_executor_layers: Vec::new(),
        };
        let mut target_cfg = cfg.clone();
        target_cfg.fit_memory = false;
        target_cfg.n_gpu_layers = test_gpu_layers();
        let target_plan = empty_fit_plan(&target_cfg);
        let loaded = LoadedModel::load_with_mtp(&path, &target_plan, false)
            .expect("modelo alvo sem pesos MTP carrega");
        let mtp_loaded = LoadedModel::load_mtp_cpu(&path, &target_cfg).expect("modelo MTP CPU");
        let engine = LlmEngine::from_loaded_with_mtp_offload(
            loaded,
            &target_cfg,
            target_plan.applied,
            target_cfg.n_gpu_layers,
            Some(mtp_loaded),
            false,
        )
        .expect("contexto MTP na RAM");

        assert!(engine.mtp_ready(), "o GGUF precisa trazer o bloco nextn");
        assert_eq!(engine.mtp_allocation(), Some("ram"));
    }

    /// `tokenize` sizes its buffer from a call with `n_tokens_max = 0`, whose
    /// documented answer is the NEGATIVE of the count. Reading that as an error
    /// made every prompt come back empty, and the head peer of the pipeline
    /// rejected chunk 0 with "não tem tokens" before the second peer was ever
    /// contacted. Only a real vocab exercises the contract, so the test points
    /// at a GGUF given by the environment and skips when there is none.
    ///
    /// The residual capture has to actually fire, and the tensor has to have one
    /// row per token. This is the regression test for the eval-callback
    /// contract: returning `false` in the compute phase does not mean "not my
    /// tensor", it breaks ggml's evaluation loop and abandons the rest of the
    /// graph. With that inverted, the forward pass stopped at the input
    /// embedding, `llama_decode` still returned success, and the peer answered
    /// "residual capture did not fire".
    ///
    /// The middle-peer path: a hidden state comes in, the residual must come
    /// out. This is the one the pipeline actually breaks on and the one the
    /// token test above cannot reach — `decode_hidden` builds a different batch
    /// (`batch.token = NULL`, data in `batch.embd`) and therefore a different
    /// graph. Set `POM_TEST_GPU_LAYERS=99` to run it with the weights
    /// offloaded, which is how every real peer runs it.
    #[test]
    fn residual_capture_fires_for_an_injected_hidden_state() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("POM_TEST_GGUF") else {
            eprintln!("skipping: set POM_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: false,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 512,
            n_gpu_layers: test_gpu_layers(),
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: true,
            mtp: false,
            mtp_cpu_fallback_enabled: true,
            rs_seq_window: 0,
            moe_external_executor: false,
            moe_external_executor_layers: Vec::new(),
        };
        let mut engine = LlmEngine::load_with_cache(&path, &cfg, &ModelCache::new(), false)
            .expect("model loads");

        let n_tokens = 4;
        // Values in the range a real residual carries; zeros would let a graph
        // that never ran look identical to one that did.
        let hidden: Vec<f32> = (0..n_tokens * engine.n_embd)
            .map(|i| ((i % 97) as f32 - 48.0) / 48.0)
            .collect();
        engine
            .decode_hidden(
                &hidden,
                &vec![n_tokens, engine.n_embd],
                n_tokens,
                0,
                0,
                true,
                None,
            )
            .expect("decode of an injected hidden state succeeds");

        let residual = engine
            .take_residual(n_tokens)
            .expect("the capture callback must fire on the hidden-state path too");
        assert_eq!(residual.values.len(), n_tokens * engine.n_embd);
        assert!(
            residual.values.iter().any(|v| *v != 0.0 && v.is_finite()),
            "residual is all zeros or non-finite — the graph did not run"
        );
    }

    /// Prova E2E do contrato que falhou em produção: o mesmo hidden e o mesmo
    /// GGUF passam pelo bloco nativo e pelo callback externo CUDA, e a captura
    /// é feita exatamente depois da camada 1. Além de comparar o residual,
    /// exige que toda chamada observada tenha usado CUDA. Assim um callback que
    /// leia a rota com passo errado ou uma área ainda não materializada falha
    /// antes de poder parecer uma divergência de quantização.
    ///
    /// ```text
    /// POM_MOE_NUMERIC_MODEL=/caminho/qwen35moe.gguf \
    /// POM_MOE_NUMERIC_GPU_LAYERS=99 \
    /// cargo test -p node --features cuda --bin pom \
    ///   inference::llm::pool_tests::qwen35moe_external_route_matches_native_at_layer \
    ///   -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requer GGUF qwen35moe e CUDA real"]
    fn qwen35moe_external_route_matches_native_at_layer() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(cfg!(feature = "cuda"), "rode com --features cuda");

        let path = std::env::var("POM_MOE_NUMERIC_MODEL")
            .expect("POM_MOE_NUMERIC_MODEL deve apontar para um GGUF qwen35moe");
        let info = crate::gguf::read_info(std::path::Path::new(&path))
            .expect("o cabeçalho GGUF deve ser legível");
        assert_eq!(
            info.architecture.as_deref(),
            Some("qwen35moe"),
            "o teste é específico para a rota do qwen35moe"
        );
        assert_eq!(
            crate::moe::format_q4_0::detect_expert_format(&path)
                .expect("o formato dos experts deve ser detectável"),
            crate::moe::format_q4_0::ExpertFormat::Heterogeneous,
            "o Ornith deve passar pelo inventário heterogêneo Q4_K/Q6_K"
        );
        let expert_dtypes = crate::moe::format_q4_0::detect_expert_dtypes(&path)
            .expect("os dtypes dos experts devem ser detectáveis");
        assert!(
            expert_dtypes.contains(&12) && expert_dtypes.contains(&14),
            "a prova precisa incluir Q4_K e Q6_K: {expert_dtypes:?}"
        );
        let experts = info
            .expert_count
            .expect("qwen35moe deve declarar expert_count");
        let top_k = info.expert_used_count.unwrap_or(1).max(1) as usize;
        let layer = std::env::var("POM_MOE_NUMERIC_LAYER")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(1);
        let gpu_layers = std::env::var("POM_MOE_NUMERIC_GPU_LAYERS")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(99);
        let plan = crate::moe::runtime::MoeRuntime::external_executor_plan(&path)
            .expect("o planejamento de ownership MoE deve funcionar");
        assert_eq!(
            plan.layers.get(layer as usize).copied(),
            Some(1),
            "a camada {layer} não tem executor externo CUDA: {:?}",
            plan.reason
        );

        let config = |external| EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: false,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 4,
            n_gpu_layers: gpu_layers,
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: true,
            mtp: false,
            mtp_cpu_fallback_enabled: true,
            rs_seq_window: 0,
            moe_external_executor: external,
            moe_external_executor_layers: if external {
                plan.layers.clone()
            } else {
                Vec::new()
            },
        };

        let mut native =
            LlmEngine::load_with_cache(&path, &config(false), &ModelCache::new(), false)
                .expect("o modelo nativo deve carregar");
        native
            .set_capture_layer(Some(layer))
            .expect("a camada de captura deve existir");
        let n_tokens = 4;
        let hidden: Vec<f32> = (0..n_tokens * native.n_embd)
            .map(|index| ((index % 127) as f32 - 63.0) / 63.0)
            .collect();
        native
            .decode_hidden(
                &hidden,
                &vec![n_tokens, native.n_embd],
                n_tokens,
                0,
                0,
                true,
                None,
            )
            .expect("o caminho nativo deve decodificar");
        let native_residual = native
            .take_residual(n_tokens)
            .expect("o residual nativo deve ser capturado");
        drop(native);

        let mut external =
            LlmEngine::load_with_cache(&path, &config(true), &ModelCache::new(), false)
                .expect("o modelo em modo metadata-only deve carregar");
        external
            .set_capture_layer(Some(layer))
            .expect("a camada de captura deve existir");
        let weights_bytes = std::fs::metadata(&path).expect("o GGUF deve existir").len();
        let cuda_compute_bytes = external
            .cuda_compute_bytes()
            .expect("o contexto deve expor um compute buffer CUDA medido");
        let runtime = std::sync::Arc::new(
            crate::moe::runtime::MoeRuntime::open(
                &path,
                "qwen35moe-numeric-proof",
                experts,
                top_k,
                weights_bytes,
                Some(cuda_compute_bytes),
            )
            .expect("o runtime externo deve abrir os experts roteados"),
        );
        external
            .set_moe_runtime(runtime)
            .expect("o callback MoE deve instalar");
        let timings = external
            .decode_hidden(
                &hidden,
                &vec![n_tokens, external.n_embd],
                n_tokens,
                0,
                0,
                true,
                None,
            )
            .expect("a rota externa deve decodificar sem NaN");
        let external_residual = external
            .take_residual(n_tokens)
            .expect("o residual externo deve ser capturado");

        assert!(timings.moe_forward_us > 0, "o callback MoE não foi medido");
        assert!(
            !timings.moe_executors.is_empty(),
            "nenhum callback MoE foi observado"
        );
        assert!(
            timings
                .moe_executors
                .iter()
                .all(|executor| *executor == crate::moe::runtime::MoeExecutorKind::Cuda),
            "o teste precisa provar execução CUDA: {:?}",
            timings.moe_executors
        );
        assert!(
            external_residual
                .values
                .iter()
                .all(|value| value.is_finite()),
            "o residual externo contém valor não finito"
        );
        assert_eq!(native_residual.values.len(), external_residual.values.len());

        let mut max_abs = 0.0f32;
        let mut max_relative = 0.0f32;
        let mut max_index = 0usize;
        for (index, (&expected, &actual)) in native_residual
            .values
            .iter()
            .zip(&external_residual.values)
            .enumerate()
        {
            assert!(
                expected.is_finite(),
                "residual nativo não finito no índice {index}"
            );
            let absolute = (actual - expected).abs();
            let relative = absolute / expected.abs().max(1e-3);
            if absolute > max_abs {
                max_abs = absolute;
                max_index = index;
            }
            max_relative = max_relative.max(relative);
            assert!(
                absolute <= 5e-2 + 2e-2 * expected.abs().max(1.0),
                "divergência no residual índice {index}: nativo={expected:?}, externo={actual:?}, abs={absolute:?}, rel={relative:?}"
            );
        }
        println!(
            "MOE_NUMERIC_PROOF layer={layer} tokens={n_tokens} experts={experts} top_k={top_k} max_abs={max_abs:.6e} max_relative={max_relative:.6e} max_index={max_index} executors={:?}",
            timings.moe_executors
        );
    }

    #[test]
    fn residual_capture_fires_with_one_row_per_token() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("POM_TEST_GGUF") else {
            eprintln!("skipping: set POM_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: false,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 512,
            n_gpu_layers: test_gpu_layers(),
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: true,
            mtp: false,
            mtp_cpu_fallback_enabled: true,
            rs_seq_window: 0,
            moe_external_executor: false,
            moe_external_executor_layers: Vec::new(),
        };
        let mut engine = LlmEngine::load_with_cache(&path, &cfg, &ModelCache::new(), false)
            .expect("model loads");
        let toks = engine.tokenize("Explique inferencia distribuida em uma frase.", true, true);
        assert!(!toks.is_empty(), "prompt must tokenize");
        engine
            .decode_tokens(&toks, 0, 0, true)
            .expect("decode succeeds");

        let residual = engine
            .take_residual(toks.len())
            .expect("the capture callback must have fired");
        assert_eq!(residual.values.len(), toks.len() * engine.n_embd);
        // An abandoned graph leaves the buffer zeroed; a real residual does not.
        assert!(
            residual.values.iter().any(|v| *v != 0.0 && v.is_finite()),
            "residual is all zeros or non-finite — the graph did not run"
        );
    }

    /// Mudança 6 do spec de 2026-08-12: `node_logits_access_ms` marcava ~4,9 ms
    /// por token no nó da LM head, com `backend_completion_ms` em zero quando o
    /// `llama_synchronize` de diagnóstico está desligado pela configuração. A
    /// hipótese é que esses milissegundos são a GPU terminando o trabalho,
    /// cobrados na etapa errada - o primeiro toque de CPU nos logits é quem
    /// espera.
    ///
    /// Este teste decide isso sem precisar de duas máquinas: decodifica um
    /// token na mesma fatia duas vezes, uma com `POM_PROFILE_SYNC`
    /// desligado e outra com ele ligado, e imprime para onde o tempo foi.
    ///
    /// ```text
    /// POM_TEST_GGUF=<fatia final>.gguf POM_TEST_GPU_LAYERS=99 \
    ///   cargo test -p node --release --features metal logits_access_versus -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn logits_access_versus_backend_completion() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("POM_TEST_GGUF") else {
            eprintln!("skipping: set POM_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: false,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 512,
            n_gpu_layers: test_gpu_layers(),
            n_threads: 0,
            max_sessions: 1,
            // O nó da LM head não exporta residual — é o ponto da Mudança 5.
            need_embeddings: false,
            mtp: false,
            mtp_cpu_fallback_enabled: true,
            rs_seq_window: 0,
            moe_external_executor: false,
            moe_external_executor_layers: Vec::new(),
        };
        let mut engine = LlmEngine::load_with_cache(&path, &cfg, &ModelCache::new(), false)
            .expect("model loads");
        let tokens = engine.tokenize(
            "Explique o que e inferencia distribuida em uma frase.",
            true,
            true,
        );

        for sync in [false, true, false, true] {
            std::env::set_var("POM_PROFILE_SYNC", if sync { "1" } else { "0" });
            // Cache limpo entre as rodadas; o `false` aqui seria o cache que
            // não desloca, e este teste não depende disso.
            let _ = engine.kv_remove(0, 0, -1);
            let mut pos = 0u32;
            engine
                .decode_tokens(&tokens, pos, 0, false)
                .expect("prefill decodes");
            let _ = engine.logits_ith(tokens.len() as i32 - 1)[0];
            pos += tokens.len() as u32;

            let mut access_us = 0u64;
            let mut completion_us = 0u64;
            let mut enqueue_us = 0u64;
            let reps = 16;
            let wall_started = Instant::now();
            for _ in 0..reps {
                let t = engine.decode_tokens(&[1i32], pos, 0, false).unwrap();
                let started = Instant::now();
                let logits = engine.logits_ith(0);
                let _touch = logits[0] + logits[logits.len() - 1];
                access_us += started.elapsed().as_micros() as u64;
                completion_us += t.backend_completion_us;
                enqueue_us += t.enqueue_us;
                pos += 1;
            }
            let wall = wall_started.elapsed().as_secs_f64() * 1000.0 / reps as f64;
            println!(
                "PROFILE_SYNC={}: TOTAL {wall:.2} ms/token | enfileirar {:.2} ms | conclusão {:.2} ms | acesso aos logits {:.2} ms",
                sync as u8,
                enqueue_us as f64 / reps as f64 / 1000.0,
                completion_us as f64 / reps as f64 / 1000.0,
                access_us as f64 / reps as f64 / 1000.0,
            );
        }
        std::env::remove_var("POM_PROFILE_SYNC");
    }

    #[test]
    fn tokenize_returns_tokens_for_a_real_vocab() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("POM_TEST_GGUF") else {
            eprintln!("skipping: set POM_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: false,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 512,
            n_gpu_layers: 0,
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: false,
            mtp: false,
            mtp_cpu_fallback_enabled: true,
            rs_seq_window: 0,
            moe_external_executor: false,
            moe_external_executor_layers: Vec::new(),
        };
        let engine = LlmEngine::load_with_cache(&path, &cfg, &ModelCache::new(), false)
            .expect("model loads");
        let tokens = engine.tokenize("oi", true, true);
        assert!(!tokens.is_empty(), "a two-character prompt must tokenize");
        assert!(
            engine
                .tokenize(
                    "Explique o que e inferencia distribuida em uma frase.",
                    true,
                    true
                )
                .len()
                > tokens.len(),
            "a longer prompt must produce more tokens"
        );
    }

    #[test]
    fn chat_template_wraps_the_user_prompt_and_opens_assistant_turn() {
        let _guard = ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(path) = std::env::var("POM_TEST_GGUF") else {
            eprintln!("skipping: set POM_TEST_GGUF to a .gguf to run this");
            return;
        };
        let cfg = EngineConfig {
            n_ctx: 512,
            offload_kqv: true,
            cache_type_k: KvCacheType::F16,
            cache_type_v: KvCacheType::F16,
            fit_memory: false,
            fit_memory_margin_mb: crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB,
            n_batch: 512,
            n_gpu_layers: 0,
            n_threads: 2,
            max_sessions: 1,
            need_embeddings: false,
            mtp: false,
            mtp_cpu_fallback_enabled: true,
            rs_seq_window: 0,
            moe_external_executor: false,
            moe_external_executor_layers: Vec::new(),
        };
        let engine = LlmEngine::load_with_cache(&path, &cfg, &ModelCache::new(), false)
            .expect("model loads");
        let formatted = engine
            .apply_chat_template(&[("user", "Oi, teste")], true, false)
            .expect("chat template must render");
        assert!(formatted.contains("Oi, teste"));
        assert!(!formatted.starts_with("Oi, teste"));
        assert!(formatted.to_ascii_lowercase().contains("assistant"));
        assert!(
            engine.tokenize(&formatted, false, true).len()
                > engine.tokenize("Oi, teste", true, true).len()
        );
    }

    #[test]
    fn gemma4_fallback_usa_turnos_e_token_de_thinking_do_modelo() {
        let formatted = render_gemma4_template(
            GEMMA4_TEMPLATE_FIXTURE,
            "<bos>",
            &[("user", "Oi, teste")],
            true,
            true,
        )
        .unwrap();

        assert!(formatted.starts_with("<bos><|turn>user\n"));
        assert!(formatted.contains("<|turn>user\nOi, teste<turn|>\n"));
        assert!(formatted.ends_with("<|turn>model\n<|channel>thought\n<channel|>"));
        assert!(formatted.contains("<|channel>thought\n<channel|>"));
        assert!(!formatted.contains("<|im_start|>"));
        assert!(!formatted.contains("<|im_end|>"));
        assert!(!formatted.contains("<think>"));
    }

    #[test]
    fn gemma4_fallback_preserva_o_inicio_de_thinking_quando_habilitado() {
        let formatted = render_gemma4_template(
            GEMMA4_TEMPLATE_FIXTURE,
            "<bos>",
            &[("user", "Oi")],
            true,
            false,
        )
        .unwrap();

        assert!(formatted.contains("<|turn>system\n<|think|>\n<turn|>\n"));
        assert!(formatted.ends_with("<|turn>model\n"));
        assert!(!formatted.contains("<|channel>thought\n<channel|>"));
    }

    #[test]
    fn gemma4_renderer_honors_add_generation_prompt_from_the_gguf_template() {
        let formatted = render_gemma4_template(
            GEMMA4_TEMPLATE_FIXTURE,
            "<bos>",
            &[("user", "Oi")],
            false,
            true,
        )
        .unwrap();

        assert_eq!(formatted, "<bos><|turn>user\nOi<turn|>\n");
    }

    #[test]
    fn generation_stop_matches_a_complete_turn_marker() {
        assert!(token_sequence_ends_with(&[12, 106], &[106]));
        assert!(!token_sequence_ends_with(&[12, 105], &[106]));
    }
}

#[cfg(test)]
mod utf8_assembler_tests {
    use super::*;

    /// Um token pode terminar no meio de um codepoint (byte-fallback) e o
    /// pedaço seguinte completa a sequência. Converter cada chamada com
    /// `from_utf8_lossy` nascia U+FFFD no meio da palavra - este é o
    /// contrato de regressão: só caractere completo sai, e o rabinho fica.
    #[test]
    fn assembler_emite_so_caractere_completo_e_guarda_o_rabicho() {
        let mut asm = Utf8Assembler::new();
        // "ão" = c3 a3, quebrado entre dois tokens depois de "Jo".
        assert_eq!(asm.feed(b"Jo\xc3"), "Jo");
        assert_eq!(asm.feed(b"\xa3o "), "\u{e3}o ");
        // Emoji de 4 bytes (🧠 = f0 9f a7 a0) quebrado em três feeds.
        assert_eq!(asm.feed(&[0xf0]), "");
        assert_eq!(asm.feed(&[0x9f, 0xa7]), "");
        assert_eq!(asm.feed(&[0xa0, b'!']), "\u{1f9e0}!");
        assert_eq!(asm.flush(), "");
    }

    #[test]
    fn assembler_passa_ascii_inteiro_numa_so_chamada() {
        let mut asm = Utf8Assembler::new();
        assert_eq!(
            asm.feed(b"Ol\xc3\xa1, a\xc3\xa7\xc3\xa3o, cora\xc3\xa7\xc3\xa3o"),
            "Ol\u{e1}, a\u{e7}\u{e3}o, cora\u{e7}\u{e3}o"
        );
    }

    /// Byte inválido de verdade (não é prefixo de nada): vira um U+FFFD na
    /// hora e o stream continua - mesma semântica do from_utf8_lossy global,
    /// sem esperar bytes que nunca vão completar sequência alguma.
    #[test]
    fn assembler_repassa_byte_invalido_como_um_fffd_e_segue() {
        let mut asm = Utf8Assembler::new();
        assert_eq!(asm.feed(b"a\xffb"), "a\u{fffd}b");
    }

    /// Fim do stream com sequência truncada: o flush drena o resto como um
    /// único replacement, e chamar de novo devolve vazio.
    #[test]
    fn flush_drena_rabicho_truncado_uma_vez() {
        let mut asm = Utf8Assembler::new();
        assert_eq!(asm.feed(b"ok\xc3"), "ok");
        assert_eq!(asm.flush(), "\u{fffd}");
        assert_eq!(asm.flush(), "");
    }
}
