//! libp2p request-response protocol carrying inference HTTP calls, as a
//! Transporte preferencial de inferência para peers alcançados pelo swarm.
//! Quando o túnel não está disponível ou falha por transporte, o chamador usa
//! o cliente direto pinado como fallback.
//!
//! Mirrors `bitswap.rs`'s shape ([4-byte BE length][JSON payload] framing
//! over an `AsyncRead`/`AsyncWrite` stream), but carries one generic
//! `{path, body}` pair instead of a fixed message enum — exactly mirroring
//! how `relay::send_frame` already takes `path: &str` generically across
//! `/v1/prefill`, `/v1/step`, `/v1/session/release`, `/v1/session/rollback`.
//! `body` is the same wire-encoded byte blob `send_frame` already builds;
//! this is purely an alternate transport for it, not a new format.

use async_trait::async_trait;
use futures::prelude::*;
use libp2p::{
    request_response::{self, Codec},
    PeerId, StreamProtocol,
};
use serde::{Deserialize, Serialize};
use std::io;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub const INFER_TUNNEL_PROTOCOL: &str = "/pom/infer-tunnel/1.0.0";

/// Qual servidor local atende a chamada que chegou pelo túnel.
///
/// O destino é escolhido pelo emissor mas **resolvido** pelo receptor: ele
/// mapeia a variante para uma das duas portas que ele próprio escolheu
/// (`inference_port`, `gateway_port`). Um par remoto nunca nomeia porta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunnelTarget {
    /// Servidor de inferência embutido (`/v1/prefill`, `/v1/step`, ...).
    #[default]
    Inference,
    /// Gateway: o plano de controle. Ver `gateway_path_is_allowed`.
    Gateway,
}

/// Rotas do gateway que o túnel entrega. **Lista fechada, conferida no
/// receptor.**
///
/// Encaminhar uma chamada do túnel para o gateway a faz chegar lá por
/// `127.0.0.1`, e o gateway usa o endereço do socket (`serve::PeerAddr`) para
/// decidir o que é local: `install_trust_anchor` libera a administração da
/// rede com base **só** nisso. Um túnel que aceitasse qualquer caminho daria a
/// qualquer peer do swarm um caminho para se tornar administrador. Por isso
/// aqui não há prefixo nem curinga: só as rotas de que a admissão e as
/// catracas de política precisam, cada uma já autenticada por conta própria.
/// Cabeçalho com que o encaminhador do túnel marca o pedido que entrega ao
/// gateway deste nó. O gateway (`serve::handle_conn`) só aceita a marca vinda
/// do loopback e com o segredo deste processo (`tunnel_mark`), e então trata o
/// pedido como remoto: tudo que sai do túnel chega por `127.0.0.1`, e sem a
/// marca o modo localhost daria ao par do outro lado a administração do nó.
pub(crate) const TUNNEL_ORIGIN_HEADER: &str = "x-pom-tunnel-origin";

/// Segredo aleatório deste processo para `TUNNEL_ORIGIN_HEADER`. Nunca sai do
/// processo: um cliente local não o conhece e um par remoto também não, e o
/// encaminhador sobrescreve qualquer valor que o par mande com esse nome.
pub(crate) fn tunnel_mark() -> &'static str {
    static MARK: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MARK.get_or_init(|| hex::encode(rand::random::<[u8; 32]>()))
}

pub(crate) fn is_tunnel_mark(value: &[u8]) -> bool {
    value == tunnel_mark().as_bytes()
}

