//! Registro do callback de tensor parallelism no llama.cpp (caminho C).
//!
//! O fork do llama.cpp insere um op de reduce (GGML_OP_CUSTOM) depois das
//! projeções row-parallel (wo e ffn_down). O op chama a llama_tp_reduce_fun,
//! que copia a parcial e invoca este callback para somar as parciais do grupo.
//!
//! O callback roda num thread de blocking durante o llama_decode. Os workers
//! fazem um POST da própria parcial para o primary (rank 0), cujo endpoint
//! `/v1/tp/reduce` acumula via `ReduceBarrier`. O próprio primary contribui
//! diretamente na barreira local, sem discar a própria URL pública. A parcial
//! é f32 (a precisão da GEMM).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::collective::ReduceBarrier;
use crate::relay::ByteTransport;

/// Cabeçalhos da coletiva. O corpo é a parcial em f32 cru; metadado de tamanho
/// fixo vai em cabeçalho, e não misturado ao vetor.
pub const TP_BARRIER_HEADER: &str = "x-pom-tp-barrier";
pub const TP_RANK_HEADER: &str = "x-pom-tp-rank";

struct TpState {
    rank: u32,
    size: u32,
    /// URL do primary (rank 0) da coletiva. Os workers POSTam para cá; o rank
    /// 0 usa a barreira local e não disca a própria URL.
    rank0_url: String,
    /// Candidatos alternativos do primary, escolhidos por quem fez o deploy.
    /// O primeiro é `rank0_url`; os demais cobrem NAT, LAN e containers no
    /// mesmo host quando só um deles é alcançável deste rank.
    rank0_urls: Vec<String>,
    /// Endereços UDP anunciados pelo primary, para o túnel reconstruir as
    /// multiaddresses TCP sem depender da tabela de descoberta deste rank.
    rank0_addr_candidates: Vec<String>,
    /// PeerId e capacidade do túnel do primary, para redes sem rota HTTPS
    /// direta entre as máquinas.
    rank0_peer_id: Option<String>,
    rank0_supports_tunnel: bool,
    /// Cliente **pinado no `node_id` do primary**, montado uma vez no registro.
    ///
    /// A coletiva usava `reqwest::Client::new()`, que valida o certificado pela
    /// cadeia pública; o do cluster é autoassinado e verificado por
    /// `SHA256(pubkey) == node_id`. Com TLS ligado, todo POST morria em 13 ms
    /// (medido na ksia, 18/08) e o all-reduce nunca acontecia.
    client: reqwest::Client,
    /// Configuração viva usada para obter a credencial interna. O receptor
    /// também aceita API keys ativas, mas os nós usam a connection_key.
    auth: Option<crate::auth::AuthState>,
    /// Runtime que hospeda o servidor de inferência. O callback do llama.cpp
    /// roda numa thread `spawn_blocking`, então `Handle::current()` não existe
    /// ali; o handle precisa ser capturado no registro do grupo TP.
    runtime: Option<tokio::runtime::Handle>,
    /// Contador de reduces. Em execução lockstep, o k-ésimo reduce de todos os
    /// ranks corresponde à mesma projeção, então o id casa entre os processos.
    counter: AtomicU64,
    /// Barreira local pertencente ao servidor que registrou este estado TP.
    /// Mantê-la no estado, e não num único global do processo, evita que uma
    /// reconfiguração troque a barreira de um decode antigo ainda drenando.
    local_barrier: std::sync::RwLock<Option<(ReduceBarrier, u32, u64)>>,
}

/// Estado da coletiva deste processo. `None` = este nó não é de um grupo TP.
///
/// Era um `OnceLock`, que **não tem como ser limpo**: um processo que um dia
/// serviu um grupo TP continuava se declarando TP para sempre. Implantar um
/// modelo inteiro por cima de um grupo herdava esse estado, e a checagem de
/// "nenhum reduce neste decode" chegava a derrubar requisições legítimas de um
/// modelo que nunca teve coletiva nenhuma (18/08). Trocar o grupo também
/// exigia reiniciar o processo, o que era limitação do tipo, não do desenho.
static TP_STATE: std::sync::RwLock<Option<Arc<TpState>>> = std::sync::RwLock::new(None);

static NEXT_LOCAL_BARRIER_TOKEN: AtomicU64 = AtomicU64::new(1);

pub(crate) struct LocalBarrierHandle {
    state: Arc<TpState>,
    token: u64,
}

pub(crate) fn set_local_barrier(barrier: ReduceBarrier, size: u32) -> Option<LocalBarrierHandle> {
    let state = tp_state()?;
    let token = NEXT_LOCAL_BARRIER_TOKEN.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut slot) = state.local_barrier.write() {
        *slot = Some((barrier, size, token));
    } else {
        return None;
    }
    Some(LocalBarrierHandle { state, token })
}

pub(crate) fn clear_local_barrier(handle: LocalBarrierHandle) {
    if let Ok(mut slot) = handle.state.local_barrier.write() {
        if slot
            .as_ref()
            .is_some_and(|(_, _, current)| *current == handle.token)
        {
            *slot = None;
        }
    }
}

fn local_barrier_for(state: &Arc<TpState>) -> Option<(ReduceBarrier, u32)> {
    if state.rank != 0 {
        return None;
    }
    state.local_barrier.read().ok().and_then(|slot| {
        slot.as_ref()
            .map(|(barrier, size, _)| (barrier.clone(), *size))
    })
}

