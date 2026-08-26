//! Resposta direta: a cauda entrega o resultado a quem pediu.
//!
//! O modo anel (`relay/v1-chain`) encadeia chamadas HTTP aninhadas — cada hop
//! segura a própria resposta aberta enquanto chama o hop seguinte, e o
//! resultado volta refazendo o caminho da ida. São `2N` travessias de rede por
//! token para uma cadeia de `N` hops, e a conta piora justamente quando a
//! cabeça não está na máquina que recebeu a requisição: aí a ida e a volta
//! atravessam o enlace duas vezes cada. Medido em 2026-08-14 entre os dois
//! sites do captain (RTT 30 ms): 25 tok/s com a cabeça local contra 14 tok/s
//! com ela remota, exatamente um RTT a mais por token.
//!
//! Aqui a cadeia deixa de voltar. O gateway manda junto do quadro um
//! [`DirectReply::reply_to`] — os endereços por onde ele mesmo pode ser
//! alcançado, mais um *ticket* aleatório — e cada hop repassa esse campo
//! adiante sem tocar nele. Quem termina (a cauda, ou o hop que falhou ao
//! repassar) faz **um** POST em `/v1/relay-result` direto ao gateway. São
//! `N + 1` travessias, e o custo deixa de depender de onde está a cabeça.
//!
//! O que cada hop devolve ao seu chamador vira um *ack* imediato: o hop já
//! aceitou a responsabilidade pelo quadro, e nada mais viaja por ali. O ack
//! sai do caminho crítico — quem espera é o gateway, no ticket.
//!
//! ## Falha, cancelamento e prazo
//!
//! * **Hop do meio morre antes de aceitar** — quem o chamou recebe erro de
//!   transporte e entrega um [`crate::relay::RelayFailure`] direto ao gateway,
//!   nomeando o nó que realmente morreu.
//! * **Cauda morre** — idem, denunciada pelo hop anterior.
//! * **Hop morre depois de aceitar** (processo caiu, máquina sumiu) — não há
//!   quem denuncie; quem fecha a requisição é o prazo de [`DIRECT_REPLY_DEADLINE`].
//! * **Resposta direta perdida** (gateway inalcançável a partir da cauda) —
//!   mesmo prazo. A cauda tenta cada URL de `reply_to` antes de desistir.
//! * **Resposta que chega depois da desistência** — o [`Ticket`] já saiu do
//!   inbox (é removido no `Drop`, em qualquer caminho de saída), e o resultado
//!   atrasado leva `410 Gone`. Como o ticket é único por quadro, ele nunca
//!   pode ser confundido com o do token seguinte.
//!
//! A capacidade é negociada como todas as outras: só entra quando **todos** os
//! passos anunciam `relay/v2-direct` (ver `PipelinePlan::supports_direct_reply`)
//! e o gateway sabe pelo menos um endereço de si mesmo. Um cluster em
//! atualização progressiva cai no anel de sempre, sem nada a configurar.

use anyhow::{anyhow, Result};
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, warn};

/// Rota em que o gateway recebe o resultado que a cauda entrega.
pub const RELAY_RESULT_PATH: &str = "/v1/relay-result";

/// Quanto o gateway espera pelo resultado direto antes de desistir do quadro.
///
/// Mesmo teto que `relay::send_frame` já usa por chamada: um hop que estoura
/// isso não está lento, está morto, e a sessão precisa ser liberada.
pub const DIRECT_REPLY_DEADLINE: Duration = Duration::from_secs(600);

/// Tentativas de entrega do resultado direto, por URL candidata.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);

// ── Inbox ─────────────────────────────────────────────────────────────────────

/// Os quadros que este nó despachou e ainda espera resultado.
///
/// Um mapa de ticket → canal. Vive no gateway (`GatewayState`) porque é o
/// gateway que abre a requisição; o handler de `/v1/relay-result` só procura o
/// ticket aqui.
#[derive(Clone, Default)]
pub struct ReplyInbox(Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>);

