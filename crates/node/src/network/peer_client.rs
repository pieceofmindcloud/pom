//! Um `reqwest::Client` por peer, porque o verifier é por destino.
//!
//! Antes desta feature havia um cliente só para tudo. Com pinning por node_id,
//! o `rustls::ClientConfig` carrega **qual** node_id esperar, e ele é fixado na
//! construção do cliente - então o cliente passa a ser por destino. Sem TLS,
//! volta a ser um só, porque aí não há nada a fixar.
//!
//! *Alternativa recusada:* pôr o node_id no SNI (`<node_id>.pom`) e resolver
//! com `resolve_to_addrs`, o que permitiria um cliente único. As resoluções do
//! reqwest são estáticas na construção, então o ganho evapora num cluster onde
//! peers entram e saem.
//!
//! Tudo aqui é síncrono de propósito: o cache é um `HashMap` sob um lock da
//! `std`, e nenhuma seção crítica atravessa um `await`. Um lock assíncrono
//! obrigaria todo ponto de chamada peer a peer a virar `async` por causa de uma
//! busca em tabela hash.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Se este nó fala TLS com os peers, visível de qualquer lugar do processo.
///
/// É global porque a flag é propriedade **do nó**, não de uma conexão: os
/// construtores de URL do `PeerInfo` (`inference_candidates`, `gateway_url`)
/// precisam do esquema, e passá-lo por parâmetro em cada um deles espalharia um
/// argumento por dezenas de assinaturas para carregar um único bit que nunca
/// difere entre eles. Dois nós no mesmo processo só acontecem em teste.
static TLS_ATIVO: AtomicBool = AtomicBool::new(false);
static QUIC_ATIVO: AtomicBool = AtomicBool::new(false);

pub fn tls_ativo() -> bool {
    TLS_ATIVO.load(Ordering::Relaxed)
}

pub fn set_tls_ativo(on: bool) {
    TLS_ATIVO.store(on, Ordering::Relaxed);
}

pub fn quic_ativo() -> bool {
    QUIC_ATIVO.load(Ordering::Relaxed)
}

pub fn set_quic_ativo(on: bool) {
    QUIC_ATIVO.store(on, Ordering::Relaxed);
}

/// Esquema em vigor para falar com um peer.
pub fn peer_scheme() -> &'static str {
    scheme(tls_ativo())
}

/// Trava que serializa os testes que leem ou escrevem `TLS_ATIVO`.
///
/// O estado é global ao processo e os testes rodam em paralelo: sem isto, um
/// teste que liga a política faz outro, que monta URL de peer, ver `https://`
/// onde esperava `http://`. Já aconteceu. Todo teste que toque no esquema
/// global pega esta trava primeiro.
#[cfg(test)]
pub static POLITICA_GLOBAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `https` quando o cluster está cifrado, `http` quando não.
pub fn scheme(tls: bool) -> &'static str {
    if tls {
        "https"
    } else {
        "http"
    }
}

/// Estado observável do pool HTTP do peer.
///
/// O reqwest mantém o socket individual sob o pool interno, então esta
/// distinção identifica construção/reutilização do cliente pinado. Quando o
/// caminho usa o cliente claro compartilhado, a reutilização de um socket não
/// pode ser provada nesta camada e fica desconhecida.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionState {
    New,
    Reused,
    Unknown,
}

impl ConnectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Reused => "reused",
            Self::Unknown => "unknown",
        }
    }
}

/// Base de URL para falar com um peer, com o esquema que a flag manda.
///
/// O colchete do IPv6 é responsabilidade daqui: o cluster é IPv6 na prática, e
/// `http://2804:d55::1:8181` não parseia como URL - o parser lê o último grupo
/// como porta.
pub fn peer_base_url(host: &str, port: u16, tls: bool) -> String {
    let host = if host.starts_with('[') || !host.contains(':') {
        host.to_string()
    } else {
        format!("[{host}]")
    };
    format!("{}://{host}:{port}", scheme(tls))
}