fn tp_state() -> Option<Arc<TpState>> {
    TP_STATE.read().ok().and_then(|s| s.clone())
}

/// Última falha da coletiva, para o decode transformar em erro da requisição.
///
/// Um all-reduce que falha **não degrada a qualidade da resposta: ele produz
/// números errados**. O rank fica com a própria parcial - metade das cabeças e
/// metade do FFN - e a geração sai sintaticamente plausível e semanticamente
/// lixo (`oI, TESTE` seguido de `ape ilib ion perc`, 18/08). Por isso a falha
/// é registrada aqui e derruba a requisição, em vez de virar texto.
static TP_FAILURE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Consome a falha pendente da coletiva, se houver.
///
/// Chamado pelo decode: enquanto isto devolver `Some`, a resposta em curso está
/// contaminada e não pode ser entregue.
pub fn take_failure() -> Option<String> {
    TP_FAILURE.lock().ok().and_then(|mut f| f.take())
}

/// Decode em curso: `(id, próxima sequência de reduce)`.
///
/// O `barrier_id` **tem que ser o mesmo em todos os ranks** para as parciais se
/// encontrarem. Ele era um contador por processo, o que só casa enquanto os
/// processos estiverem perfeitamente alinhados desde o arranque: um worker que
/// reinicia zera o contador e o grupo nunca mais se encontra - erro que não dá
/// erro, só trava. Agora o rank 0 gera o id do decode, manda junto do lote, e
/// os dois lados numeram os reduces daquele decode a partir de zero.
///
/// Global porque o `llama_context` não é seguro para uso concorrente e já vive
/// atrás de um mutex: há no máximo um decode por processo de cada vez.
static DECODE: std::sync::Mutex<Option<(String, u64)>> = std::sync::Mutex::new(None);

#[derive(Debug, Clone, serde::Serialize)]
pub struct TpDecodeTelemetry {
    pub enabled: bool,
    pub rank: u32,
    pub size: u32,
    pub expected_reduces: u64,
    pub reduces: u64,
    pub wait_ms: u64,
    pub reduce_p50_ms: u64,
    pub reduce_p95_ms: u64,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub local_calls: u64,
    pub direct_calls: u64,
    pub tunnel_calls: u64,
    pub fallback_calls: u64,
    pub broadcast_calls: u64,
    pub broadcast_bytes: u64,
    #[serde(skip)]
    samples_ms: Vec<u64>,
}

static DECODE_TELEMETRY: std::sync::Mutex<Option<TpDecodeTelemetry>> = std::sync::Mutex::new(None);

fn percentile(samples: &[u64], percent: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() - 1) * percent + 50) / 100;
    sorted[index]
}

fn telemetry_snapshot(current: &TpDecodeTelemetry) -> TpDecodeTelemetry {
    let mut snapshot = current.clone();
    snapshot.reduce_p50_ms = percentile(&snapshot.samples_ms, 50);
    snapshot.reduce_p95_ms = percentile(&snapshot.samples_ms, 95);
    snapshot.samples_ms.clear();
    snapshot
}

/// Este processo está executando um decode como **réplica** (rank != 0)?
///
/// Uma réplica não tem opinião: ela existe para contribuir com a metade dela em
/// cada reduce. Quem amostra o token, decide se a geração acabou e comanda o
/// ciclo de vida da sessão é o rank 0 - a réplica segue.
///
/// Ignorar isso custou o 400 de 18/08: a amostragem é estocástica (temperatura,
/// top_p, top_k, com RNG independente em cada processo), o worker amostrou EOS
/// enquanto o rank 0 amostrava um token normal, deu a sessão por encerrada e
/// apagou. O step seguinte chegou nele e recebeu "sessão desconhecida".
pub fn is_replica() -> bool {
    REPLICA.load(Ordering::Relaxed)
}

static REPLICA: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Abre um decode como réplica: mesmo id do rank 0, e sem direito a decisão.
pub fn begin_replica_decode(id: &str) {
    REPLICA.store(true, Ordering::Relaxed);
    begin_decode_inner(id);
}

/// Abre um decode: os reduces seguintes pertencem a `id` e recomeçam do zero.
///
/// Chamado pelos handlers de `/v1/prefill` e `/v1/step` nos **dois** lados - o
/// rank 0 com o id que ele sorteou, o worker com o id que recebeu no cabeçalho.
pub fn begin_decode(id: &str) {
    REPLICA.store(false, Ordering::Relaxed);
    begin_decode_inner(id);
}

fn begin_decode_inner(id: &str) {
    if let Ok(mut d) = DECODE.lock() {
        *d = Some((id.to_string(), 0));
    }
    if let Ok(mut telemetry) = DECODE_TELEMETRY.lock() {
        *telemetry = tp_state().map(|state| TpDecodeTelemetry {
            enabled: true,
            rank: state.rank,
            size: state.size,
            expected_reduces: 0,
            reduces: 0,
            wait_ms: 0,
            reduce_p50_ms: 0,
            reduce_p95_ms: 0,
            request_bytes: 0,
            response_bytes: 0,
            local_calls: 0,
            direct_calls: 0,
            tunnel_calls: 0,
            fallback_calls: 0,
            broadcast_calls: 0,
            broadcast_bytes: 0,
            samples_ms: Vec::new(),
        });
    }
}