pub fn gateway_path_is_allowed(path: &str) -> bool {
    // Query string não muda a rota, mas não pode servir para escapar da
    // comparação exata.
    let path = path.split(['?', '#']).next().unwrap_or(path);
    matches!(
        path,
        "/handshake"
            | "/health"
            | "/api/cluster/tls"
            | "/api/cluster/quic"
            | "/api/cluster/logs"
            | "/api/cluster/network-chat"
            | "/api/cluster/network-settings"
            // A volta da resposta direta. O gateway manda seus endereços no
            // `reply_to` e a cauda entrega ali; atrás de NAT nenhum deles é
            // alcançável a partir da outra ponta, e sem este caminho o quadro
            // morre calado no prazo do frame. Ambas as rotas
            // são autenticadas pelo ticket, que é único por quadro: entregar
            // sem ticket válido devolve 410 e não alcança sessão nenhuma.
            | "/v1/relay-result"
            | "/v1/remote-stream-result"
            // O plano de controle da implantação e das leituras do cluster, que
            // `GatewayState::control_get`/`control_post_json` mandam pelo
            // túnel quando nenhum endereço do peer responde - o caso de um
            // peer atrás de NAT simétrico. Seguro só porque o pedido chega
            // marcado (`TUNNEL_ORIGIN_HEADER`) e conta como remoto: cada rota
            // autentica o nó chamador pela assinatura de controle ou pela
            // chave de conexão, como faria numa chamada direta.
            | "/api/ui/node"
            | "/api/ui/models"
            | "/api/ui/model-metadata"
            | "/api/ui/model-identity"
            | "/api/ui/model-layer-options"
            | "/api/ui/slice-ranges"
            | "/api/ui/plan"
            | "/api/ui/apply"
            | "/api/ui/reconfigure"
            | "/api/ui/stop-inference"
            | "/api/ui/unload"
            | "/api/ui/leave"
            | "/api/ui/cache-types"
            | "/api/ui/deployments"
            | "/api/ui/deploy/capacity-preview"
            | "/api/ui/tp/check"
            | "/api/ui/network/rtt"
            | "/api/ui/network/chat"
            | "/api/internal/reload_model"
            // Administrative sync with the principal (`admin.rs`): API keys,
            // trust chain, succession and infer admission. A node that only
            // reaches the principal through the relay had no other path. All
            // of them require `InternalNodeAuth` (control signature or
            // connection key), never the socket's origin.
            | "/api/internal/api-keys/upsert"
            | "/api/internal/api-keys/revoke"
            | "/api/internal/api-keys/revoke-origin"
            | "/api/internal/api-keys/validate"
            | "/api/internal/admin-trust/reconcile"
            | "/api/internal/admin-trust/complete-transfer"
            | "/api/internal/profile/authorize-node"
            | "/api/internal/profile/accept-node"
            // A oferta de um arquivo: pequena e assinada como as outras. O
            // download que ela provoca continua sendo HTTP direto.
            | "/api/transfer/offer"
    )
}

/// A single generic inference call, forwarded to the receiving peer's own
/// loopback server chosen by `target` (`http://127.0.0.1:<porta><path>`)
/// once it arrives — see `discovery.rs`'s handling of
/// `DiscBehaviourEvent::InferTunnel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelRequest {
    /// Método HTTP a ser usado no loopback do peer. Peers antigos não enviam
    /// esse campo e continuam sendo tratados como chamadas POST.
    #[serde(default = "default_method")]
    pub method: String,
    /// Servidor local que atende. Ausente num peer de binário anterior, que só
    /// sabia falar com o servidor de inferência.
    #[serde(default)]
    pub target: TunnelTarget,
    pub path: String,
    pub body: Vec<u8>,
    /// Cabeçalhos necessários por rotas que não carregam seu metadado no corpo
    /// (por exemplo, a barreira do reduce TP).
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

fn default_method() -> String {
    "POST".to_string()
}

impl TunnelRequest {
    pub fn post(path: impl Into<String>, body: Vec<u8>, headers: Vec<(String, String)>) -> Self {
        Self {
            method: "POST".to_string(),
            target: TunnelTarget::Inference,
            path: path.into(),
            body,
            headers,
        }
    }