impl ReplyInbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Abre um ticket. O guarda devolvido remove-o do inbox quando cai, em
    /// qualquer caminho de saída — é o que impede um ticket órfão de segurar
    /// memória depois de um erro ou de um prazo estourado.
    pub fn register(&self) -> Ticket {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.0.lock().unwrap().insert(id.clone(), tx);
        Ticket {
            id,
            inbox: self.clone(),
            rx,
        }
    }

    /// Entrega um resultado. `false` quando o ticket não existe mais — o
    /// gateway já desistiu, e quem entregou precisa saber que chegou tarde.
    pub fn deliver(&self, ticket: &str, meta: Value) -> bool {
        let tx = self.0.lock().unwrap().remove(ticket);
        match tx {
            Some(tx) => tx.send(meta).is_ok(),
            None => false,
        }
    }

    fn cancel(&self, ticket: &str) {
        self.0.lock().unwrap().remove(ticket);
    }

    /// Quantos quadros estão em voo. Só para teste e diagnóstico.
    pub fn outstanding(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

/// Um quadro em voo. Removido do inbox no `Drop`.
pub struct Ticket {
    id: String,
    inbox: ReplyInbox,
    rx: oneshot::Receiver<Value>,
}

impl Ticket {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.inbox.cancel(&self.id);
    }
}

// ── O que o gateway manda junto do quadro ─────────────────────────────────────

/// Endereços por onde a cauda alcança este gateway, mais o inbox onde o
/// resultado será procurado.
#[derive(Clone)]
pub struct DirectReply {
    inbox: ReplyInbox,
    urls: Vec<String>,
    /// node_id de quem vai receber a resposta.
    ///
    /// Viaja no `reply_to` porque quem entrega precisa dele para fixar o
    /// certificado do gateway: com o cluster em TLS, uma URL sem identidade não
    /// dá para verificar. Peer em binário antigo não manda o campo, e aí a
    /// entrega cai no cliente sem pinning - ver `deliver_result`.
    node_id: String,
}

impl DirectReply {
    /// `None` quando este nó não sabe nenhum endereço de si mesmo: sem isso a
    /// cauda não teria para onde responder, e o certo é cair no anel de sempre
    /// em vez de despachar um quadro que nunca volta.
    pub fn new(inbox: ReplyInbox, urls: Vec<String>, node_id: String) -> Option<Self> {
        if urls.is_empty() {
            return None;
        }
        Some(Self {
            inbox,
            urls,
            node_id,
        })
    }

    pub fn ticket(&self) -> Ticket {
        self.inbox.register()
    }

    pub fn reply_to(&self, ticket: &str) -> Value {
        json!({ "urls": self.urls, "ticket": ticket, "node_id": self.node_id })
    }
}

/// URLs de gateway deste nó, na ordem em que um peer deve tentá-las.
///
/// Mesma regra de preferência de `gateway::self_dial_host`: o endereço fixado
/// pelo operador vale mais que o da LAN, porque entre sites um `192.168.x.y` é
/// tão inútil quanto o loopback. Os candidatos enumerados das interfaces vêm
/// depois, e daí o loopback não entra: a cauda quase nunca é esta máquina.
///
/// `pinned` é o endereço que o operador fixou (`advertise_addr`), e só ele pode
/// ser loopback: numa máquina só, apontar para si mesmo é a configuração certa
/// e foi escolha de quem configurou. O que não pode entrar é o loopback que
/// aparece sozinho, do fallback `127.0.0.1` de `gateway::self_dial_host` ou da
/// enumeração de interfaces: aí a cauda em outra máquina entrega ao loopback
/// *dela* e a resposta se perde, com prazo de 600 s. Sem nenhuma URL utilizável
/// a lista fica vazia e o plano cai no anel, que funciona.
pub fn self_reply_urls(
    pinned: Option<String>,
    hosts: impl IntoIterator<Item = String>,
    gateway_port: u16,
) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    let fixado = pinned.into_iter().map(|h| (h, true));
    for (host, fixado_pelo_operador) in fixado.chain(hosts.into_iter().map(|h| (h, false))) {
        let host = crate::network::ip_host(&host).to_string();
        if host.is_empty() || (!fixado_pelo_operador && is_unroutable_host(&host)) {
            continue;
        }
        // O esquema segue a política do cluster: estas URLs são as que a cauda
        // vai discar de volta, e num cluster em TLS o `/relay/result` deste nó
        // recusa texto claro vindo de fora.
        let url =
            crate::peer_client::peer_base_url(&host, gateway_port, crate::peer_client::tls_ativo());
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    urls
}

/// Endereço que não serve para outra máquina nos alcançar: o curinga de bind e
/// o loopback, em v4 e v6. O loopback é o que mais engana, porque
/// `gateway::self_dial_host` cai em `127.0.0.1` quando não acha endereço
/// melhor: a lista fica não-vazia, o modo direto liga, e a cauda em outra
/// máquina entrega o resultado ao loopback *dela*, onde ninguém espera.
fn is_unroutable_host(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback() || ip.is_unspecified(),
        // Nome que não é IP (um `--public-addr` com domínio, por exemplo)
        // passa: resolvê-lo aqui seria adivinhar.
        Err(_) => bare.eq_ignore_ascii_case("localhost"),
    }
}