pub fn set_expected_reduces(expected: u64) {
    if let Ok(mut telemetry) = DECODE_TELEMETRY.lock() {
        if let Some(current) = telemetry.as_mut() {
            current.expected_reduces = expected;
        }
    }
}

pub fn record_broadcast_scheduled(calls: u64, bytes: u64) {
    if let Ok(mut telemetry) = DECODE_TELEMETRY.lock() {
        if let Some(current) = telemetry.as_mut() {
            current.broadcast_calls += calls;
            current.broadcast_bytes += calls * bytes;
        }
    }
}

pub fn record_reduce(
    _elements: usize,
    request_bytes: u64,
    response_bytes: u64,
    elapsed: std::time::Duration,
    transport: ByteTransport,
    fallback: bool,
) {
    if let Ok(mut telemetry) = DECODE_TELEMETRY.lock() {
        if let Some(current) = telemetry.as_mut() {
            let elapsed_ms = elapsed.as_millis() as u64;
            current.reduces += 1;
            current.wait_ms += elapsed_ms;
            current.request_bytes += request_bytes;
            current.response_bytes += response_bytes;
            current.samples_ms.push(elapsed_ms);
            match transport {
                ByteTransport::Local => current.local_calls += 1,
                ByteTransport::Direct => current.direct_calls += 1,
                ByteTransport::Tunnel => current.tunnel_calls += 1,
            }
            if fallback {
                current.fallback_calls += 1;
            }
        }
    }
}

pub fn take_decode_telemetry() -> Option<TpDecodeTelemetry> {
    DECODE_TELEMETRY.lock().ok().and_then(|mut telemetry| {
        let snapshot = telemetry.as_ref().map(telemetry_snapshot);
        *telemetry = None;
        snapshot
    })
}

/// Quantos reduces o decode em curso já fez. Zero = o op não entrou no grafo.
pub fn reduces_in_current_decode() -> u64 {
    DECODE
        .lock()
        .ok()
        .and_then(|d| d.as_ref().map(|(_, n)| *n))
        .unwrap_or(0)
}

/// Consome o próximo `barrier_id` deste decode.
fn next_barrier_id() -> Option<String> {
    let mut d = DECODE.lock().ok()?;
    let (id, seq) = d.as_mut()?;
    let atual = *seq;
    *seq += 1;
    Some(format!("{id}#{atual}"))
}

/// A coletiva está ligada neste processo?
pub fn is_active() -> bool {
    tp_state().is_some()
}

/// Liga a ponte que o `llama-engine` usa para consultar este módulo de dentro
/// do decode (`llm.rs` -> `llama_engine::tp_probe`). Chamado uma vez no arranque
/// do `node`; `tp.rs` fica em `node` porque a coletiva fala com o primary por
/// HTTP/túnel libp2p.
pub fn install_probe() {
    llama_engine::tp_probe::install(llama_engine::tp_probe::TpProbe {
        is_active,
        reduce_count,
        reduces_in_current_decode,
        take_failure,
        set_expected_reduces,
    });
}

/// Quantos reduces este processo já executou.
///
/// O decode compara antes e depois: com a coletiva ligada, um decode que não
/// disparou **nenhum** reduce rodou o grafo com meia largura e sem soma, e a
/// resposta é falsa. Foi o estado de 18/08 - 30 tok/s de texto sem sentido -, e
/// ele não tinha nenhum sintoma além do texto.
pub fn reduce_count() -> u64 {
    tp_state()
        .map(|s| s.counter.load(Ordering::Relaxed))
        .unwrap_or(0)
}

fn note_failure(motivo: String) {
    tracing::error!("coletiva TP falhou: {motivo}");
    if let Ok(mut f) = TP_FAILURE.lock() {
        // A primeira falha é a que explica; as seguintes são consequência dela.
        if f.is_none() {
            *f = Some(motivo);
        }
    }
}

/// Liga a coletiva TP deste processo e registra o callback no llama.cpp.
///
/// Chamado pelo apply quando o nó é primary/worker de um grupo (size > 1).
#[cfg(test)]
pub fn register_tp_reduce(
    rank: u32,
    size: u32,
    rank0_url: String,
    rank0_node_id: Option<String>,
    rank0_peer_id: Option<String>,
    rank0_supports_tunnel: bool,
    auth: Option<crate::auth::AuthState>,
) {
    register_tp_reduce_with_urls(
        rank,
        size,
        rank0_url.clone(),
        vec![rank0_url],
        Vec::new(),
        rank0_node_id,
        rank0_peer_id,
        rank0_supports_tunnel,
        auth,
    );
}