    pub fn get(path: impl Into<String>) -> Self {
        Self {
            method: "GET".to_string(),
            target: TunnelTarget::Inference,
            path: path.into(),
            body: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// Chamada do plano de controle, atendida pelo gateway do destino.
    pub fn gateway(
        method: &str,
        path: impl Into<String>,
        body: Vec<u8>,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            method: method.to_string(),
            target: TunnelTarget::Gateway,
            path: path.into(),
            body,
            headers,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Falha do request/response com a fronteira de entrega conhecida. Erros de
/// dial ocorrem antes de enviar o request; timeout ou conexão fechada depois
/// do despacho não provam que o handler deixou de executar o frame.
#[derive(Debug)]
pub struct TunnelDeliveryError {
    pub before_request: bool,
    message: String,
}

impl TunnelDeliveryError {
    pub fn before_request(message: impl Into<String>) -> Self {
        Self {
            before_request: true,
            message: message.into(),
        }
    }

    pub fn after_dispatch(message: impl Into<String>) -> Self {
        Self {
            before_request: false,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TunnelDeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TunnelDeliveryError {}

/// One outbound call dispatched to the swarm's own task (`Discovery::run`)
/// from outside its loop — the gateway in hub mode, or a relaying hop inside
/// `infer_server.rs` in ring mode. `Discovery::run` resolves `reply` by
/// matching the `OutboundRequestId` the swarm assigns when `send_request` is
/// called against the `Message::Response` / `Event::OutboundFailure` that
/// eventually arrives for it — see `relay::send_frame_via_tunnel`, the only
/// intended caller.
pub struct TunnelOutbound {
    pub peer_id: PeerId,
    /// Endereços UDP anunciados pelo destino, usados para reconstruir as
    /// multiaddresses TCP quando o swarm local ainda não recebeu o anúncio.
    pub addr_candidates: Vec<String>,
    pub request: TunnelRequest,
    pub reply: oneshot::Sender<Result<TunnelResponse, TunnelDeliveryError>>,
}

pub type TunnelOutboundSender = mpsc::Sender<TunnelOutbound>;

#[derive(Debug, Clone, Default)]
pub struct InferTunnelCodec;

/// A tunnel request/response carries exactly one wire-encoded frame
/// (`crate::wire`), so it must not be allowed to grow past what that frame
/// format itself already permits — same bound `send_frame` enforces.
const MAX_TUNNEL_PAYLOAD_BYTES: usize = crate::wire::MAX_FRAME_PAYLOAD_BYTES;

async fn read_msg<T, R>(io: &mut R) -> io::Result<T>
where
    T: serde::de::DeserializeOwned,
    R: AsyncRead + Unpin + Send,
{
    use futures::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_TUNNEL_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "tunnel frame of {len} bytes exceeds the {MAX_TUNNEL_PAYLOAD_BYTES}-byte bound"
            ),
        ));
    }
    let mut data = vec![0u8; len];
    io.read_exact(&mut data).await?;
    serde_json::from_slice(&data)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

async fn write_msg<T, W>(io: &mut W, msg: &T) -> io::Result<()>
where
    T: Serialize,
    W: AsyncWrite + Unpin + Send,
{
    use futures::AsyncWriteExt;
    let data = serde_json::to_vec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    io.write_all(&(data.len() as u32).to_be_bytes()).await?;
    io.write_all(&data).await?;
    Ok(())
}

#[async_trait]
impl Codec for InferTunnelCodec {
    type Protocol = StreamProtocol;
    type Request = TunnelRequest;
    type Response = TunnelResponse;

    async fn read_request<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_msg(io).await
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_msg(io).await
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_msg(io, &req).await
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_msg(io, &res).await
    }
}

/// Create a `request_response::Behaviour` for the infer-tunnel protocol.
/// Same request-timeout policy as bitswap's — this rides the same swarm and
/// its connections are just as long-lived.
pub fn infer_tunnel_behaviour() -> request_response::Behaviour<InferTunnelCodec> {
    request_response::Behaviour::new(
        [(
            StreamProtocol::new(INFER_TUNNEL_PROTOCOL),
            request_response::ProtocolSupport::Full,
        )],
        // Teto de segurança, não o prazo de ninguém: cada chamador impõe o seu
        // (`tunnel_gateway_call`, `send_frame_via_tunnel`). Chamadas de
        // controle como `/api/ui/slice-ranges` esperam até uma hora pelo
        // fatiamento; com 60 s aqui, elas morriam no túnel com o trabalho
        // ainda correndo do outro lado.
        request_response::Config::default().with_request_timeout(Duration::from_secs(3600)),
    )
}

static TUNNEL_SENDER: std::sync::OnceLock<TunnelOutboundSender> = std::sync::OnceLock::new();
static LOCAL_NODE_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Registra a identidade de descoberta deste processo para que os chamadores
/// de relay não tentem abrir um túnel para o próprio nó.
pub fn set_local_node_id(node_id: String) {
    let _ = LOCAL_NODE_ID.set(node_id);
}

/// O `node_id` deste processo, quando já anotado.
pub fn local_node_id() -> Option<&'static str> {
    LOCAL_NODE_ID.get().map(String::as_str)
}

/// True quando o destino é este processo, não um peer remoto.
pub fn is_local_node(node_id: &str) -> bool {
    LOCAL_NODE_ID.get().is_some_and(|local| local == node_id)
}

/// Set once, from `main.rs`, right after the channel is created (before
/// `Discovery::new` is even called — see that call site's comment). The only
/// way `infer_server.rs`'s ring-relay code, which holds no `GatewayState`
/// handle at all, can reach the tunnel — mirrors that module's existing
/// `http_client()` lazy-static for the exact same reason. A second call is a
/// bug (only one such channel is ever created per process) and is
/// intentionally dropped rather than panicking, matching this crate's
/// existing "config wins once, don't crash on a redundant set" style.
pub fn set_tunnel_sender(tx: TunnelOutboundSender) {
    let _ = TUNNEL_SENDER.set(tx);
}

/// `None` only if `set_tunnel_sender` was never called at all — in practice
/// that never happens on the path that constructs `infer_server.rs`'s state,
/// since `main.rs` sets it unconditionally before libp2p discovery is even
/// attempted. A tunnel call made through this sender still degrades
/// correctly (fast, documented "channel closed" error) when libp2p itself
/// never came up — see the doc comment on the channel's construction site.
pub fn tunnel_sender() -> Option<&'static TunnelOutboundSender> {
    TUNNEL_SENDER.get()
}

/// Mede a latência de ida e volta entre este nó e um peer pelo mesmo caminho
/// libp2p/Noise usado para transportar os frames de inferência.
///
/// O endpoint remoto é `/health` no loopback do peer. A medição inclui a fila
/// do swarm, eventual estabelecimento da conexão, o transporte, o
/// encaminhamento local e o processamento do handler `/health`, sem carregar o
/// custo de prefill, decode ou transferência de estado oculto. A medição é
/// direcional: cada processo registra apenas o tempo da sua própria origem.
pub async fn measure_rtt(
    sender: &TunnelOutboundSender,
    peer_id: PeerId,
    timeout: Duration,
) -> Option<f64> {
    let (reply, response) = oneshot::channel();
    let started = std::time::Instant::now();
    sender
        .send(TunnelOutbound {
            peer_id,
            addr_candidates: Vec::new(),
            request: TunnelRequest::get("/health"),
            reply,
        })
        .await
        .ok()?;

    let Ok(Ok(Ok(response))) = tokio::time::timeout(timeout, response).await else {
        return None;
    };
    if !(200..300).contains(&response.status) {
        return None;
    }
    Some(started.elapsed().as_secs_f64() * 1000.0)
}

/// `GET /health` do servidor de inferência de um peer pelo túnel, e o corpo
/// quando ele responde 2xx. É a sonda de admissão ao plano
/// (`gateway::ready_for_pipeline`) de um peer que nenhum endereço alcança.
pub(crate) async fn health_via_tunnel(
    peer: &crate::peer_client::TunnelPeer,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let sender = tunnel_sender()?;
    let (reply, response) = oneshot::channel();
    sender
        .send(TunnelOutbound {
            peer_id: peer.peer_id,
            addr_candidates: peer.addrs.clone(),
            request: TunnelRequest::get("/health"),
            reply,
        })
        .await
        .ok()?;
    let Ok(Ok(Ok(response))) = tokio::time::timeout(timeout, response).await else {
        return None;
    };
    if !(200..300).contains(&response.status) {
        return None;
    }
    serde_json::from_slice(&response.body).ok()
}

/// A real swarm speaking this protocol, wired to a real `TunnelOutboundSender`:
/// the same pairing `Discovery::run` provides in production, reduced to the one
/// behaviour under test.
///
/// Exists so the live probes (`tunnel_reaches_a_live_peer` here, and
/// `relay::tests::the_tunnel_carries_a_real_frame_to_a_live_peer`) drive
/// production code against a machine that is actually on the network, instead
/// of a fake channel. Test-only: production always goes through `Discovery`,
/// which owns the full behaviour set.
#[cfg(test)]
pub fn live_probe_swarm(peer: PeerId, addr: libp2p::Multiaddr) -> TunnelOutboundSender {
    use libp2p::{noise, request_response::Event, swarm::SwarmEvent, tcp, yamux};
    use std::collections::HashMap;

    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )
        .expect("tcp")
        .with_dns()
        .expect("dns")
        .with_behaviour(|_| infer_tunnel_behaviour())
        .expect("behaviour")
        // Mesmo tempo ocioso do enxame de produção (`Discovery::new`). O
        // padrão do `SwarmBuilder` é zero: a conexão fecha assim que fica
        // parada, e um desvio que só chega depois do HTTP direto expirar
        // encontraria o caminho já derrubado.
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();
    swarm.add_peer_address(peer, addr.clone());
    swarm.dial(addr).expect("dial");