// ── Lado de quem recebe o resultado (gateway) ─────────────────────────────────

/// Handler de `/v1/relay-result`: procura o ticket e resolve o canal.
///
/// A autenticação é o próprio ticket, um UUID v4 sorteado por quadro e
/// conhecido apenas pela cadeia. Um resultado sem ticket vivo é recusado com
/// `410`, que é o que diz a quem entregou que o gateway já desistiu.
pub fn handle_relay_result(inbox: &ReplyInbox, body: Bytes) -> Response {
    let (mut meta, _tensor) = match crate::wire::decode(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("quadro inválido: {e}")).into_response()
        }
    };
    let Some(ticket) = meta
        .get("ticket")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return (StatusCode::BAD_REQUEST, "resultado sem ticket").into_response();
    };
    // Os bytes que atravessaram a rede na volta. Só quem recebeu sabe.
    meta["result_bytes"] = json!(body.len());
    if inbox.deliver(&ticket, meta) {
        (StatusCode::OK, "ok").into_response()
    } else {
        debug!("resultado direto para ticket desconhecido (o gateway já desistiu)");
        (StatusCode::GONE, "ticket desconhecido").into_response()
    }
}

/// Espera o resultado direto, sem soltar o erro do ack nem o prazo.
///
/// Três coisas correm ao mesmo tempo e a primeira que chegar decide:
///
/// 1. **o resultado**, entregue pela cauda no inbox — o caso normal;
/// 2. **o ack da cabeça**, que só interessa quando falha (aí não há cadeia
///    nenhuma para esperar) ou quando vem com resposta completa dentro, que é
///    um peer antigo borbulhando em vez de aceitar — nesse caso é a resposta;
/// 3. **o prazo**, para o hop que morreu depois de aceitar.
pub async fn await_result<F>(ack: F, ticket: &mut Ticket, head_label: &str) -> Result<Value>
where
    F: Future<Output = Result<crate::relay::SentFrame>>,
{
    tokio::pin!(ack);
    let deadline = tokio::time::sleep(DIRECT_REPLY_DEADLINE);
    tokio::pin!(deadline);
    let mut acked = false;

    loop {
        tokio::select! {
            biased;
            got = &mut ticket.rx => {
                return got.map_err(|_| anyhow!("resultado direto perdido antes de chegar"));
            }
            sent = &mut ack, if !acked => {
                acked = true;
                match sent {
                    // Peer antigo: em vez de aceitar e repassar, borbulhou a
                    // resposta inteira. Vale como resultado — é o que mantém
                    // um cluster meio atualizado funcionando mesmo se a
                    // negociação de capacidade escapar.
                    Ok(frame) if frame.meta.get("token_id").is_some() => return Ok(frame.meta),
                    Ok(_) => {}
                    Err(e) => return Err(e),
                }
            }
            _ = &mut deadline => {
                return Err(anyhow!(
                    "nenhum hop entregou o resultado em {}s (cadeia despachada em {head_label})",
                    DIRECT_REPLY_DEADLINE.as_secs()
                ));
            }
        }
    }
}

/// O erro que um hop entregou no lugar do resultado, se foi isso que chegou.
pub fn result_failure(meta: &Value) -> Option<crate::relay::RelayFailure> {
    serde_json::from_value(meta.get("error")?.clone()).ok()
}

// ── Lado de quem entrega (cauda, ou hop que falhou) ───────────────────────────

