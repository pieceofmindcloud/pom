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
use std::sync::RwLock;
use std::time::Duration;

/// Se este nó fala TLS com os peers, visível de qualquer lugar do processo.
///
/// É global porque a flag é propriedade **do nó**, não de uma conexão: os
/// construtores de URL do `PeerInfo` (`inference_candidates`, `gateway_url`)
/// precisam do esquema, e passá-lo por parâmetro em cada um deles espalharia um
/// argumento por dezenas de assinaturas para carregar um único bit que nunca
/// difere entre eles. Dois nós no mesmo processo só acontecem em teste.
static TLS_ATIVO: AtomicBool = AtomicBool::new(false);

pub fn tls_ativo() -> bool {
    TLS_ATIVO.load(Ordering::Relaxed)
}

pub fn set_tls_ativo(on: bool) {
    TLS_ATIVO.store(on, Ordering::Relaxed);
}

/// Esquema em vigor para falar com um peer.
pub fn peer_scheme() -> &'static str {
    scheme(tls_ativo())
}

/// `https` quando o cluster está cifrado, `http` quando não.
pub fn scheme(tls: bool) -> &'static str {
    if tls {
        "https"
    } else {
        "http"
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

pub struct PeerClients {
    timeout: Duration,
    tls: AtomicBool,
    plain: reqwest::Client,
    per_peer: RwLock<HashMap<String, reqwest::Client>>,
}

impl PeerClients {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            tls: AtomicBool::new(false),
            plain: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("cliente HTTP sem TLS não pode falhar na construção"),
            per_peer: RwLock::new(HashMap::new()),
        }
    }

    pub fn tls_enabled(&self) -> bool {
        self.tls.load(Ordering::Relaxed)
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

    /// Cliente para falar com `node_id`, construído sob demanda.
    ///
    /// Sem TLS devolve o cliente compartilhado, que é exatamente o
    /// comportamento anterior a esta feature.
    pub fn for_peer(&self, node_id: &str) -> Result<reqwest::Client> {
        if !self.tls_enabled() {
            return Ok(self.plain.clone());
        }
        if let Ok(m) = self.per_peer.read() {
            if let Some(c) = m.get(node_id) {
                return Ok(c.clone());
            }
        }
        let cfg = crate::tls::client_config(node_id)?;
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .use_preconfigured_tls(cfg)
            .build()
            .context("não deu para construir o cliente TLS do peer")?;
        if let Ok(mut m) = self.per_peer.write() {
            m.insert(node_id.to_string(), client.clone());
        }
        Ok(client)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esquema_da_url_segue_a_flag() {
        assert_eq!(scheme(true), "https");
        assert_eq!(scheme(false), "http");
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

    /// Com TLS, cada peer precisa do seu, porque o verifier é fixado na
    /// construção do cliente e carrega o node_id esperado.
    #[test]
    fn com_tls_cada_peer_tem_o_seu_e_e_reaproveitado() {
        let c = PeerClients::new(Duration::from_secs(600));
        c.set_tls(true);
        c.for_peer("aaaa").unwrap();
        c.for_peer("aaaa").unwrap();
        c.for_peer("bbbb").unwrap();
        assert_eq!(c.cached_peers(), vec!["aaaa".to_string(), "bbbb".to_string()]);
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
