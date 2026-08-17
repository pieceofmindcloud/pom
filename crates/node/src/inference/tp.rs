//! Registro do callback de tensor parallelism no llama.cpp (caminho C).
//!
//! O fork do llama.cpp insere um op de reduce (GGML_OP_CUSTOM) depois das
//! projeções row-parallel (wo e ffn_down). O op chama a llama_tp_reduce_fun,
//! que copia a parcial e invoca este callback para somar as parciais do grupo.
//!
//! O callback roda num thread de blocking durante o llama_decode, então usa a
//! BlockingReduceBarrier (síncrona). A parcial é interpretada como f32 (a
//! precisão da GEMM).

use crate::collective::BlockingReduceBarrier;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

struct TpState {
    rank: u32,
    size: u32,
    barrier: BlockingReduceBarrier,
    /// Contador de reduces. Em execução lockstep, o k-ésimo reduce de todos os
    /// ranks corresponde à mesma projeção, então o id casa entre os processos.
    counter: AtomicU64,
}

static TP_STATE: OnceLock<Arc<TpState>> = OnceLock::new();

/// Liga a coletiva TP deste processo e registra o callback no llama.cpp.
///
/// Chamado pelo apply quando o nó é primary/worker de um grupo (size > 1).
/// Com size <= 1 o callback devolve a parcial intacta (identidade). V1: o
/// estado é fixado uma vez por processo; trocar o grupo exige reiniciar.
pub fn register_tp_reduce(rank: u32, size: u32) {
    let state = Arc::new(TpState {
        rank,
        size,
        barrier: BlockingReduceBarrier::new(),
        counter: AtomicU64::new(0),
    });
    let _ = TP_STATE.set(state);
    unsafe {
        llama_cpp_sys_2::llama_set_tp_reduce_callback(Some(tp_reduce_cb), std::ptr::null_mut());
    }
}

/// Desliga a coletiva: o callback vira None e o fork deixa de inserir o op de
/// reduce. Útil para teste (não vazar o estado global) e para reconfigurar.
pub fn unregister_tp_reduce() {
    unsafe {
        llama_cpp_sys_2::llama_set_tp_reduce_callback(None, std::ptr::null_mut());
    }
}

/// O callback do op de reduce: lê a parcial (f32), soma com a dos demais ranks
/// e devolve a soma no mesmo buffer.
unsafe extern "C" fn tp_reduce_cb(data: *mut c_void, n_bytes: usize, _userdata: *mut c_void) {
    if data.is_null() || n_bytes == 0 || n_bytes % 4 != 0 {
        return;
    }
    let Some(state) = TP_STATE.get() else {
        return;
    };
    let n = n_bytes / 4;
    let partial = unsafe { std::slice::from_raw_parts(data as *const f32, n).to_vec() };
    let barrier_id = state.counter.fetch_add(1, Ordering::Relaxed).to_string();
    if let Ok(sum) = state.barrier.reduce(&barrier_id, state.rank, state.size, &partial) {
        unsafe {
            std::ptr::copy_nonoverlapping(sum.as_ptr(), data as *mut f32, n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Com um rank só (size=1), o reduce é identidade: a parcial volta igual.
    #[test]
    fn a_single_rank_reduce_is_identity() {
        register_tp_reduce(0, 1);
        let mut buf: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        unsafe {
            tp_reduce_cb(
                buf.as_mut_ptr() as *mut c_void,
                buf.len() * std::mem::size_of::<f32>(),
                std::ptr::null_mut(),
            );
        }
        assert_eq!(buf, vec![1.0, 2.0, 3.0, 4.0]);
        unregister_tp_reduce();
    }
}