/// Entrega um resultado ao gateway, tentando cada URL de `reply_to` em ordem.
///
/// Uma máquina pode ser alcançável por vários endereços e nenhum deles é
/// certo de fora: o gateway anuncia todos os que conhece de si e quem entrega
/// descobre qual funciona. Falhou em todos, o gateway fecha no prazo — não há
/// terceira via, e inventar uma seria pior que a espera.
pub async fn deliver_result(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    mut meta: Value,
) -> bool {
    let Some(ticket) = reply_to.get("ticket").and_then(|v| v.as_str()) else {
        warn!("reply_to sem ticket — resultado direto descartado");
        return false;
    };
    meta["ticket"] = json!(ticket);

    let body = match crate::wire::encode(&meta, None, crate::wire::WireDtype::F16) {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!("resultado direto não pôde ser codificado: {e}");
            return false;
        }
    };

    let urls = reply_to
        .get("urls")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    // Sem node_id no `reply_to` o remetente roda binário antigo: não há o que
    // fixar, e a entrega vai pelo cliente compartilhado. Num cluster estrito
    // ela será recusada do outro lado, que é o resultado correto - o que não
    // pode acontecer é entregar em texto claro fingindo que foi verificado.
    let cliente = match reply_to.get("node_id").and_then(|v| v.as_str()) {
        Some(id) => match http.for_peer(id) {
            Ok(c) => c,
            Err(e) => {
                warn!("sem cliente para entregar o resultado a {id}: {e}");
                return false;
            }
        },
        None => http.plain(),
    };
    let mut last = String::new();
    for url in urls.iter().filter_map(|u| u.as_str()) {
        let target = format!("{url}{RELAY_RESULT_PATH}");
        match cliente
            .post(&target)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body.clone())
            .timeout(DELIVERY_TIMEOUT)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => return true,
            // `410` é resposta, não falha de rota: o gateway desistiu deste
            // quadro. Tentar o próximo endereço só acharia o mesmo inbox.
            Ok(r) if r.status() == StatusCode::GONE => {
                debug!("gateway já havia desistido do quadro entregue em {target}");
                return false;
            }
            Ok(r) => last = format!("{target} respondeu {}", r.status()),
            Err(e) => last = format!("{target}: {e}"),
        }
    }
    warn!("resultado direto não chegou ao gateway ({last})");
    false
}

/// Entrega uma falha no lugar do resultado, para que o gateway nomeie o nó que
/// morreu em vez de esperar o prazo inteiro.
pub async fn deliver_failure(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    failure: crate::relay::RelayFailure,
) {
    let meta = json!({ "error": failure });
    deliver_result(http, reply_to, meta).await;
}

// ── Telemetria acumulada na ida ───────────────────────────────────────────────

/// A contribuição de um hop para o `hop_timings` que viaja **na ida**.
///
/// No anel borbulhado cada entrada era montada por quem chamou, na volta. Sem
/// volta, cada hop descreve a si mesmo e empurra a lista adiante; a cauda
/// entrega o conjunto junto do resultado, na mesma ordem cabeça→cauda que
/// `RequestTrace::record_hop_from_json` já espera.
///
/// `bytes_sent` é o quadro que **entrou** neste hop, que é exatamente o que o
/// anterior pôs no fio. `round_trip_ms` fica igual ao `processing_ms` porque a
/// perna de rede de cada hop não é observável por ninguém no modo direto: quem
/// manda não espera a resposta, e quem recebe não tem o relógio de quem
/// mandou. O gateway conhece o total, e é ele que lança o transporte que
/// sobrou — ver `credit_transport`.
pub fn hop_entry(processing_ms: &Value, node_timings: &Value, inbound_bytes: usize) -> Value {
    json!({
        "round_trip_ms": processing_ms.clone(),
        "processing_ms": processing_ms.clone(),
        "bytes_sent": inbound_bytes,
        "bytes_received": 0,
        "client_encode_ms": 0,
        "response_headers_ms": 0,
        "response_body_ms": 0,
        "client_decode_ms": 0,
        "node_timings": node_timings.clone(),
    })
}

/// Variant used by inference hops after the selected wire dtype is known.
/// `inbound_bytes` remains the complete frame size for transport telemetry;
/// the hidden-state counters isolate the tensor payload on either side of the
/// crossing. Direct mode records both sides because no caller waits for a
/// response frame from each hop.
pub fn hop_entry_with_state(
    processing_ms: &Value,
    node_timings: &Value,
    inbound_bytes: usize,
    hidden_state_bytes_received: usize,
    hidden_state_bytes_sent: usize,
    dtype: crate::wire::WireDtype,
) -> Value {
    let mut entry = hop_entry(processing_ms, node_timings, inbound_bytes);
    entry["hidden_state_bytes_sent"] = json!(hidden_state_bytes_sent);
    entry["hidden_state_bytes_received"] = json!(hidden_state_bytes_received);
    entry["hidden_state_dtype"] = json!(dtype.as_str());
    entry
}