/// Liga a coletiva TP com todos os endereços conhecidos do primary.
pub fn register_tp_reduce_with_urls(
    rank: u32,
    size: u32,
    rank0_url: String,
    mut rank0_urls: Vec<String>,
    rank0_addr_candidates: Vec<String>,
    rank0_node_id: Option<String>,
    rank0_peer_id: Option<String>,
    rank0_supports_tunnel: bool,
    auth: Option<crate::auth::AuthState>,
) {
    let runtime = tokio::runtime::Handle::try_current().ok();
    if runtime.is_none() {
        note_failure(
            "coletiva TP registrada fora de um runtime Tokio; o callback não poderá aguardar a barreira"
                .to_string(),
        );
    }
    if rank0_urls.is_empty() {
        rank0_urls.push(rank0_url.clone());
    } else if !rank0_urls.iter().any(|url| url == &rank0_url) {
        rank0_urls.insert(0, rank0_url.clone());
    }
    // Cliente da coletiva: pinado quando o cluster está em TLS, simples quando
    // não está. Falhar aqui é melhor que descobrir no meio do primeiro decode.
    let client = match (crate::peer_client::tls_ativo(), rank0_node_id.as_deref()) {
        (true, Some(id)) => match crate::tls::client_config(id) {
            Ok(cfg) => reqwest::Client::builder()
                .use_preconfigured_tls(cfg)
                .build()
                .unwrap_or_else(|e| {
                    note_failure(format!("cliente TLS da coletiva não subiu: {e}"));
                    reqwest::Client::new()
                }),
            Err(e) => {
                note_failure(format!("certificado do primary {id} não pinou: {e}"));
                reqwest::Client::new()
            }
        },
        (true, None) => {
            // Grupo montado por uma versão que não propagava o id: o POST vai
            // ser recusado pelo certificado, e é melhor dizer isso agora.
            note_failure(
                "cluster em TLS e o grupo TP não trouxe o node_id do primary: \
                 refaça a implantação do grupo"
                    .to_string(),
            );
            reqwest::Client::new()
        }
        (false, _) => reqwest::Client::new(),
    };
    let state = Arc::new(TpState {
        rank,
        size,
        rank0_url,
        rank0_urls,
        rank0_addr_candidates,
        rank0_peer_id,
        rank0_supports_tunnel,
        client,
        auth,
        runtime,
        counter: AtomicU64::new(0),
        local_barrier: std::sync::RwLock::new(None),
    });
    if let Ok(mut slot) = TP_STATE.write() {
        *slot = Some(state);
    }
    unsafe {
        llama_engine::llama_cpp_sys_2::llama_set_tp_reduce_callback(
            Some(tp_reduce_cb),
            std::ptr::null_mut(),
        );
    }
}

/// Desliga a coletiva: o callback vira None e o fork deixa de inserir o op.
pub fn unregister_tp_reduce() {
    unsafe {
        llama_engine::llama_cpp_sys_2::llama_set_tp_reduce_callback(None, std::ptr::null_mut());
    }
    // Limpar o estado é metade do desligamento: sem isto `is_active()` continua
    // verdadeiro e o nó se comporta como membro de um grupo que não existe mais.
    if let Ok(mut slot) = TP_STATE.write() {
        *slot = None;
    }
    if let Ok(mut d) = DECODE.lock() {
        *d = None;
    }
    if let Ok(mut telemetry) = DECODE_TELEMETRY.lock() {
        *telemetry = None;
    }
    REPLICA.store(false, Ordering::Relaxed);
    // Falha do grupo antigo não pode derrubar a primeira requisição do modelo
    // novo, que não tem nada a ver com ela.
    let _ = take_failure();
}

/// O callback do op de reduce: lê a parcial (f32), contribui na barreira local
/// quando é o primary ou POSTa para ele quando é worker, e devolve a soma no
/// mesmo buffer.
unsafe extern "C" fn tp_reduce_cb(data: *mut c_void, n_bytes: usize, _userdata: *mut c_void) {
    if data.is_null() || n_bytes == 0 || n_bytes % 4 != 0 {
        return;
    }
    let Some(state) = tp_state() else {
        return;
    };
    let state = &state;
    let n = n_bytes / 4;
    let partial = unsafe { std::slice::from_raw_parts(data as *const f32, n).to_vec() };
    let seq = state.counter.fetch_add(1, Ordering::Relaxed);
    let Some(barrier_id) = next_barrier_id() else {
        // Reduce fora de um decode conhecido: não há id que case com os outros
        // ranks, então a soma seria por acaso. Melhor falhar nomeando isso.
        note_failure("reduce fora de um decode conhecido (begin_decode não foi chamado)".into());
        return;
    };
    if seq == 0 {
        // A primeira invocação é a prova de que o op do fork está no grafo.
        // Sem esta linha, "o callback não é chamado" e "o callback falha" têm
        // exatamente o mesmo sintoma: texto errado, rápido, sem log.
        if state.rank == 0 {
            tracing::info!(
                "coletiva TP ativa: primeiro reduce do rank 0 ({} valores) -> barreira local",
                n
            );
        } else {
            tracing::info!(
                "coletiva TP ativa: primeiro reduce do rank {} ({} valores) -> {}",
                state.rank,
                n,
                state.rank0_url
            );
        }
    }

    let Some(runtime) = state.runtime.as_ref() else {
        note_failure("coletiva TP sem runtime Tokio para aguardar o reduce".to_string());
        return;
    };
    let sum = runtime.block_on(post_reduce(
        state,
        &barrier_id,
        partial,
        crate::infer_tunnel::tunnel_sender(),
    ));
    match sum {
        Ok(sum) if sum.len() == n => unsafe {
            std::ptr::copy_nonoverlapping(sum.as_ptr(), data as *mut f32, n);
        },
        // Soma de tamanho errado é tão fatal quanto soma ausente: copiar
        // parcialmente deixaria metade do vetor reduzido e metade não.
        Ok(sum) => note_failure(format!(
            "soma da coletiva veio com {} valores, esperado {n}",
            sum.len()
        )),
        // O buffer continua com a parcial deste rank. Não copiar nada e seguir
        // era o defeito: a parcial passava por soma e a resposta saía errada.
        Err(e) => note_failure(e),
    }
}