    let (tx, mut rx) = mpsc::channel::<TunnelOutbound>(8);
    tokio::spawn(async move {
        let mut pending: HashMap<_, oneshot::Sender<Result<TunnelResponse, TunnelDeliveryError>>> =
            HashMap::new();
        loop {
            tokio::select! {
                out = rx.recv() => {
                    let Some(TunnelOutbound { peer_id, request, reply, .. }) = out else { break };
                    eprintln!("[probe-swarm] send_request -> {peer_id}");
                    let id = swarm.behaviour_mut().send_request(&peer_id, request);
                    pending.insert(id, reply);
                }
                event = swarm.select_next_some() => match event {
                    SwarmEvent::Behaviour(Event::Message {
                        message: request_response::Message::Response { request_id, response },
                        ..
                    }) => {
                        if let Some(reply) = pending.remove(&request_id) {
                            let _ = reply.send(Ok(response));
                        }
                    }
                    SwarmEvent::Behaviour(Event::OutboundFailure { request_id, error, .. }) => {
                        if let Some(reply) = pending.remove(&request_id) {
                            let before_request = matches!(
                                &error,
                                request_response::OutboundFailure::DialFailure
                            );
                            let failure = if before_request {
                                TunnelDeliveryError::before_request(error.to_string())
                            } else {
                                TunnelDeliveryError::after_dispatch(error.to_string())
                            };
                            let _ = reply.send(Err(failure));
                        }
                    }
                    other => eprintln!("[probe-swarm] {other:?}"),
                },
            }
        }
    });
    tx
}