#[derive(Clone)]
pub struct PeerClients {
    timeout: Duration,
    tls: Arc<AtomicBool>,
    quic: Arc<AtomicBool>,
    quic_pool: crate::quic::ClientPool,
    plain: reqwest::Client,
    internal_plain: reqwest::Client,
    per_peer: Arc<RwLock<HashMap<String, reqwest::Client>>>,
    reply_routes: Arc<RwLock<HashMap<String, ReplyRouteState>>>,
    auth: Arc<RwLock<Option<crate::auth::AuthState>>>,
}

#[derive(Default)]
struct ReplyRouteState {
    preferred: Option<String>,
    /// Falhas **seguidas** por endereço, zeradas por qualquer sucesso.
    streak: HashMap<String, u32>,
    /// Quando o endereço acumulou [`ROUTE_FAILURE_STREAK`] falhas seguidas.
    failed: HashMap<String, Instant>,
}

/// Quanto tempo um endereço fica no **fim** da ordem depois de acumular
/// falhas seguidas. Não é exclusão - ver [`PeerClients::route_order`].
///
/// É o mesmo prazo da quarentena do canal persistente
/// (`link::OPEN_FAILURE_COOLDOWN`, que aponta para cá): os dois governam a
/// mesma decisão - se vale a pena voltar a pagar este endereço - e dois prazos
/// diferentes para a mesma decisão só produzem a aparência de recuperação.
pub(crate) const ROUTE_FAILURE_COOLDOWN: Duration = Duration::from_secs(300);

/// Quantas falhas **seguidas** um endereço precisa acumular antes de ir para o
/// fim da ordem.
///
/// O teto de uma tentativa não é critério de quarentena. Uma entrega que
/// estourou o teto pode estar só lenta - gateway ocupado, handshake TLS frio,
/// lote grande, WAN - e uma amostra só não distingue "devagar" de "morto". Um
/// endereço realmente morto acumula as três em poucos milissegundos, porque o
/// `CONNECT_TIMEOUT` corta antes.
const ROUTE_FAILURE_STREAK: u32 = 3;

/// Um endereço anunciado que engole SYN não pode entregar o controle do
/// processo ao timeout do sistema operacional. O timeout total da requisição
/// continua sendo escolhido pelo chamador, mas a conexão tem teto próprio.
#[cfg(not(test))]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(test)]
const CONNECT_TIMEOUT: Duration = Duration::from_millis(50);