/// Acrescenta a entrada deste hop à lista que veio na ida.
pub fn accumulate(inbound_meta: &Value, entry: Value) -> Vec<Value> {
    let mut hops: Vec<Value> = inbound_meta
        .get("hop_timings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    hops.push(entry);
    hops
}

/// Fecha a telemetria com o que só o gateway sabe: quanto a cadeia inteira
/// gastou de relógio e quantos bytes voltaram.
///
/// O transporte não atribuído (total menos a soma do que cada hop declarou de
/// si) vai para a entrada da cabeça. Não é a perna dela — é a cadeia inteira,
/// e está aí porque nenhuma perna é medível de dentro do modo direto. Somado,
/// o número continua certo; dividido por hop, ele não existe. O nome honesto
/// disso está no doc de `hop_entry`.
pub fn credit_transport(hops: &mut [Value], total_ms: u64, result_bytes: u64) {
    let declared: u64 = hops
        .iter()
        .map(|h| h.get("processing_ms").and_then(Value::as_u64).unwrap_or(0))
        .sum();
    if let Some(first) = hops.first_mut() {
        let own = first
            .get("processing_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        first["round_trip_ms"] = json!(own + total_ms.saturating_sub(declared));
    }
    if let Some(last) = hops.last_mut() {
        last["bytes_received"] = json!(result_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_delivered_result_reaches_the_ticket_that_asked_for_it() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register();
        assert_eq!(inbox.outstanding(), 1);

        assert!(inbox.deliver(&ticket.id().to_string(), json!({"token_id": 7})));
        assert_eq!(inbox.outstanding(), 0, "entregar consome o ticket");

        let got = (&mut ticket.rx).await.expect("resultado");
        assert_eq!(got["token_id"], 7);
    }

    /// O guarda tem que limpar o inbox sozinho: o caminho de erro e o de prazo
    /// estourado saem por `return`, e um ticket esquecido segura memória por
    /// requisição perdida.
    #[test]
    fn a_dropped_ticket_leaves_no_entry_behind() {
        let inbox = ReplyInbox::new();
        {
            let _ticket = inbox.register();
            assert_eq!(inbox.outstanding(), 1);
        }
        assert_eq!(inbox.outstanding(), 0);
    }

    /// Um hop que responde depois de o gateway ter desistido não pode ser
    /// confundido com o quadro seguinte, nem derrubar nada.
    #[test]
    fn a_late_result_is_refused_instead_of_landing_on_another_frame() {
        let inbox = ReplyInbox::new();
        let atrasado = {
            let ticket = inbox.register();
            ticket.id().to_string()
        };
        let _seguinte = inbox.register();

        assert!(!inbox.deliver(&atrasado, json!({"token_id": 1})));
        assert_eq!(inbox.outstanding(), 1, "o ticket seguinte fica intacto");
    }

    #[test]
    fn a_reply_url_never_points_at_the_wildcard_address() {
        let urls = self_reply_urls(
            None,
            vec![
                "0.0.0.0:42777".into(),
                "192.168.1.9:42777".into(),
                "[2804:f30::1]:42777".into(),
                "192.168.1.9:42777".into(),
            ],
            8080,
        );
        assert_eq!(
            urls,
            vec![
                "http://192.168.1.9:8080".to_string(),
                "http://[2804:f30::1]:8080".to_string(),
            ]
        );
    }

    /// Loopback é endereço da máquina de quem responde, não desta. Uma lista
    /// que só tem loopback tem que ficar vazia para `DirectReply::new` recusar
    /// o modo direto e o plano cair no anel, que funciona: ligado, o quadro
    /// sairia para nunca voltar e a requisição só terminaria no prazo de 600 s.
    #[test]
    fn a_loopback_only_host_list_falls_back_to_the_ring() {
        let urls = self_reply_urls(
            None,
            vec![
                "127.0.0.1:42777".into(),
                "127.0.0.53:42777".into(),
                "[::1]:42777".into(),
                "localhost:42777".into(),
            ],
            8080,
        );
        assert!(
            urls.is_empty(),
            "loopback não alcança outra máquina: {urls:?}"
        );
        assert!(
            DirectReply::new(ReplyInbox::new(), urls, "n1".into()).is_none(),
            "sem endereço utilizável o plano tem que cair no anel"
        );

        // O loopback que o operador fixou é outra história: numa máquina só,
        // apontar para si mesmo é a configuração certa e foi escolha dele.
        assert_eq!(
            self_reply_urls(
                Some("127.0.0.1:42777".into()),
                vec!["127.0.0.1:42777".into()],
                8080
            ),
            vec!["http://127.0.0.1:8080".to_string()]
        );
    }

    #[test]
    fn the_unattributed_transport_lands_in_the_total_exactly_once() {
        let mut hops = vec![
            hop_entry(&json!(10), &json!({}), 1_000),
            hop_entry(&json!(20), &json!({}), 2_000),
        ];
        credit_transport(&mut hops, 100, 87);

        // 100 de relógio, 30 declarados pelos hops → 70 de transporte.
        assert_eq!(hops[0]["round_trip_ms"], 80);
        assert_eq!(hops[1]["round_trip_ms"], 20);
        assert_eq!(hops[1]["bytes_received"], 87);
    }
}