/// The entire inbound side of the NAT-tunnel: forward a request that arrived
/// over the swarm to this node's own inference server on loopback, and carry
/// back whatever it answers.
///
/// Deliberately not a live `SharedState` handle — `InferState` has no stable
/// external handle to hold (it's destroyed and recreated on every model
/// reconfiguration by `NodeRuntime`'s supervisor), so keeping this task in
/// sync with that would be its own ongoing-maintenance burden. The always-
/// current, always-correct entry point stays the real Axum server; this is
/// just a same-machine hop in front of it, at a sub-millisecond cost.
///
/// Frames de inferência continuam sendo POSTs. A única exceção é o GET curto
/// de `/health` usado pela medição direcional de RTT.
/// The status `serve_loopback` answers when the peer's own server is not
/// listening: the tunnel reached the peer, the destination did not exist.
pub(crate) const NOTHING_LISTENING_STATUS: u16 = 502;

pub async fn serve_loopback(
    http: &reqwest::Client,
    port: u16,
    request: TunnelRequest,
) -> TunnelResponse {
    // Sempre texto claro no loopback: `serve::plain_allowed` libera loopback
    // mesmo com o cluster em TLS estrito, e o certificado do nó não cobre
    // `127.0.0.1`.
    let url = format!("http://127.0.0.1:{port}{}", request.path);
    let has_content_type = request
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(reqwest::header::CONTENT_TYPE.as_str()));
    // Método desconhecido cai em POST, o comportamento antigo para qualquer
    // coisa que não fosse GET.
    let method = reqwest::Method::from_bytes(request.method.to_ascii_uppercase().as_bytes())
        .unwrap_or(reqwest::Method::POST);
    let mut request_builder = if method == reqwest::Method::GET {
        http.get(&url)
    } else {
        let builder = http.request(method, &url).body(request.body);
        if has_content_type {
            builder
        } else {
            builder.header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        }
    };
    for (name, value) in request.headers {
        if name.eq_ignore_ascii_case(TUNNEL_ORIGIN_HEADER) {
            continue;
        }
        request_builder = request_builder.header(name, value);
    }
    let sent = request_builder
        .header(TUNNEL_ORIGIN_HEADER, tunnel_mark())
        .send()
        .await;

    match sent {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
            TunnelResponse { status, body }
        }
        // The peer's own inference server isn't reachable even on its own
        // loopback (not yet started, mid-reconfiguration, crashed) — a
        // synthetic 502 lets the caller's fallback logic tell this apart
        // from a successful-but-erroring response, without inventing a new
        // status code space just for the tunnel.
        Err(e) => TunnelResponse {
            status: NOTHING_LISTENING_STATUS,
            body: e.to_string().into_bytes(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A escalada que a lista fechada existe para impedir: `install_trust_anchor`
    /// libera a administração da rede olhando só se o socket é loopback, e
    /// tudo que sai do túnel chega ao gateway por `127.0.0.1`.
    #[test]
    fn o_tunel_nao_carrega_rota_administrativa() {
        assert!(!gateway_path_is_allowed("/api/ui/trust/anchor"));
        assert!(!gateway_path_is_allowed("/api/ui/network/trust-anchor"));
        assert!(!gateway_path_is_allowed("/admin/config"));
        assert!(!gateway_path_is_allowed("/admin/nodes"));
        assert!(!gateway_path_is_allowed("/api/generate"));
        assert!(!gateway_path_is_allowed("/"));
    }

    /// O plano de controle da implantação passa, e só porque o pedido chega
    /// marcado como remoto (ver `serve::request_origin`).
    #[test]
    fn o_tunel_carrega_o_plano_de_controle_da_implantacao() {
        assert!(gateway_path_is_allowed("/api/ui/node"));
        assert!(gateway_path_is_allowed("/api/ui/apply"));
        assert!(gateway_path_is_allowed("/api/ui/deployments?local=true"));
    }

    /// Administrative sync with the principal passes: a node that only reaches
    /// the principal through the relay must register the Chat key. The other
    /// internal routes stay out.
    #[test]
    fn o_tunel_carrega_a_sincronizacao_com_o_principal() {
        for path in [
            "/api/internal/api-keys/upsert",
            "/api/internal/api-keys/revoke",
            "/api/internal/api-keys/revoke-origin",
            "/api/internal/api-keys/validate",
            "/api/internal/admin-trust/reconcile",
            "/api/internal/admin-trust/complete-transfer",
            "/api/internal/profile/authorize-node",
            "/api/internal/profile/accept-node",
        ] {
            assert!(gateway_path_is_allowed(path), "{path}");
        }
        assert!(!gateway_path_is_allowed(
            "/api/internal/enterprise/fingerprint"
        ));
        assert!(!gateway_path_is_allowed("/api/internal/api-keys/upsert/x"));
    }

    /// A marca é um segredo do processo: constante enquanto ele vive e nunca
    /// um valor que um par do outro lado adivinharia.
    #[test]
    fn a_marca_do_tunel_e_um_segredo_estavel() {
        assert_eq!(tunnel_mark(), tunnel_mark());
        assert_eq!(tunnel_mark().len(), 64);
        assert!(is_tunnel_mark(tunnel_mark().as_bytes()));
        assert!(!is_tunnel_mark(b""));
        assert!(!is_tunnel_mark(b"1"));
    }

    /// As rotas de que a admissão e as catracas de política dependem. Sem
    /// `/handshake` aqui, um par atrás de NAT nunca entra na tabela `peers`.
    #[test]
    fn o_tunel_carrega_o_plano_de_controle() {
        for path in [
            "/handshake",
            "/health",
            "/api/cluster/tls",
            "/api/cluster/quic",
            "/api/cluster/logs",
            "/api/cluster/network-chat",
            "/api/cluster/network-settings",
            "/v1/relay-result",
            "/v1/remote-stream-result",
        ] {
            assert!(gateway_path_is_allowed(path), "{path} devia passar");
        }
    }

    /// Comparação exata, não prefixo: um sufixo não pode pendurar outra rota
    /// numa permitida, e query string não pode servir de escape.
    #[test]
    fn a_comparacao_da_lista_e_exata() {
        assert!(!gateway_path_is_allowed(
            "/handshake/../api/ui/trust/anchor"
        ));
        assert!(!gateway_path_is_allowed("/health/../admin"));
        assert!(!gateway_path_is_allowed("/api/cluster/tlsx"));
        assert!(gateway_path_is_allowed("/api/cluster/tls?verbose=1"));
    }

    /// Um peer de binário anterior não manda `target`, e só sabia falar com o
    /// servidor de inferência.
    #[test]
    fn um_pedido_sem_target_vai_para_a_inferencia() {
        let json = br#"{"path":"/v1/step","body":[]}"#;
        let request: TunnelRequest = serde_json::from_slice(json).expect("parse");
        assert_eq!(request.target, TunnelTarget::Inference);
        assert_eq!(request.method, "POST");
    }
    use futures::io::Cursor;

    /// A request/response has to survive the exact framing a real
    /// `AsyncRead`/`AsyncWrite` stream would carry it over — this drives the
    /// codec through an in-memory buffer instead of a real libp2p stream,
    /// the same spirit as `wire.rs`'s own round-trip tests.
    #[tokio::test]
    async fn a_tunnel_request_round_trips_through_the_codec() {
        let mut codec = InferTunnelCodec;
        let protocol = StreamProtocol::new(INFER_TUNNEL_PROTOCOL);
        let req = TunnelRequest::post("/v1/prefill", vec![1, 2, 3, 4, 5], Vec::new());

        let mut buf = Vec::new();
        codec
            .write_request(&protocol, &mut buf, req.clone())
            .await
            .expect("write_request");

        let mut cursor = Cursor::new(buf);
        let decoded = codec
            .read_request(&protocol, &mut cursor)
            .await
            .expect("read_request");

        assert_eq!(decoded.path, req.path);
        assert_eq!(decoded.body, req.body);
    }

    #[tokio::test]
    async fn a_tunnel_response_round_trips_through_the_codec() {
        let mut codec = InferTunnelCodec;
        let protocol = StreamProtocol::new(INFER_TUNNEL_PROTOCOL);
        let res = TunnelResponse {
            status: 200,
            body: b"hello from the other side".to_vec(),
        };

        let mut buf = Vec::new();
        codec
            .write_response(&protocol, &mut buf, res.clone())
            .await
            .expect("write_response");

        let mut cursor = Cursor::new(buf);
        let decoded = codec
            .read_response(&protocol, &mut cursor)
            .await
            .expect("read_response");

        assert_eq!(decoded.status, res.status);
        assert_eq!(decoded.body, res.body);
    }

    /// An empty body (a `GET` with nothing to send) is a real case — `/health`
    /// checks look exactly like this — and must not be confused with "no
    /// frame at all".
    #[tokio::test]
    async fn an_empty_body_round_trips_cleanly() {
        let mut codec = InferTunnelCodec;
        let protocol = StreamProtocol::new(INFER_TUNNEL_PROTOCOL);
        let req = TunnelRequest::get("/health");

        let mut buf = Vec::new();
        codec
            .write_request(&protocol, &mut buf, req.clone())
            .await
            .expect("write_request");

        let mut cursor = Cursor::new(buf);
        let decoded = codec
            .read_request(&protocol, &mut cursor)
            .await
            .expect("read_request");

        assert_eq!(decoded.path, "/health");
        assert_eq!(decoded.method, "GET");
        assert!(decoded.body.is_empty());
    }

    /// A declared length past the wire's own frame bound must be refused
    /// before the codec allocates a buffer for it — the same defence
    /// `wire.rs::read_frame` has for a hostile or corrupted declared size.
    #[tokio::test]
    async fn a_frame_declaring_more_than_the_bound_is_refused() {
        let mut codec = InferTunnelCodec;
        let protocol = StreamProtocol::new(INFER_TUNNEL_PROTOCOL);

        let oversized_len = (MAX_TUNNEL_PAYLOAD_BYTES as u32) + 1;
        let mut buf = oversized_len.to_be_bytes().to_vec();
        buf.extend_from_slice(b"doesn't matter, never read");

        let mut cursor = Cursor::new(buf);
        let result = codec.read_request(&protocol, &mut cursor).await;

        assert!(result.is_err(), "oversized frame must be refused");
    }

    /// `serve_loopback` against a real (in-process, real TCP on loopback)
    /// fake inference server — a genuine HTTP round trip, not a mock, since
    /// that's exactly the "new glue code" (URL construction, status/body
    /// carry-through) a bug here would hide in.
    async fn spawn_fake_inference_server(
        handler: impl Fn(Vec<u8>) -> (u16, Vec<u8>) + Clone + Send + Sync + 'static,
    ) -> u16 {
        use axum::{body::Bytes, routing::post, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new().route(
            "/v1/step",
            post(move |body: Bytes| {
                let handler = handler.clone();
                async move {
                    let (status, body) = handler(body.to_vec());
                    (axum::http::StatusCode::from_u16(status).unwrap(), body)
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        port
    }

    async fn spawn_fake_health_server() -> u16 {
        use axum::{routing::get, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new().route("/health", get(|| async { "ok" }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        port
    }

    #[tokio::test]
    async fn serve_loopback_uses_get_for_the_rtt_probe() {
        let port = spawn_fake_health_server().await;
        let response =
            serve_loopback(&reqwest::Client::new(), port, TunnelRequest::get("/health")).await;

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
    }

    #[tokio::test]
    async fn serve_loopback_forwards_the_body_and_carries_back_the_response() {
        let port = spawn_fake_inference_server(|body| {
            assert_eq!(body, b"the request body");
            (200, b"the response body".to_vec())
        })
        .await;

        let http = reqwest::Client::new();
        let response = serve_loopback(
            &http,
            port,
            TunnelRequest::post("/v1/step", b"the request body".to_vec(), Vec::new()),
        )
        .await;

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"the response body");
    }

    #[tokio::test]
    async fn serve_loopback_preserves_json_content_type_for_lifecycle_calls() {
        use axum::{
            body::Bytes,
            http::{HeaderMap, StatusCode},
            routing::post,
            Router,
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new().route(
            "/v1/session/release",
            post(|headers: HeaderMap, body: Bytes| async move {
                let content_type = headers
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok());
                if content_type == Some("application/json")
                    && body.as_ref() == br#"{"session_id":"s"}"#
                {
                    StatusCode::OK
                } else {
                    StatusCode::UNSUPPORTED_MEDIA_TYPE
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let response = serve_loopback(
            &reqwest::Client::new(),
            port,
            TunnelRequest::post(
                "/v1/session/release",
                br#"{"session_id":"s"}"#.to_vec(),
                vec![(
                    reqwest::header::CONTENT_TYPE.as_str().to_string(),
                    "application/json".to_string(),
                )],
            ),
        )
        .await;

        assert_eq!(response.status, 200);
    }

    #[tokio::test]
    async fn serve_loopback_carries_back_a_non_success_status_unchanged() {
        let port = spawn_fake_inference_server(|_body| (503, b"kv cache busy".to_vec())).await;

        let http = reqwest::Client::new();
        let response = serve_loopback(
            &http,
            port,
            TunnelRequest::post("/v1/step", vec![], Vec::new()),
        )
        .await;

        assert_eq!(response.status, 503);
        assert_eq!(response.body, b"kv cache busy");
    }

    /// No inference server listening at all on that port — the loopback hop
    /// itself failed, not the model. The caller needs a way to tell this
    /// apart from a real (successfully reached) error response.
    #[tokio::test]
    async fn serve_loopback_reports_a_synthetic_502_when_nothing_is_listening() {
        let http = reqwest::Client::new();
        // Port 0 alone never accepts a connection; using the OS-assigned
        // port from a listener we immediately drop guarantees nothing is
        // bound there without racing a real port number.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let response = serve_loopback(
            &http,
            port,
            TunnelRequest::post("/v1/step", vec![], Vec::new()),
        )
        .await;

        assert_eq!(response.status, 502);
    }

    /// Fala o protocolo do túnel contra um peer **de verdade**, pela internet.
    ///
    /// O commit que criou o túnel registrou como lacuna conhecida que ele só
    /// tinha sido exercitado em loopback e com enxames falsos. Isto é o que
    /// fecha essa lacuna, e não pode virar teste de CI: depende de uma máquina
    /// remota estar no ar. Fica `#[ignore]` e é dirigido por variável de
    /// ambiente, como procedimento reproduzível.
    ///
    /// ```sh
    /// POM_TUNNEL_PROBE=/ip6/2804:f30:3:1a6f:96e1:21bd:f074:967/tcp/52777/p2p/12D3KooWRuq6eW5rXSTa87FgrrXrAHdYCGYv3vKuLxtiFn4vfTRM \
    ///   cargo test -p node --bin pom tunnel_reaches_a_live_peer -- --ignored --nocapture
    /// ```
    ///
    /// O multiaddr sai do `libp2p_peer_id` que o peer anuncia mais o endereço
    /// dele com a porta libp2p (UDP + 10000). Uma resposta qualquer já prova o
    /// caminho: o 502 sintético de `serve_loopback` é gerado **na outra
    /// máquina**, então até ele prova que o quadro atravessou.
    #[tokio::test]
    #[ignore = "precisa de um peer vivo; ver o doc do teste"]
    async fn tunnel_reaches_a_live_peer() {
        use libp2p::{noise, request_response::Event, swarm::SwarmEvent, tcp, yamux, Multiaddr};

        let Ok(target) = std::env::var("POM_TUNNEL_PROBE") else {
            panic!("defina POM_TUNNEL_PROBE com o multiaddr do peer");
        };
        let addr: Multiaddr = target.parse().expect("multiaddr inválido");
        let peer_id = addr
            .iter()
            .find_map(|p| match p {
                libp2p::multiaddr::Protocol::P2p(id) => Some(id),
                _ => None,
            })
            .expect("o multiaddr tem que terminar em /p2p/<peer-id>");
        let path =
            std::env::var("POM_TUNNEL_PROBE_PATH").unwrap_or_else(|_| "/v1/step".to_string());

        let mut swarm = libp2p::SwarmBuilder::with_new_identity()
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                yamux::Config::default,
            )
            .expect("tcp")
            .with_dns()
            .expect("dns")
            .with_behaviour(|_| infer_tunnel_behaviour())
            .expect("behaviour")
            .build();

        swarm.add_peer_address(peer_id, addr.clone());
        swarm.dial(addr).expect("dial");
        let id = swarm
            .behaviour_mut()
            .send_request(&peer_id, TunnelRequest::get(path.clone()));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        loop {
            let event = tokio::time::timeout_at(deadline, swarm.select_next_some())
                .await
                .expect("o peer não respondeu no prazo");
            match event {
                SwarmEvent::Behaviour(Event::Message {
                    message:
                        request_response::Message::Response {
                            request_id,
                            response,
                        },
                    ..
                }) if request_id == id => {
                    println!(
                        "túnel respondeu: status={} body={:?}",
                        response.status,
                        String::from_utf8_lossy(&response.body[..response.body.len().min(200)])
                    );
                    return;
                }
                SwarmEvent::Behaviour(Event::OutboundFailure { error, .. }) => {
                    panic!("o túnel falhou: {error}");
                }
                SwarmEvent::OutgoingConnectionError { error, .. } => {
                    panic!("não deu para conectar: {error}");
                }
                other => println!("… {other:?}"),
            }
        }
    }
}