impl PeerClients {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            tls: Arc::new(AtomicBool::new(false)),
            quic: Arc::new(AtomicBool::new(false)),
            quic_pool: crate::quic::ClientPool::new(),
            plain: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(timeout)
                .build()
                .expect("cliente HTTP sem TLS não pode falhar na construção"),
            internal_plain: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(timeout)
                .default_headers(internal_node_headers())
                .build()
                .expect("cliente HTTP interno sem TLS não pode falhar na construção"),
            per_peer: Arc::new(RwLock::new(HashMap::new())),
            reply_routes: Arc::new(RwLock::new(HashMap::new())),
            auth: Arc::new(RwLock::new(None)),
        }
    }

    /// Liga este cache de clientes à configuração viva do nó. A leitura é
    /// feita por chamada, porque a chave pode ser regenerada pela API sem
    /// reiniciar o processo.
    pub fn set_auth_state(&self, auth: crate::auth::AuthState) {
        if let Ok(mut slot) = self.auth.write() {
            *slot = Some(auth);
        }
    }

    /// Return the shared peer credential for an internal inference request.
    /// Client API keys never cross this boundary.
    pub async fn internal_node_token(&self) -> Option<String> {
        let auth = self.auth.read().ok()?.clone()?;
        let config = auth.read().await;
        crate::auth::internal_node_token(&config)
    }

    pub async fn authorize_inference_request_for_session(
        &self,
        request: reqwest::RequestBuilder,
        session_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let token = self.internal_node_token().await;
        let request = match session_id {
            Some(id) if !id.is_empty() => request.header(crate::auth::INFERENCE_SESSION_HEADER, id),
            _ => request,
        };
        match token {
            Some(token) => {
                request.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            }
            None => request,
        }
    }

    pub async fn authorize_inference_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        match self.internal_node_token().await {
            Some(token) => {
                request.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            }
            None => request,
        }
    }

    pub fn tls_enabled(&self) -> bool {
        self.tls.load(Ordering::Relaxed)
    }

    pub fn quic_enabled(&self) -> bool {
        self.quic.load(Ordering::Relaxed)
    }

    /// Preferência efetiva do nó para novas travessias de inferência.
    ///
    /// A política do principal também é mantida no atômico global para
    /// alcançar os clientes lazy do servidor de inferência. Consultar os dois
    /// evita que um `PeerClients` criado antes da adoção da política continue
    /// escolhendo o túnel enquanto o restante do processo já escolhe QUIC.
    pub fn effective_quic_enabled(&self) -> bool {
        self.quic_enabled() || quic_ativo()
    }

    /// Atualiza a preferência sem tocar em clientes TCP já abertos. A política
    /// só escolhe chamadas novas; uma travessia em andamento conserva o canal
    /// que já adquiriu.
    pub fn set_quic(&self, on: bool) {
        self.quic.store(on, Ordering::Relaxed);
    }

    pub async fn quic_post(
        &self,
        peer_id: &str,
        base_url: &str,
        quic_addresses: &[String],
        path: &str,
        body: Vec<u8>,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> anyhow::Result<crate::quic::QuicResponse> {
        let token = self.internal_node_token().await;
        self.quic_pool
            .request(
                peer_id,
                base_url,
                quic_addresses,
                path,
                token,
                session_id,
                body,
                timeout,
            )
            .await
    }

    /// Vira a flag e joga fora os clientes: um cliente construído para falar
    /// claro nunca vai falar TLS, e o pool de conexões dele também não serve.
    pub fn set_tls(&self, on: bool) {
        let antes = self.tls.swap(on, Ordering::Relaxed);
        if antes != on {
            if let Ok(mut m) = self.per_peer.write() {
                m.clear();
            }
        }
    }

    /// Aplica a política do cluster: este cache **e** o esquema global das URLs.
    ///
    /// É o caminho da produção, e o único que mexe em `TLS_ATIVO`. `set_tls`
    /// fica só com o cache, para um teste de unidade não empurrar o esquema
    /// global para dentro dos testes que rodam em paralelo com ele.
    pub fn apply_cluster_policy(&self, on: bool) {
        set_tls_ativo(on);
        self.set_tls(on);
    }

    /// Cliente sem pinning, para destino que não é peer (loopback, Ollama).
    pub fn plain(&self) -> reqwest::Client {
        self.plain.clone()
    }

    /// Cliente exclusivo para sondas de latência.
    ///
    /// A sonda só precisa saber se o peer respondeu. Ela não autentica uma
    /// sessão de inferência e, portanto, não deve depender do certificado
    /// autoassinado nem do nome presente no certificado do peer. O cliente
    /// pinado de `for_peer` continua sendo o único caminho para dados de
    /// inferência.
    pub fn probe(&self) -> Result<reqwest::Client> {
        if !self.tls_enabled() {
            return Ok(self.internal_plain.clone());
        }
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(self.timeout)
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .context("não deu para construir o cliente TLS da sonda")
    }

    /// Cliente para falar com `node_id`, construído sob demanda.
    ///
    /// Sem TLS devolve o cliente interno compartilhado, que marca as chamadas
    /// de peer para a fronteira administrativa sem alterar a credencial delas.
    pub fn for_peer(&self, node_id: &str) -> Result<reqwest::Client> {
        self.for_peer_with_state(node_id).map(|(client, _)| client)
    }

    /// Busca uma rota de controle autenticada em todos os endereços anunciados
    /// para o mesmo peer.
    ///
    /// Um endereço antigo pode continuar aceitando TCP e responder 403 porque
    /// caiu em outro nó (ou num processo fantasma), portanto uma resposta HTTP
    /// de erro também precisa deixar a sonda tentar o próximo candidato. A
    /// identidade TLS continua fixada no peer_id para todas as tentativas.
    pub(crate) async fn get_internal_from_candidates(
        &self,
        peer_id: &str,
        base_urls: &[String],
        path: &str,
        connection_key: &str,
        extra_headers: &reqwest::header::HeaderMap,
    ) -> Result<reqwest::Response> {
        let http = self.for_peer(peer_id)?;
        let mut failures = Vec::with_capacity(base_urls.len());
        for base in base_urls {
            let url = format!("{base}{path}");
            let mut request = http.get(&url).headers(extra_headers.clone());
            if !connection_key.is_empty() {
                request = request.bearer_auth(connection_key);
            }
            match request.timeout(crate::webui::PEER_TIMEOUT).send().await {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => failures.push(format!("{url}: {}", response.status())),
                Err(error) => failures.push(format!("{url}: {error}")),
            }
        }
        anyhow::bail!(
            "nenhum candidato respondeu à rota interna {path}: {}",
            if failures.is_empty() {
                "nenhum endereço anunciado".to_string()
            } else {
                failures.join(", ")
            }
        )
    }

    /// Cliente e observação atômica da entrada no cache. A observação é feita
    /// junto da inserção para que chamadas concorrentes não rotulem como
    /// reutilizado um cliente que acabou de ser construído.
    pub fn for_peer_with_state(&self, node_id: &str) -> Result<(reqwest::Client, ConnectionState)> {
        if !self.tls_enabled() {
            return Ok((self.internal_plain.clone(), ConnectionState::Unknown));
        }
        if let Ok(m) = self.per_peer.read() {
            if let Some(c) = m.get(node_id) {
                return Ok((c.clone(), ConnectionState::Reused));
            }
        }
        let cfg = crate::tls::client_config(node_id)?;
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(self.timeout)
            .default_headers(internal_node_headers())
            .use_preconfigured_tls(cfg)
            .build()
            .context("não deu para construir o cliente TLS do peer")?;
        if let Ok(mut m) = self.per_peer.write() {
            m.insert(node_id.to_string(), client.clone());
        }
        Ok((client, ConnectionState::New))
    }

    /// Base de URL para este peer, já com o esquema em vigor.
    pub fn base_url(&self, host: &str, port: u16) -> String {
        peer_base_url(host, port, self.tls_enabled())
    }

    /// Peers com cliente pinado em cache.
    ///
    /// Existe para o teste: o `Debug` do `reqwest::Client` é igual para dois
    /// clientes diferentes, então comparar instâncias não prova nada. O que
    /// importa observar é o cache - quem entrou, e o que a virada da flag
    /// jogou fora.
    pub fn cached_peers(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .per_peer
            .read()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Ordena endereços de retorno: primeiro a rota que já respondeu, depois
    /// as demais, e por último as que vêm falhando - da que falhou há mais
    /// tempo para a mais recente.
    ///
    /// A rota é usada uma vez por lote de decode. Sem esta memória,
    /// um IPv4/IPv6 que aceita o SYN mas nunca responde é tentado de novo em
    /// cada lote e o custo do timeout vira custo por ficha.
    ///
    /// **Nunca devolve lista vazia para uma lista de candidatos não vazia.**
    /// A memória de falhas reordena, não exclui. Excluir era o defeito: com
    /// todos os candidatos em quarentena o chamador não tentava endereço
    /// nenhum, e sem tentativa não há erro para reportar - só o prazo do
    /// gateway a vencer (600 s) e um aviso com o motivo vazio. O que decide se
    /// um endereço serve é a tentativa real; esta função só escolhe a ordem em
    /// que ela é paga.
    pub(crate) fn route_order(&self, key: &str, candidates: &[String]) -> Vec<String> {
        let Ok(mut routes) = self.reply_routes.write() else {
            return candidates.to_vec();
        };
        let state = routes.entry(key.to_string()).or_default();
        let now = Instant::now();
        let known = |url: &String| candidates.iter().any(|candidate| candidate == url);
        let expired: Vec<String> = state
            .failed
            .iter()
            .filter(|(url, failed_at)| {
                known(url) && now.duration_since(**failed_at) >= ROUTE_FAILURE_COOLDOWN
            })
            .map(|(url, _)| url.clone())
            .collect();
        state.failed.retain(|url, failed_at| {
            known(url) && now.duration_since(*failed_at) < ROUTE_FAILURE_COOLDOWN
        });
        for url in expired {
            state.streak.remove(&url);
        }
        state.streak.retain(|url, _| known(url));

        let mut ordered = Vec::with_capacity(candidates.len());
        if let Some(preferred) = state
            .preferred
            .as_ref()
            .filter(|url| known(url) && !state.failed.contains_key(*url))
        {
            ordered.push(preferred.clone());
        }
        for candidate in candidates {
            if !ordered.iter().any(|url| url == candidate) && !state.failed.contains_key(candidate)
            {
                ordered.push(candidate.clone());
            }
        }
        let mut quarantined: Vec<String> = Vec::new();
        for candidate in candidates {
            if !ordered.iter().any(|url| url == candidate)
                && !quarantined.iter().any(|url| url == candidate)
            {
                quarantined.push(candidate.clone());
            }
        }
        // Quem falhou há mais tempo teve mais tempo para voltar, então é o
        // primeiro da fila dos degradados.
        quarantined.sort_by_key(|url| state.failed.get(url).copied());
        ordered.extend(quarantined);
        ordered
    }

    /// Regista uma falha neste endereço.
    ///
    /// A primeira falha só tira o endereço da preferência - ele continua na
    /// ordem, à frente de quem já vinha falhando. Só [`ROUTE_FAILURE_STREAK`]
    /// falhas seguidas o mandam para o fim.
    pub(crate) fn remember_route_failure(&self, key: &str, url: &str) {
        if let Ok(mut routes) = self.reply_routes.write() {
            let state = routes.entry(key.to_string()).or_default();
            let streak = {
                let counter = state.streak.entry(url.to_string()).or_insert(0);
                *counter = counter.saturating_add(1);
                *counter
            };
            if streak >= ROUTE_FAILURE_STREAK {
                state.failed.insert(url.to_string(), Instant::now());
            }
            if state.preferred.as_deref() == Some(url) {
                state.preferred = None;
            }
        }
    }

    pub(crate) fn remember_route_success(&self, key: &str, url: &str) {
        if let Ok(mut routes) = self.reply_routes.write() {
            let state = routes.entry(key.to_string()).or_default();
            state.streak.remove(url);
            state.failed.remove(url);
            state.preferred = Some(url.to_string());
        }
    }

    /// Quantas falhas seguidas este endereço acumulou. Existe para a bancada
    /// provar a escada "demove, depois degrada" sem esperar 300 s.
    #[cfg(test)]
    pub(crate) fn route_failure_streak(&self, key: &str, url: &str) -> u32 {
        self.reply_routes
            .read()
            .ok()
            .and_then(|routes| {
                routes
                    .get(key)
                    .and_then(|state| state.streak.get(url).copied())
            })
            .unwrap_or(0)
    }
}

fn internal_node_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        crate::auth::INTERNAL_NODE_REQUEST_HEADER,
        reqwest::header::HeaderValue::from_static("1"),
    );
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esquema_da_url_segue_a_flag() {
        assert_eq!(scheme(true), "https");
        assert_eq!(scheme(false), "http");
    }

    /// O defeito que pendurava a requisição por 600 s: com todos os candidatos
    /// em quarentena a ordem saía vazia, o `for` do chamador não executava e
    /// ninguém tentava endereço nenhum. Sem tentativa não há erro para
    /// reportar - só o prazo do gateway a vencer.
    #[test]
    fn candidatos_todos_em_quarentena_ainda_produzem_tentativas() {
        let c = PeerClients::new(Duration::from_secs(600));
        let candidates = vec!["http://a".to_string(), "http://b".to_string()];
        for url in &candidates {
            for _ in 0..ROUTE_FAILURE_STREAK {
                c.remember_route_failure("reply:peer", url);
            }
        }
        let ordered = c.route_order("reply:peer", &candidates);
        assert_eq!(
            ordered.len(),
            candidates.len(),
            "a memória de falhas reordena, não exclui: {ordered:?}"
        );
        for candidate in &candidates {
            assert!(
                ordered.contains(candidate),
                "{candidate} sumiu de {ordered:?}"
            );
        }
    }

    /// Quem falhou há mais tempo teve mais tempo para voltar, e por isso é o
    /// primeiro dos degradados.
    #[test]
    fn entre_degradados_o_mais_antigo_vem_primeiro() {
        let c = PeerClients::new(Duration::from_secs(600));
        let candidates = vec!["http://a".to_string(), "http://b".to_string()];
        for _ in 0..ROUTE_FAILURE_STREAK {
            c.remember_route_failure("reply:peer", "http://a");
        }
        std::thread::sleep(Duration::from_millis(5));
        for _ in 0..ROUTE_FAILURE_STREAK {
            c.remember_route_failure("reply:peer", "http://b");
        }
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://a".to_string(), "http://b".to_string()]
        );
    }

    /// Uma entrega apenas lenta não é uma rota morta: o teto de uma tentativa
    /// não pode ser o critério de quarentena. A primeira falha só tira o
    /// endereço da preferência.
    #[test]
    fn uma_falha_isolada_nao_degrada_a_rota() {
        let c = PeerClients::new(Duration::from_secs(600));
        let candidates = vec!["http://lento".to_string(), "http://outro".to_string()];
        c.remember_route_success("reply:peer", "http://lento");
        c.remember_route_failure("reply:peer", "http://lento");
        assert_eq!(c.route_failure_streak("reply:peer", "http://lento"), 1);
        // Perdeu a preferência, mas continua à frente de quem nunca respondeu?
        // Não: perder a preferência é ir para a ordem natural dos candidatos.
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://lento".to_string(), "http://outro".to_string()],
            "uma falha só não manda o endereço para o fim"
        );

        c.remember_route_failure("reply:peer", "http://lento");
        c.remember_route_failure("reply:peer", "http://lento");
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://outro".to_string(), "http://lento".to_string()],
            "três falhas seguidas mandam para o fim"
        );

        // E um sucesso zera a conta: a rota volta a ser a preferida.
        c.remember_route_success("reply:peer", "http://lento");
        assert_eq!(c.route_failure_streak("reply:peer", "http://lento"), 0);
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://lento".to_string(), "http://outro".to_string()]
        );
    }

    /// A rota que respondeu vem primeiro, e é isso - não a quarentena - que
    /// impede o caminho quente de voltar a pagar um endereço morto.
    #[test]
    fn a_rota_que_respondeu_vem_primeiro() {
        let c = PeerClients::new(Duration::from_secs(600));
        let candidates = vec!["http://morto".to_string(), "http://vivo".to_string()];
        c.remember_route_failure("reply:peer", "http://morto");
        c.remember_route_success("reply:peer", "http://vivo");
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://vivo".to_string(), "http://morto".to_string()]
        );
    }

    /// Candidato repetido não vira tentativa repetida.
    #[test]
    fn candidato_repetido_aparece_uma_vez() {
        let c = PeerClients::new(Duration::from_secs(600));
        let candidates = vec!["http://a".to_string(), "http://a".to_string()];
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://a".to_string()]
        );
        for _ in 0..ROUTE_FAILURE_STREAK {
            c.remember_route_failure("reply:peer", "http://a");
        }
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://a".to_string()]
        );
    }

    /// Sem candidato nenhum não há o que ordenar - e o chamador precisa dizer
    /// isso com todas as letras em vez de um motivo vazio.
    #[test]
    fn sem_candidatos_a_ordem_e_vazia() {
        let c = PeerClients::new(Duration::from_secs(600));
        assert!(c.route_order("reply:peer", &[]).is_empty());
    }

    #[test]
    fn url_de_peer_segue_a_flag_e_colcheta_ipv6() {
        assert_eq!(
            peer_base_url("192.168.1.5", 8181, false),
            "http://192.168.1.5:8181"
        );
        assert_eq!(
            peer_base_url("192.168.1.5", 8181, true),
            "https://192.168.1.5:8181"
        );
        // IPv6 sem colchete vira URL inválida, e o cluster é IPv6 na prática.
        assert_eq!(
            peer_base_url("2804:d55::1", 8181, true),
            "https://[2804:d55::1]:8181"
        );
        // Já colchetado não ganha um segundo par.
        assert_eq!(
            peer_base_url("[2804:d55::1]", 8181, true),
            "https://[2804:d55::1]:8181"
        );
    }

    /// Sem TLS, todo peer compartilha um cliente só - é o comportamento que
    /// existia antes desta feature, e não custa uma conexão nova por peer.
    #[test]
    fn sem_tls_ninguem_entra_no_cache() {
        let c = PeerClients::new(Duration::from_secs(600));
        assert!(!c.tls_enabled());
        c.for_peer("aaaa").unwrap();
        c.for_peer("bbbb").unwrap();
        assert!(
            c.cached_peers().is_empty(),
            "sem TLS todo mundo usa o cliente compartilhado"
        );
    }

    #[tokio::test]
    async fn cliente_de_peer_marca_controle_e_plain_nao() {
        async fn capture_headers(client: reqwest::Client) -> String {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let read = stream.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                String::from_utf8_lossy(&request).into_owned()
            });

            client
                .get(format!("http://{address}/health"))
                .send()
                .await
                .unwrap();
            server.await.unwrap()
        }

        let c = PeerClients::new(Duration::from_secs(600));
        let interno = capture_headers(c.for_peer("peer-a").unwrap()).await;
        assert!(interno.contains("x-pom-internal-node: 1"));

        let externo = capture_headers(c.plain()).await;
        assert!(!externo.contains("x-pom-internal-node:"));
    }

    /// Com TLS, cada peer precisa do seu, porque o verifier é fixado na
    /// construção do cliente e carrega o node_id esperado.
    #[test]
    fn com_tls_cada_peer_tem_o_seu_e_e_reaproveitado() {
        let c = PeerClients::new(Duration::from_secs(600));
        c.set_tls(true);
        let (_, first) = c.for_peer_with_state("aaaa").unwrap();
        let (_, second) = c.for_peer_with_state("aaaa").unwrap();
        let (_, third) = c.for_peer_with_state("bbbb").unwrap();
        assert_eq!(first, ConnectionState::New);
        assert_eq!(second, ConnectionState::Reused);
        assert_eq!(third, ConnectionState::New);
        assert_eq!(
            c.cached_peers(),
            vec!["aaaa".to_string(), "bbbb".to_string()]
        );
    }

    /// Virar a flag joga fora os clientes: um cliente construído para falar
    /// claro nunca vai falar TLS, e o pool de conexões dele também não serve.
    #[test]
    fn virar_a_flag_esvazia_o_cache() {
        let c = PeerClients::new(Duration::from_secs(600));
        c.set_tls(true);
        c.for_peer("aaaa").unwrap();
        assert_eq!(c.cached_peers(), vec!["aaaa".to_string()]);
        c.set_tls(false);
        assert!(c.cached_peers().is_empty());
    }
}