/// Cabeçalho que marca um quadro como réplica de TP e carrega o id do decode.
///
/// Presente = "você é um worker executando o decode que o rank 0 está
/// executando agora". Ausente = quadro normal, vindo do gateway.
pub const TP_DECODE_HEADER: &str = "x-pom-tp-decode";

/// Põe os workers para rodar **o mesmo decode**, com o mesmo lote.
///
/// Esta é a metade que faltava do tensor parallelism. O gateway só fala com o
/// rank 0 (o worker é excluído do pipeline de propósito), então sem esta
/// transmissão o rank 0 postava a parcial e ficava esperando na barreira uma
/// contribuição que ninguém ia mandar: o decode travava. TP exige que todos os
/// ranks percorram o mesmo grafo sobre os mesmos tokens.
///
/// O corpo repassado é **o quadro original, byte a byte** - o worker roda o
/// mesmo handler e produz a mesma sequência de reduces. A resposta dele é
/// descartada: quem amostra o token é o rank 0, e com a cabeça de saída
/// replicada os dois chegam aos mesmos logits depois das somas.
///
/// Dispara em paralelo e **não espera**: quem espera é a barreira, no momento
/// certo. Esperar aqui seria serializar o que precisa ser simultâneo.
pub fn broadcast_decode(
    members: &[crate::network::TpMember],
    rota: &str,
    decode_id: &str,
    body: Vec<u8>,
) {
    broadcast_to(members, rota, decode_id, body, None)
}

fn broadcast_target(member_url: &str, rota: &str) -> (String, String) {
    (
        member_url.trim_end_matches('/').to_string(),
        rota.to_string(),
    )
}

/// O mesmo, dizendo o `content-type` quando o corpo não é um quadro binário.
///
/// O extrator `Json` do axum **exige** o cabeçalho: sem ele a réplica devolve
/// 415 e a chamada some, que é o pior tipo de falha para uma rota de ciclo de
/// vida - a sessão ficaria pendurada e o defeito só apareceria na requisição
/// seguinte, longe da causa.
pub fn broadcast_to(
    members: &[crate::network::TpMember],
    rota: &str,
    decode_id: &str,
    body: Vec<u8>,
    content_type: Option<&'static str>,
) {
    let replicas = members.len().saturating_sub(1) as u64;
    record_broadcast_scheduled(replicas, body.len() as u64);
    for membro in members.iter().skip(1) {
        // A base e o path precisam permanecer separados: o túnel usa apenas
        // o path para encaminhar ao loopback do worker. Concatenar a rota e
        // passar `path=""` faz o túnel chegar em `/`, que responde 404.
        let (url, path) = broadcast_target(&membro.url, rota);
        let url_candidates = if membro.url_candidates.is_empty() {
            vec![url.clone()]
        } else {
            membro.url_candidates.clone()
        };
        let cliente = match peer_client_for(&membro.node_id) {
            Some(c) => c,
            None => continue,
        };
        let corpo = body.clone();
        let ct = content_type;
        let id = decode_id.to_string();
        let quem = membro.node_id.clone();
        let auth = tp_state().and_then(|state| state.auth.clone());
        let peer_id = membro
            .libp2p_peer_id
            .as_deref()
            .and_then(|id| id.parse().ok());
        let supports_tunnel = membro.supports_tunnel;
        let addr_candidates = membro.addr_candidates.clone();
        let tunnel = crate::infer_tunnel::tunnel_sender();
        let auth_state = auth.clone();
        tokio::spawn(async move {
            let internal_token = match auth_state {
                Some(auth) => {
                    let config = auth.read().await;
                    crate::auth::internal_node_token(&config)
                }
                None => None,
            };
            let mut headers = vec![(TP_DECODE_HEADER.to_string(), id.clone())];
            if let Some(ct) = ct {
                headers.push(("content-type".to_string(), ct.to_string()));
            }
            if let Some(token) = internal_token {
                headers.push((
                    crate::auth::INTERNAL_NODE_REQUEST_HEADER.to_string(),
                    "1".to_string(),
                ));
                headers.push((
                    reqwest::header::AUTHORIZATION.as_str().to_string(),
                    format!("Bearer {token}"),
                ));
            }
            let result = crate::relay::send_bytes_or_tunnel_with_candidates(
                &cliente,
                &url,
                &url_candidates,
                &path,
                corpo,
                headers,
                &addr_candidates,
                tunnel,
                peer_id,
                supports_tunnel,
                &quem,
            )
            .await;
            if let Err(e) = result {
                note_failure(format!(
                    "não deu para pôr o rank {} no decode {id}: {e}",
                    &quem[..12.min(quem.len())]
                ));
            }
        });
    }
}

/// Cliente pinado no membro, ou `None` (com a falha anotada) se não der.
fn peer_client_for(node_id: &str) -> Option<reqwest::Client> {
    if !crate::peer_client::tls_ativo() {
        return Some(reqwest::Client::new());
    }
    match crate::tls::client_config(node_id) {
        Ok(cfg) => reqwest::Client::builder()
            .use_preconfigured_tls(cfg)
            .build()
            .ok()
            .or_else(|| {
                note_failure(format!("cliente TLS para {node_id} não subiu"));
                None
            }),
        Err(e) => {
            note_failure(format!("certificado de {node_id} não pinou: {e}"));
            None
        }
    }
}

