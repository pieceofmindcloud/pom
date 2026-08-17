//! Registro do callback de tensor parallelism no llama.cpp (caminho C).
//!
//! O fork do llama.cpp insere um op de reduce (GGML_OP_CUSTOM) depois das
//! projeções row-parallel (wo e ffn_down). O op chama a llama_tp_reduce_fun,
//! que copia a parcial e invoca este callback para somar as parciais do grupo.
//!
//! O callback roda num thread de blocking durante o llama_decode. O transporte
//! é uniforme: TODO rank (inclusive o 0) faz um POST da própria parcial para o
//! primary (rank 0), cujo endpoint /v1/tp/reduce acumula via ReduceBarrier e
//! devolve a soma. A parcial é f32 (a precisão da GEMM).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

#[derive(serde::Serialize)]
struct TpReduceRequest {
    barrier_id: String,
    rank: u32,
    data: Vec<f32>,
}

#[derive(serde::Deserialize)]
struct TpReduceResponse {
    sum: Vec<f32>,
}

struct TpState {
    rank: u32,
    /// URL do primary (rank 0) da coletiva. Todo rank POSTa para cá.
    rank0_url: String,
    /// Contador de reduces. Em execução lockstep, o k-ésimo reduce de todos os
    /// ranks corresponde à mesma projeção, então o id casa entre os processos.
    counter: AtomicU64,
}

static TP_STATE: OnceLock<Arc<TpState>> = OnceLock::new();

/// Liga a coletiva TP deste processo e registra o callback no llama.cpp.
///
/// Chamado pelo apply quando o nó é primary/worker de um grupo (size > 1).
/// V1: o estado é fixado uma vez por processo; trocar o grupo exige reiniciar.
pub fn register_tp_reduce(rank: u32, rank0_url: String) {
    let state = Arc::new(TpState {
        rank,
        rank0_url,
        counter: AtomicU64::new(0),
    });
    let _ = TP_STATE.set(state);
    unsafe {
        llama_cpp_sys_2::llama_set_tp_reduce_callback(Some(tp_reduce_cb), std::ptr::null_mut());
    }
}

/// Desliga a coletiva: o callback vira None e o fork deixa de inserir o op.
pub fn unregister_tp_reduce() {
    unsafe {
        llama_cpp_sys_2::llama_set_tp_reduce_callback(None, std::ptr::null_mut());
    }
}

/// O callback do op de reduce: lê a parcial (f32), POSTa para o primary e
/// devolve a soma no mesmo buffer.
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
    let sum = tokio::runtime::Handle::current()
        .block_on(post_reduce(state, &barrier_id, partial));
    if let Ok(sum) = sum {
        unsafe {
            std::ptr::copy_nonoverlapping(sum.as_ptr(), data as *mut f32, n);
        }
    }
}

async fn post_reduce(
    state: &Arc<TpState>,
    barrier_id: &str,
    data: Vec<f32>,
) -> Result<Vec<f32>, String> {
    let resp = http_client()
        .post(format!("{}/v1/tp/reduce", state.rank0_url))
        .json(&TpReduceRequest {
            barrier_id: barrier_id.to_string(),
            rank: state.rank,
            data,
        })
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("POST /v1/tp/reduce respondeu {}", resp.status()));
    }
    let body: TpReduceResponse = resp.json().await.map_err(|e| e.to_string())?;
    Ok(body.sum)
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collective::ReduceBarrier;
    use axum::{extract::State, routing::post, Json, Router};
    use serde_json::json;

    #[derive(Clone)]
    struct TestState {
        barrier: ReduceBarrier,
        size: u32,
    }

    async fn handle(State(s): State<TestState>, Json(req): Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
        let barrier_id = req["barrier_id"].as_str().unwrap().to_string();
        let rank = req["rank"].as_u64().unwrap() as u32;
        let data: Vec<f32> = req["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        let rx = s.barrier.contribute(&barrier_id, rank, s.size, data).unwrap();
        let sum = rx.await.unwrap();
        axum::Json(json!({ "sum": sum }))
    }

    /// A coletiva distribuída soma as parciais de 3 ranks concorrentes por
    /// HTTP - o mesmo contrato do POST /v1/tp/reduce do primary.
    #[tokio::test]
    async fn three_ranks_reduce_over_http() {
        let app = Router::new()
            .route("/v1/tp/reduce", post(handle))
            .with_state(TestState {
                barrier: ReduceBarrier::new(),
                size: 3,
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let url = format!("http://{addr}");
        let partials = vec![vec![1.0, 2.0], vec![3.0, 4.0], vec![5.0, 6.0]];
        let expected = vec![9.0, 12.0];
        let client = reqwest::Client::new();

        let handles: Vec<_> = partials
            .into_iter()
            .enumerate()
            .map(|(rank, data)| {
                let client = client.clone();
                let url = url.clone();
                tokio::spawn(async move {
                    let resp = client
                        .post(format!("{url}/v1/tp/reduce"))
                        .json(&json!({ "barrier_id": "b", "rank": rank, "data": data }))
                        .send()
                        .await
                        .unwrap();
                    let body: serde_json::Value = resp.json().await.unwrap();
                    let sum: Vec<f32> = body["sum"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_f64().unwrap() as f32)
                        .collect();
                    sum
                })
            })
            .collect();

        for h in handles {
            assert_eq!(h.await.unwrap(), expected);
        }
    }
}
