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

/// Cabeçalhos da coletiva. O corpo é a parcial em f32 cru; metadado de tamanho
/// fixo vai em cabeçalho, e não misturado ao vetor.
pub const TP_BARRIER_HEADER: &str = "x-pom-tp-barrier";
pub const TP_RANK_HEADER: &str = "x-pom-tp-rank";



struct TpState {
    rank: u32,
    /// URL do primary (rank 0) da coletiva. Todo rank POSTa para cá.
    rank0_url: String,
    /// Cliente **pinado no `node_id` do primary**, montado uma vez no registro.
    ///
    /// A coletiva usava `reqwest::Client::new()`, que valida o certificado pela
    /// cadeia pública; o do cluster é autoassinado e verificado por
    /// `SHA256(pubkey) == node_id`. Com TLS ligado, todo POST morria em 13 ms
    /// (medido na ksia, 18/08) e o all-reduce nunca acontecia.
    client: reqwest::Client,
    /// Contador de reduces. Em execução lockstep, o k-ésimo reduce de todos os
    /// ranks corresponde à mesma projeção, então o id casa entre os processos.
    counter: AtomicU64,
}

static TP_STATE: OnceLock<Arc<TpState>> = OnceLock::new();

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
}

/// Quantos reduces o decode em curso já fez. Zero = o op não entrou no grafo.
pub fn reduces_in_current_decode() -> u64 {
    DECODE.lock().ok().and_then(|d| d.as_ref().map(|(_, n)| *n)).unwrap_or(0)
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
    TP_STATE.get().is_some()
}

/// Quantos reduces este processo já executou.
///
/// O decode compara antes e depois: com a coletiva ligada, um decode que não
/// disparou **nenhum** reduce rodou o grafo com meia largura e sem soma, e a
/// resposta é falsa. Foi o estado de 18/08 - 30 tok/s de texto sem sentido -, e
/// ele não tinha nenhum sintoma além do texto.
pub fn reduce_count() -> u64 {
    TP_STATE
        .get()
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
/// V1: o estado é fixado uma vez por processo; trocar o grupo exige reiniciar.
pub fn register_tp_reduce(rank: u32, rank0_url: String, rank0_node_id: Option<String>) {
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
        rank0_url,
        client,
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
        tracing::info!(
            "coletiva TP ativa: primeiro reduce do rank {} ({} valores) -> {}",
            state.rank,
            n,
            state.rank0_url
        );
    }

    let sum = tokio::runtime::Handle::current()
        .block_on(post_reduce(state, &barrier_id, partial));
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
    for membro in members.iter().skip(1) {
        let url = format!("{}{rota}", membro.url.trim_end_matches('/'));
        let cliente = match peer_client_for(&membro.node_id) {
            Some(c) => c,
            None => continue,
        };
        let corpo = body.clone();
        let ct = content_type;
        let id = decode_id.to_string();
        let quem = membro.node_id.clone();
        tokio::spawn(async move {
            let mut req = cliente.post(&url).header(TP_DECODE_HEADER, &id);
            if let Some(ct) = ct {
                req = req.header("content-type", ct);
            }
            let r = req.body(corpo).send().await;
            match r {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => {
                    // O corpo é a explicação do próprio worker ("sessão
                    // desconhecida", "chunk fora de ordem", ...). Reportar só o
                    // status obriga quem depura a adivinhar - foi o que custou
                    // a manhã de 17/08 com a checagem de shard.
                    let status = resp.status();
                    let motivo = resp
                        .text()
                        .await
                        .unwrap_or_else(|e| format!("(corpo ilegível: {e})"));
                    note_failure(format!(
                        "rank {} recusou o decode {id}: HTTP {status} - {}",
                        &quem[..12.min(quem.len())],
                        motivo.trim()
                    ))
                }
                Err(e) => note_failure(format!(
                    "não deu para pôr o rank {} no decode {id}: {e}",
                    &quem[..12.min(quem.len())]
                )),
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
) -> Result<Vec<f32>, String> {
    // f32 cru, little-endian. JSON custava ~12 bytes por float de 4: um prefill
    // de 100 tokens dava ~5 MB por reduce e estourava o limite de corpo (413),
    // 64 vezes por decode.
    let mut corpo = Vec::with_capacity(data.len() * 4);
    for v in &data {
        corpo.extend_from_slice(&v.to_le_bytes());
    }
    let esperado = data.len();

    let resp = state
        .client
        .post(format!("{}/v1/tp/reduce", state.rank0_url))
        .header(TP_BARRIER_HEADER, barrier_id)
        .header(TP_RANK_HEADER, state.rank.to_string())
        .header("content-type", "application/octet-stream")
        .body(corpo)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let motivo = resp.text().await.unwrap_or_default();
        return Err(format!(
            "POST /v1/tp/reduce respondeu {status}{}",
            if motivo.trim().is_empty() {
                String::new()
            } else {
                format!(" - {}", motivo.trim())
            }
        ));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
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

    /// O `barrier_id` é função do decode, não do relógio de cada processo.
    ///
    /// Dois ranks que abrem o **mesmo** decode numeram os reduces igual, então
    /// as parciais se encontram. Enquanto foi um contador por processo, bastava
    /// um worker reiniciar para o grupo nunca mais se encontrar - e isso não
    /// dava erro, só travava.
    #[test]
    fn the_barrier_id_comes_from_the_decode_so_every_rank_agrees() {
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

    /// Réplica não decide nada; rank 0 decide. E abrir um decode normal tira a
    /// marca, senão o processo carregaria "sou réplica" para sempre.
    #[test]
    fn a_replica_has_no_say_and_the_mark_does_not_leak() {
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
    use crate::collective::ReduceBarrier;
    use axum::{routing::post, Router};

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
                    rank0_url: url.clone(),
                    client: reqwest::Client::new(),
                    counter: AtomicU64::new(0),
                });
                tokio::spawn(async move { post_reduce(&state, "b", data).await })
            })
            .collect();

        for h in handles {
            assert_eq!(h.await.unwrap().expect("reduce"), esperado);
        }
    }
}