async fn post_reduce(
    state: &Arc<TpState>,
    barrier_id: &str,
    data: Vec<f32>,
    tunnel: Option<&crate::infer_tunnel::TunnelOutboundSender>,
) -> Result<Vec<f32>, String> {
    if let Some((barrier, size)) = local_barrier_for(state) {
        let expected = data.len();
        let request_bytes = (expected * std::mem::size_of::<f32>()) as u64;
        let started = std::time::Instant::now();
        let receiver = barrier
            .contribute(barrier_id, state.rank, size, data)
            .map_err(|error| format!("barreira local do primary recusou a parcial: {error}"))?;
        let sum = receiver
            .await
            .map_err(|_| "barreira local do primary foi encerrada".to_string())?;
        record_reduce(
            expected,
            request_bytes,
            (sum.len() * std::mem::size_of::<f32>()) as u64,
            started.elapsed(),
            ByteTransport::Local,
            false,
        );
        return Ok(sum);
    }

    // f32 cru, little-endian. JSON custava ~12 bytes por float de 4: um prefill
    // de 100 tokens dava ~5 MB por reduce e estourava o limite de corpo (413),
    // 64 vezes por decode.
    let mut corpo = Vec::with_capacity(data.len() * 4);
    for v in &data {
        corpo.extend_from_slice(&v.to_le_bytes());
    }
    let esperado = data.len();
    let request_bytes = corpo.len() as u64;
    let started = std::time::Instant::now();

    let internal_token = match &state.auth {
        Some(auth) => {
            let config = auth.read().await;
            crate::auth::internal_node_token(&config)
        }
        None => None,
    };
    let headers = vec![
        (TP_BARRIER_HEADER.to_string(), barrier_id.to_string()),
        (TP_RANK_HEADER.to_string(), state.rank.to_string()),
        (
            "content-type".to_string(),
            "application/octet-stream".to_string(),
        ),
    ]
    .into_iter()
    .chain(internal_token.as_ref().map(|_| {
        (
            crate::auth::INTERNAL_NODE_REQUEST_HEADER.to_string(),
            "1".to_string(),
        )
    }))
    .chain(internal_token.as_ref().map(|token| {
        (
            reqwest::header::AUTHORIZATION.as_str().to_string(),
            format!("Bearer {token}"),
        )
    }))
    .collect();
    let response = crate::relay::send_bytes_or_tunnel_with_candidates(
        &state.client,
        &state.rank0_url,
        &state.rank0_urls,
        "/v1/tp/reduce",
        corpo,
        headers,
        &state.rank0_addr_candidates,
        tunnel,
        state
            .rank0_peer_id
            .as_deref()
            .and_then(|id| id.parse().ok()),
        state.rank0_supports_tunnel,
        "primary TP",
    )
    .await
    .map_err(|e| e.to_string())?;
    record_reduce(
        esperado,
        request_bytes,
        response.body.len() as u64,
        started.elapsed(),
        response.transport,
        response.fallback,
    );
    let bytes = response.body;
    if bytes.len() != esperado * 4 {
        return Err(format!(
            "soma veio com {} bytes, esperado {}",
            bytes.len(),
            esperado * 4
        ));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_keeps_the_inference_route_when_using_the_tunnel() {
        let (base_url, path) = broadcast_target("https://[2001:db8::2]:8187", "/v1/prefill");

        assert_eq!(base_url, "https://[2001:db8::2]:8187");
        assert_eq!(path, "/v1/prefill");
    }

    /// O `barrier_id` é função do decode, não do relógio de cada processo.
    ///
    /// Dois ranks que abrem o **mesmo** decode numeram os reduces igual, então
    /// as parciais se encontram. Enquanto foi um contador por processo, bastava
    /// um worker reiniciar para o grupo nunca mais se encontrar - e isso não
    /// dava erro, só travava.
    #[test]
    fn the_barrier_id_comes_from_the_decode_so_every_rank_agrees() {
        let _trava = ESTADO_GLOBAL.lock();
        limpa();
        begin_decode("sessao-x:chunk-0");
        let rank0: Vec<String> = (0..3).map(|_| next_barrier_id().unwrap()).collect();

        // Outro processo, mesmo decode: a numeração recomeça do zero.
        begin_decode("sessao-x:chunk-0");
        let rank1: Vec<String> = (0..3).map(|_| next_barrier_id().unwrap()).collect();

        assert_eq!(rank0, rank1, "os ranks têm que concordar nos ids");
        assert_eq!(rank0[0], "sessao-x:chunk-0#0");
        assert_eq!(rank0[2], "sessao-x:chunk-0#2");

        // Decode seguinte não reaproveita id do anterior.
        begin_decode("sessao-x:chunk-1");
        assert_eq!(next_barrier_id().unwrap(), "sessao-x:chunk-1#0");
    }

    /// Desligar o grupo desliga o nó: implantar um modelo inteiro por cima de
    /// um grupo TP não pode deixar resíduo.
    ///
    /// `TP_STATE` era um `OnceLock`, que não tem como ser limpo. Um processo que
    /// um dia serviu TP continuava se declarando TP para sempre: a implantação
    /// seguinte, de um modelo inteiro sem coletiva nenhuma, avisava "reduziu 1
    /// vezes, esperado 64" - e a checagem de zero reduces chegava a derrubar a
    /// requisição (18/08).
    #[test]
    fn turning_the_group_off_leaves_no_residue() {
        let _trava = ESTADO_GLOBAL.lock();
        limpa();
        register_tp_reduce(
            1,
            2,
            "https://127.0.0.1:9/".into(),
            Some("um-node-id-qualquer".into()),
            None,
            false,
            None,
        );
        begin_replica_decode("decode-do-grupo");
        let _ = next_barrier_id();
        note_failure("falha do grupo antigo".into());
        assert!(is_active(), "com grupo ligado, o nó é TP");

        unregister_tp_reduce();

        assert!(!is_active(), "sem grupo, o nó não é TP");
        assert_eq!(reduce_count(), 0);
        assert!(!is_replica(), "a marca de réplica não sobrevive");
        assert!(
            next_barrier_id().is_none(),
            "não há decode aberto depois de desligar"
        );
        assert!(
            take_failure().is_none(),
            "falha do grupo antigo não derruba a requisição do modelo novo"
        );
    }

    /// Réplica não decide nada; rank 0 decide. E abrir um decode normal tira a
    /// marca, senão o processo carregaria "sou réplica" para sempre.
    #[test]
    fn a_replica_has_no_say_and_the_mark_does_not_leak() {
        let _trava = ESTADO_GLOBAL.lock();
        limpa();
        begin_decode("normal");
        assert!(!is_replica(), "rank 0 decide por si");

        begin_replica_decode("do-rank-0");
        assert!(is_replica(), "quem recebeu o id do rank 0 é réplica");
        assert_eq!(next_barrier_id().unwrap(), "do-rank-0#0");

        begin_decode("outro");
        assert!(
            !is_replica(),
            "um decode próprio depois de uma réplica volta a ser próprio"
        );
    }

    /// A falha da coletiva é guardada, consumida uma vez, e a **primeira**
    /// vence: as seguintes são consequência dela.
    ///
    /// É o que transforma "resposta errada" em "requisição com erro". Um
    /// all-reduce que falha não degrada a qualidade - ele produz números
    /// falsos, e o texto sai plausível e sem sentido.
    #[test]
    fn the_first_failure_wins_and_is_consumed_once() {
        let _trava = ESTADO_GLOBAL.lock();
        limpa();
        let _ = take_failure(); // limpa resíduo de outro teste

        note_failure("certificado do primary não pinou".into());
        note_failure("erro seguinte, consequência do primeiro".into());

        let motivo = take_failure().expect("a falha tem que sobreviver até o decode");
        assert!(
            motivo.contains("certificado"),
            "a primeira falha é a que explica: {motivo}"
        );
        assert!(
            take_failure().is_none(),
            "consumida uma vez: a requisição seguinte não herda a falha da anterior"
        );
    }

    #[test]
    fn telemetry_keeps_collective_counts_and_bytes_separate() {
        let _trava = ESTADO_GLOBAL.lock().unwrap();
        limpa();
        register_tp_reduce(0, 2, "https://127.0.0.1:9/".into(), None, None, false, None);
        begin_decode("telemetry");
        set_expected_reduces(64);
        record_broadcast_scheduled(2, 128);
        record_reduce(
            64,
            128,
            256,
            std::time::Duration::from_millis(20),
            ByteTransport::Direct,
            false,
        );
        record_reduce(
            64,
            128,
            256,
            std::time::Duration::from_millis(30),
            ByteTransport::Tunnel,
            true,
        );

        let telemetry = take_decode_telemetry().expect("TP telemetry");

        assert_eq!(telemetry.reduces, 2);
        assert_eq!(telemetry.expected_reduces, 64);
        assert_eq!(telemetry.request_bytes, 256);
        assert_eq!(telemetry.response_bytes, 512);
        assert_eq!(telemetry.direct_calls, 1);
        assert_eq!(telemetry.tunnel_calls, 1);
        assert_eq!(telemetry.fallback_calls, 1);
        assert_eq!(telemetry.broadcast_calls, 2);
        assert_eq!(telemetry.broadcast_bytes, 256);
        assert_eq!(telemetry.reduce_p50_ms, 30);
        assert_eq!(telemetry.reduce_p95_ms, 30);
    }

    #[tokio::test]
    async fn primary_contributes_to_the_local_barrier_without_dialing_itself() {
        let _trava = ESTADO_GLOBAL.lock().unwrap();
        limpa();
        let barrier = ReduceBarrier::new();
        register_tp_reduce(
            0,
            2,
            "https://[2001:db8::1]:8181".into(),
            Some("primary-node".into()),
            Some("12D3KooWPrimary".into()),
            true,
            None,
        );
        let barrier_handle = set_local_barrier(barrier.clone(), 2).expect("estado do primary");

        let state = tp_state().expect("estado do primary");
        let worker = barrier
            .contribute("local-primary", 1, 2, vec![3.0])
            .expect("contribuição do worker");
        let primary = post_reduce(&state, "local-primary", vec![2.0], None);
        let (worker_sum, primary_sum) = tokio::join!(worker, primary);

        assert_eq!(worker_sum.expect("soma do worker"), vec![5.0]);
        assert_eq!(primary_sum.expect("soma do primary"), vec![5.0]);
        clear_local_barrier(barrier_handle);
        unregister_tp_reduce();
    }

    #[tokio::test]
    async fn callback_can_block_on_tokio_from_a_llama_blocking_thread() {
        let _trava = ESTADO_GLOBAL.lock().unwrap();
        limpa();
        register_tp_reduce(
            0,
            1,
            "http://127.0.0.1:9/".into(),
            Some("primary-node".into()),
            None,
            false,
            None,
        );
        let barrier_handle = set_local_barrier(ReduceBarrier::new(), 1).expect("barreira");
        begin_decode("blocking-thread");

        let values = std::thread::spawn(|| {
            let mut values = vec![2.0f32, 3.0];
            unsafe {
                tp_reduce_cb(
                    values.as_mut_ptr().cast(),
                    values.len() * std::mem::size_of::<f32>(),
                    std::ptr::null_mut(),
                );
            }
            values
        })
        .join()
        .expect("callback não pode panicar");

        assert_eq!(values, [2.0, 3.0]);
        clear_local_barrier(barrier_handle);
        unregister_tp_reduce();
    }

    use crate::collective::ReduceBarrier;
    use axum::{routing::post, Router};
    use tokio::sync::mpsc;

    /// Trava que serializa os testes que mexem no estado GLOBAL da coletiva
    /// (`TP_STATE`, `DECODE`, `REPLICA`, `TP_FAILURE`).
    ///
    /// Sem isto eles se atropelam em paralelo e falham por acaso - o mesmo
    /// defeito que perseguiu `transfer::offer_latency_tests` o dia 18/08
    /// inteiro, e que só se explicou depois de horas. Todo teste novo que toque
    /// nesses estáticos toma esta trava.
    static ESTADO_GLOBAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Deixa o processo sem grupo, para um teste não herdar o do anterior.
    fn limpa() {
        unregister_tp_reduce();
    }

    /// Os dois lados de produção falando entre si: o cliente real
    /// (`post_reduce`) contra o handler real (`infer_server::handle_tp_reduce`).
    ///
    /// A versão anterior deste teste montava um servidor JSON próprio e um
    /// cliente JSON próprio - não exercitava nenhum dos dois lados reais, e
    /// continuou passando verde depois que a produção trocou o corpo para f32
    /// cru. Um teste que não pode falhar quando o contrato muda não é rede de
    /// proteção, é enfeite.
    #[tokio::test]
    async fn three_ranks_reduce_over_the_real_wire() {
        let app = Router::new()
            .route("/v1/tp/reduce", post(crate::infer_server::handle_tp_reduce))
            .with_state((ReduceBarrier::new(), 3u32));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let url = format!("http://{addr}");
        let partials = vec![vec![1.0f32, 2.0], vec![3.0, 4.0], vec![5.0, 6.0]];
        let esperado = vec![9.0f32, 12.0];

        let handles: Vec<_> = partials
            .into_iter()
            .enumerate()
            .map(|(rank, data)| {
                let state = Arc::new(TpState {
                    rank: rank as u32,
                    size: 3,
                    rank0_url: url.clone(),
                    rank0_urls: vec![url.clone()],
                    rank0_addr_candidates: Vec::new(),
                    rank0_peer_id: None,
                    rank0_supports_tunnel: false,
                    client: reqwest::Client::new(),
                    auth: None,
                    runtime: None,
                    counter: AtomicU64::new(0),
                    local_barrier: std::sync::RwLock::new(None),
                });
                tokio::spawn(async move { post_reduce(&state, "b", data, None).await })
            })
            .collect();

        for h in handles {
            assert_eq!(h.await.unwrap().expect("reduce"), esperado);
        }
    }

    #[tokio::test]
    async fn reduce_uses_the_tunnel_when_the_primary_http_endpoint_is_unreachable() {
        let peer_id = libp2p::PeerId::random();
        let (tunnel, mut received) = mpsc::channel::<crate::infer_tunnel::TunnelOutbound>(1);
        let receiver = tokio::spawn(async move {
            let outbound = received.recv().await.expect("tunnel request");
            assert_eq!(outbound.peer_id, peer_id);
            assert_eq!(outbound.request.path, "/v1/tp/reduce");
            assert_eq!(
                &outbound.request.headers[..2],
                &[
                    (TP_BARRIER_HEADER.to_string(), "b".to_string()),
                    (TP_RANK_HEADER.to_string(), "1".to_string()),
                ]
            );
            let values: Vec<f32> = outbound
                .request
                .body
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            assert_eq!(values, [1.0, 2.0]);
            outbound
                .reply
                .send(Ok(crate::infer_tunnel::TunnelResponse {
                    status: 200,
                    body: [3.0f32, 4.0]
                        .into_iter()
                        .flat_map(f32::to_le_bytes)
                        .collect(),
                }))
                .expect("reply");
        });

        let state = Arc::new(TpState {
            rank: 1,
            size: 2,
            rank0_url: "http://127.0.0.1:1".into(),
            rank0_urls: vec!["http://127.0.0.1:1".into()],
            rank0_addr_candidates: Vec::new(),
            rank0_peer_id: Some(peer_id.to_string()),
            rank0_supports_tunnel: true,
            client: reqwest::Client::new(),
            auth: None,
            runtime: None,
            counter: AtomicU64::new(0),
            local_barrier: std::sync::RwLock::new(None),
        });
        let result = post_reduce(&state, "b", vec![1.0, 2.0], Some(&tunnel))
            .await
            .expect("tunnel reduce");

        assert_eq!(result, [3.0, 4.0]);
        receiver.await.expect("receiver");
    }
}
