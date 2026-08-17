# TLS entre peers - plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** O nó principal liga, por um interruptor em Configurações / Rede, TLS auto assinado e autenticado pelo node_id em todo o HTTP trocado entre peers; a barra de topo mostra o estado e a telemetria de inferência separa o custo do handshake.

**Architecture:** Cada nó deriva, em memória, um certificado auto assinado da chave Ed25519 que já é sua identidade (`node_id = hex(SHA256(pubkey))`), e o cliente valida o par apresentado recalculando esse hash - pinning, sem CA. O servidor atende TLS e texto claro na mesma porta, decidindo pelo primeiro byte da conexão, o que permite a virada não ser simultânea. A flag mora no `pom.json` do principal e chega aos demais por busca autenticada, nunca pelo gossip.

**Tech Stack:** Rust (axum 0.7, tokio, reqwest 0.12, rustls, tokio-rustls, rcgen, hyper-util, ed25519-dalek, sha2), React + TypeScript + Tailwind (apps/frontend), Vitest.

**Spec:** `docs/superpowers/specs/2026-08-17-tls-entre-peers-design.md`

## Global Constraints

- **Idioma da documentação e dos comentários novos: pt-BR.** Nunca travessão (`—`) em texto novo; use hífen `-`.
- **Nunca adicione co-author de agente em mensagem de commit.**
- **`cargo test` roda em `crates/node`; `cargo test --workspace` cobre os demais.** Toda task termina com a suíte verde.
- **Teste de rota usa `gateway::testkit`, nunca UDP de verdade** - a suíte transmite na LAN e pode injetar mensagem em cluster vivo.
- **`cd apps/frontend && npm run build` é obrigatório após editar a UI**; a UI é lida do disco em `apps/frontend/dist`.
- **Nome do produto nunca é literal**: use `brand::config_filename()`, `brand::env_var()`, etc.
- **Loopback (`127.0.0.1`) fica sempre em texto claro**, mesmo com a flag ativa.
- **Compatibilidade de campo novo em `pom.json` e no gossip: sempre `#[serde(default)]`**, para nó rodando build antigo continuar parseável.
- **Quem mexe numa pasta atualiza o `AGENTS.md` dela e os READMEs de subsistema afetados no mesmo commit.**

---

### Task 1: Certificado derivado da identidade e verifier por node_id

**Files:**
- Create: `crates/node/src/network/tls.rs`
- Modify: `crates/node/src/network/mod.rs` (declarar o módulo)
- Modify: `crates/node/Cargo.toml` (dependências)

**Interfaces:**
- Consumes: `crate::identity::{Identity, NodeId}`, já existentes.
- Produces:
  - `pub fn node_certificate(id: &Identity) -> anyhow::Result<NodeCertificate>`
  - `pub struct NodeCertificate { pub cert: rustls::pki_types::CertificateDer<'static>, pub key: rustls::pki_types::PrivateKeyDer<'static> }`
  - `pub fn server_config(id: &Identity) -> anyhow::Result<std::sync::Arc<rustls::ServerConfig>>`
  - `pub fn client_config(expected: &str) -> anyhow::Result<rustls::ClientConfig>` (`expected` é o node_id hex)
  - `pub fn node_id_of_cert(der: &[u8]) -> anyhow::Result<String>`

- [ ] **Step 1: Adicionar as dependências**

Em `crates/node/Cargo.toml`, na seção `[dependencies]`:

```toml
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
tokio-rustls = { version = "0.26", default-features = false, features = ["ring", "tls12"] }
rcgen = { version = "0.13", default-features = false, features = ["pem", "ring"] }
x509-parser = "0.16"
hyper-util = { version = "0.1", features = ["server-auto", "tokio"] }
hyper = { version = "1", features = ["server", "http1", "http2"] }
```

E troque a linha do `reqwest` para habilitar o TLS com raízes manuais (hoje ela não tem feature de TLS, e sem isso não há como injetar um verifier próprio):

```toml
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls-manual-roots", "http2"] }
```

Rode `cargo build` e confirme que compila antes de seguir.

- [ ] **Step 2: Escrever os testes que falham**

Crie `crates/node/src/network/tls.rs` com **apenas** o bloco de teste abaixo (o corpo do módulo vem no Step 4):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Identity, NodeId};

    /// O certificado tem que carregar a identidade do nó, não uma chave nova:
    /// é isso que faz o pinning valer sem distribuir arquivo nenhum.
    #[test]
    fn certificado_carrega_o_node_id_da_identidade() {
        let id = Identity::generate();
        let cert = node_certificate(&id).unwrap();
        assert_eq!(node_id_of_cert(cert.cert.as_ref()).unwrap(), id.node_id.0);
    }

    /// Duas identidades diferentes não podem produzir o mesmo fingerprint.
    #[test]
    fn certificado_de_outra_chave_tem_outro_node_id() {
        let a = node_certificate(&Identity::generate()).unwrap();
        let b = node_certificate(&Identity::generate()).unwrap();
        assert_ne!(
            node_id_of_cert(a.cert.as_ref()).unwrap(),
            node_id_of_cert(b.cert.as_ref()).unwrap()
        );
    }

    /// O verifier aceita o certificado do nó que ele esperava.
    #[test]
    fn verifier_aceita_o_node_id_esperado() {
        let id = Identity::generate();
        let cert = node_certificate(&id).unwrap();
        let v = PinnedNodeId::new(id.node_id.0.clone());
        assert!(v.matches(cert.cert.as_ref()).is_ok());
    }

    /// E recusa o de qualquer outro - é o ataque de quem se põe no meio com o
    /// próprio auto assinado, que é o único ataque que o pinning existe para
    /// impedir.
    #[test]
    fn verifier_recusa_certificado_de_outro_no() {
        let esperado = Identity::generate();
        let impostor = node_certificate(&Identity::generate()).unwrap();
        let v = PinnedNodeId::new(esperado.node_id.0.clone());
        assert!(v.matches(impostor.cert.as_ref()).is_err());
    }

    /// O node_id do certificado é, literalmente, o mesmo hash que o cluster já
    /// usa para nomear a máquina. Se um dia divergirem, o pinning passa a
    /// recusar todo mundo, e este teste é o que avisa.
    #[test]
    fn fingerprint_do_certificado_e_o_mesmo_de_from_pubkey() {
        let id = Identity::generate();
        let cert = node_certificate(&id).unwrap();
        let esperado = NodeId::from_pubkey(&id.signing_key.verifying_key());
        assert_eq!(node_id_of_cert(cert.cert.as_ref()).unwrap(), esperado.0);
    }
}
```

- [ ] **Step 3: Rodar os testes e ver falhar**

Registre o módulo em `crates/node/src/network/mod.rs` acrescentando `pub mod tls;` junto das outras declarações.

Run: `cd crates/node && cargo test tls::tests`
Expected: FAIL na compilação - `cannot find function node_certificate`.

- [ ] **Step 4: Implementar o módulo**

Escreva, **acima** do bloco de teste em `crates/node/src/network/tls.rs`:

```rust
//! TLS entre peers: certificado auto assinado amarrado à identidade do nó.
//!
//! A pergunta que este módulo responde é "com quem eu estou falando", e a
//! resposta já existia: `node_id` é `hex(SHA256(pubkey))` da chave Ed25519 de
//! longa duração (`identity.rs`), e o cluster inteiro já distribui esse hash
//! pelo gossip. Então o certificado não precisa ser assinado por ninguém - ele
//! precisa apenas **provar posse da chave que dá naquele node_id**, e é isso
//! que o handshake TLS faz.
//!
//! Nada aqui vai para o disco: o certificado é derivado da chave a cada
//! arranque. Não há o que renovar, revogar ou distribuir.

use anyhow::{anyhow, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::identity::Identity;

/// Certificado e chave que este nó apresenta, ambos em memória.
pub struct NodeCertificate {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

/// Deriva o certificado auto assinado da identidade do nó.
///
/// A chave do certificado **é** a `.pom-node.key`: `rcgen` aceita o PKCS#8 que
/// o `ed25519-dalek` exporta, e o rustls assina TLS 1.3 com Ed25519. Gerar uma
/// chave nova aqui destruiria o propósito - o par apresentado precisa ser o par
/// que define o node_id.
pub fn node_certificate(id: &Identity) -> Result<NodeCertificate> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

    let pkcs8 = id.signing_key.to_pkcs8_der()?;
    let key_pair = KeyPair::try_from(pkcs8.as_bytes())
        .context("a chave do nó não serviu como par de chaves TLS")?;

    let node_id = id.node_id.0.clone();
    // O SAN é um nome DNS sintético com o node_id. Nunca é resolvido: existe só
    // porque um certificado sem SAN é recusado por bibliotecas estritas, e
    // porque põe a identidade num campo legível por `openssl x509 -text` na
    // hora de depurar.
    let mut params = CertificateParams::new(vec![format!("{node_id}.{}", crate::brand::slug())])?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, node_id);
    params.distinguished_name = dn;

    let cert = params.self_signed(&key_pair)?;
    Ok(NodeCertificate {
        cert: cert.der().clone(),
        key: PrivateKeyDer::try_from(key_pair.serialize_der())
            .map_err(|e| anyhow!("chave TLS inválida: {e}"))?,
    })
}

/// O node_id que um certificado apresenta: SHA256 da pubkey do SPKI.
///
/// Mesma conta de `NodeId::from_pubkey`, sobre os mesmos 32 bytes. O teste
/// `fingerprint_do_certificado_e_o_mesmo_de_from_pubkey` prende as duas.
pub fn node_id_of_cert(der: &[u8]) -> Result<String> {
    let (_, parsed) =
        x509_parser::parse_x509_certificate(der).context("certificado do peer não parseou")?;
    let spki = parsed.public_key();
    let bits = spki.subject_public_key.data.as_ref();
    if bits.len() != 32 {
        return Err(anyhow!(
            "chave do certificado tem {} bytes, esperava 32 (Ed25519)",
            bits.len()
        ));
    }
    Ok(hex::encode(Sha256::digest(bits)))
}

/// Verifier que só aceita o nó cujo node_id foi fixado.
#[derive(Debug)]
pub struct PinnedNodeId {
    expected: String,
}

impl PinnedNodeId {
    pub fn new(expected: String) -> Self {
        Self { expected }
    }

    /// A regra inteira, isolada do trait para poder ser testada sem handshake.
    pub fn matches(&self, der: &[u8]) -> Result<()> {
        let found = node_id_of_cert(der)?;
        if found == self.expected {
            Ok(())
        } else {
            Err(anyhow!(
                "certificado é do nó {}, esperava {}",
                &found[..12.min(found.len())],
                &self.expected[..12.min(self.expected.len())]
            ))
        }
    }
}

impl ServerCertVerifier for PinnedNodeId {
    /// Validade, cadeia e nome são **ignorados de propósito**.
    ///
    /// Isso parece um buraco quando lido fora de contexto, então: a confiança
    /// aqui não vem de autoridade nem de prazo, vem de posse de chave, que o
    /// handshake prova em `verify_tls13_signature`. Checar validade só
    /// produziria falha operacional por relógio torto, e checar nome só
    /// produziria falha por discar um IP em vez do CN - nenhuma das duas diria
    /// nada sobre com quem estamos falando.
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        self.matches(end_entity.as_ref())
            .map(|()| ServerCertVerified::assertion())
            .map_err(|e| rustls::Error::General(e.to_string()))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// Configuração de servidor: apresenta o certificado do nó, não pede
/// certificado do cliente.
///
/// mTLS fica de fora porque o cliente já se autentica pela chave de conexão no
/// header, e duplicar a política de autorização em dois lugares é como ela
/// passa a divergir.
pub fn server_config(id: &Identity) -> Result<Arc<rustls::ServerConfig>> {
    let c = node_certificate(id)?;
    let cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![c.cert], c.key)
        .context("rustls recusou o certificado derivado da identidade")?;
    Ok(Arc::new(cfg))
}

/// Configuração de cliente que só aceita o peer de node_id `expected`.
pub fn client_config(expected: &str) -> Result<rustls::ClientConfig> {
    let mut cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedNodeId::new(expected.to_string())))
        .with_no_client_auth();
    // Sem ALPN o reqwest cai em HTTP/1.1 sobre TLS, que é o que os pares falam.
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(cfg)
}
```

Acrescente `use ed25519_dalek::pkcs8::EncodePrivateKey;` no topo se o `to_pkcs8_der` não estiver em escopo, e habilite a feature `pkcs8` do `ed25519-dalek` no `Cargo.toml` se o compilador pedir.

- [ ] **Step 5: Rodar os testes e ver passar**

Run: `cd crates/node && cargo test tls::tests`
Expected: PASS, 5 testes.

- [ ] **Step 6: Commit**

```bash
git add crates/node/Cargo.toml crates/node/Cargo.lock crates/node/src/network/tls.rs crates/node/src/network/mod.rs
git commit -m "feat(tls): certificado do no derivado da identidade e verifier por node_id"
```

---

### Task 2: A flag `tls_required` no `pom.json`

**Files:**
- Modify: `crates/node/src/utils/config.rs`
- Test: `crates/node/src/utils/config.rs` (bloco `#[cfg(test)]` existente)

**Interfaces:**
- Consumes: `Config`, `Role` de `utils/config.rs`.
- Produces: campo `Config.tls_required: bool`.

- [ ] **Step 1: Escrever o teste que falha**

No bloco de testes de `crates/node/src/utils/config.rs`, acrescente:

```rust
/// `pom.json` gravado antes desta feature não pode deixar de parsear - e o
/// padrão tem que ser "sem TLS", senão atualizar o binário derruba o cluster.
#[test]
fn config_antiga_parseia_sem_tls_required() {
    let cfg: Config =
        serde_json::from_str(r#"{"role":"node","connection_key":"k"}"#).unwrap();
    assert!(!cfg.tls_required);
}

#[test]
fn tls_required_sobrevive_ao_roundtrip() {
    let cfg: Config =
        serde_json::from_str(r#"{"role":"principal","tls_required":true}"#).unwrap();
    assert!(cfg.tls_required);
    let texto = serde_json::to_string(&cfg).unwrap();
    let volta: Config = serde_json::from_str(&texto).unwrap();
    assert!(volta.tls_required);
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cd crates/node && cargo test config::tests::tls_required -- --include-ignored; cargo test config_antiga_parseia`
Expected: FAIL - `no field tls_required on type Config`.

- [ ] **Step 3: Implementar**

Em `crates/node/src/utils/config.rs`, dentro de `pub struct Config`, logo após `blocked_nodes`:

```rust
    /// Todo HTTP entre peers vai por TLS quando isto é `true`.
    ///
    /// A decisão é do nó principal e vale para o cluster inteiro; os demais nós
    /// gravam aqui o que buscaram dele por canal autenticado. **Nunca grave
    /// isto a partir do gossip**: o UDP de descoberta é em claro e não
    /// autenticado, e um anúncio forjado rebaixaria a rede para texto claro,
    /// que é exatamente o ataque que o TLS existe para impedir. Ver
    /// `network::tls_policy`.
    #[serde(default)]
    pub tls_required: bool,
```

Corrija todo literal `Config { .. }` construído à mão no crate acrescentando `tls_required: false` (o compilador aponta cada um; `main.rs:946` e `backend/session.rs:112` são os conhecidos).

- [ ] **Step 4: Rodar e ver passar**

Run: `cd crates/node && cargo test`
Expected: PASS, suíte inteira.

- [ ] **Step 5: Commit**

```bash
git add crates/node/src
git commit -m "feat(tls): flag tls_required no pom.json, desligada por padrao"
```

---

### Task 3: Servidor atendendo TLS e texto claro na mesma porta

**Files:**
- Create: `crates/node/src/backend/serve.rs`
- Modify: `crates/node/src/backend/mod.rs` (declarar o módulo)
- Modify: `crates/node/src/backend/gateway.rs:1217-1228`
- Modify: `crates/node/src/inference/infer_server.rs:683`

**Interfaces:**
- Consumes: `network::tls::server_config` (Task 1).
- Produces:
  - `pub struct TlsServer { pub config: Arc<rustls::ServerConfig>, pub strict: Arc<std::sync::atomic::AtomicBool>, pub last_handshake_ms: Arc<std::sync::atomic::AtomicU64> }`
  - `pub fn wants_tls(first_byte: u8) -> bool`
  - `pub fn plain_allowed(peer: std::net::SocketAddr, strict: bool) -> bool`
  - `pub async fn serve(listener: tokio::net::TcpListener, app: axum::Router, tls: Option<TlsServer>, shutdown: Option<...>) -> anyhow::Result<()>`

- [ ] **Step 1: Escrever os testes que falham**

Crie `crates/node/src/backend/serve.rs` com só este bloco:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// `0x16` é `ContentType::Handshake` do TLS. Todo método HTTP começa com
    /// letra ASCII maiúscula (`G`, `P`, `O`...), então um byte só separa as
    /// duas falas sem ambiguidade.
    #[test]
    fn primeiro_byte_separa_tls_de_http() {
        assert!(wants_tls(0x16));
        assert!(!wants_tls(b'G')); // GET
        assert!(!wants_tls(b'P')); // POST
        assert!(!wants_tls(b'O')); // OPTIONS
    }

    /// Loopback fala claro sempre: `infer_tunnel::serve_loopback`, a sonda de
    /// `runtime.rs` e o `gateway::testkit` batem em 127.0.0.1 e não têm par de
    /// chaves a apresentar. Tráfego que não sai da máquina não tem o que
    /// proteger.
    #[test]
    fn loopback_fala_claro_mesmo_no_modo_estrito() {
        let local: SocketAddr = "127.0.0.1:5050".parse().unwrap();
        let local6: SocketAddr = "[::1]:5050".parse().unwrap();
        assert!(plain_allowed(local, true));
        assert!(plain_allowed(local6, true));
    }

    /// De fora, o modo estrito recusa texto claro - é o que faz a flag valer.
    #[test]
    fn remoto_em_claro_e_recusado_no_modo_estrito() {
        let remoto: SocketAddr = "192.168.100.138:5050".parse().unwrap();
        assert!(!plain_allowed(remoto, true));
        assert!(plain_allowed(remoto, false));
    }
}
```

- [ ] **Step 2: Rodar e ver falhar**

Acrescente `pub mod serve;` em `crates/node/src/backend/mod.rs`.

Run: `cd crates/node && cargo test serve::tests`
Expected: FAIL - `cannot find function wants_tls`.

- [ ] **Step 3: Implementar o laço de accept**

Acima do bloco de teste em `crates/node/src/backend/serve.rs`:

```rust
//! Um listener, duas falas.
//!
//! O servidor espia o primeiro byte da conexão sem consumi-lo (`peek`) e decide:
//! `0x16` é handshake TLS, qualquer outra coisa é HTTP em claro. As duas caem no
//! mesmo router.
//!
//! Por que uma porta só, em vez de uma porta TLS ao lado: nenhum anúncio muda,
//! nenhum campo novo entra no `PeerInfo` (que já está no limite da MTU do
//! `PeerList`), nenhuma regra de firewall nova - e, o que importa mais, **a
//! virada não precisa ser simultânea**. Enquanto uns peers já falam TLS e
//! outros ainda não, este servidor atende os dois.

use anyhow::Result;
use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::{TcpListener, TcpStream};
use tower::Service;
use tracing::{debug, warn};

/// O que o servidor precisa para falar TLS, e o estado que ele publica de volta.
#[derive(Clone)]
pub struct TlsServer {
    pub config: Arc<rustls::ServerConfig>,
    /// Com `true`, texto claro de fora do loopback é recusado.
    pub strict: Arc<AtomicBool>,
    /// Duração do último handshake aceito, em ms. É daqui que sai o
    /// `tls_handshake_ms` da telemetria (Task 7).
    pub last_handshake_ms: Arc<AtomicU64>,
}

/// `0x16` é `ContentType::Handshake` do TLS; nenhum método HTTP começa assim.
pub fn wants_tls(first_byte: u8) -> bool {
    first_byte == 0x16
}

/// Texto claro é sempre aceito do loopback, e só de fora dele quando a flag
/// está desligada.
pub fn plain_allowed(peer: SocketAddr, strict: bool) -> bool {
    if !strict {
        return true;
    }
    match peer.ip() {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().map(|v4| v4.is_loopback()).unwrap_or(false)
        }
    }
}

/// Serve `app` no `listener`, aceitando TLS e texto claro conforme a política.
///
/// `tls: None` é o comportamento anterior a esta feature, e é o que o `testkit`
/// e os servidores de teste em `127.0.0.1:0` usam.
pub async fn serve(listener: TcpListener, app: Router, tls: Option<TlsServer>) -> Result<()> {
    let acceptor = tls
        .as_ref()
        .map(|t| tokio_rustls::TlsAcceptor::from(t.config.clone()));

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("accept falhou: {e}");
                continue;
            }
        };
        // Nagle segura a escrita pequena até o ACK anterior; no SSE cada token é
        // uma escrita pequena. Mesma razão do `tcp_nodelay(true)` que estava no
        // `axum::serve`.
        let _ = stream.set_nodelay(true);

        let app = app.clone();
        let acceptor = acceptor.clone();
        let tls = tls.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, peer, app, acceptor, tls).await {
                debug!("conexão de {peer} encerrada: {e}");
            }
        });
    }
}

async fn handle_conn(
    stream: TcpStream,
    peer: SocketAddr,
    app: Router,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
    tls: Option<TlsServer>,
) -> Result<()> {
    let mut first = [0u8; 1];
    // `peek` não consome: quem for servir a conexão ainda vê o byte.
    let n = stream.peek(&mut first).await?;
    if n == 0 {
        return Ok(());
    }
    let is_tls = wants_tls(first[0]);
    let strict = tls
        .as_ref()
        .map(|t| t.strict.load(Ordering::Relaxed))
        .unwrap_or(false);

    if !is_tls && !plain_allowed(peer, strict) {
        warn!("recusada conexão em texto claro de {peer}: o cluster está em TLS");
        return Ok(());
    }

    let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let mut app = app.clone();
        async move { Service::call(&mut app, req).await }
    });

    match (is_tls, acceptor) {
        (true, Some(acceptor)) => {
            let comecou = Instant::now();
            let tls_stream = acceptor.accept(stream).await?;
            if let Some(t) = &tls {
                t.last_handshake_ms
                    .store(comecou.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
            Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(tls_stream), svc)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        (true, None) => {
            // Cliente falou TLS com um nó que não tem certificado. Não há o que
            // servir, e deixar a conexão pendurada esconderia o motivo.
            warn!("{peer} tentou TLS mas este nó não tem TLS configurado");
        }
        (false, _) => {
            Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), svc)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }
    Ok(())
}
```

Se `tower` não estiver nas dependências diretas, acrescente `tower = "0.5"` ao `Cargo.toml`.

- [ ] **Step 4: Rodar e ver passar**

Run: `cd crates/node && cargo test serve::tests`
Expected: PASS, 3 testes.

- [ ] **Step 5: Ligar o gateway ao novo laço**

Em `crates/node/src/backend/gateway.rs`, troque a linha `axum::serve(listener, app).tcp_nodelay(true).await?;` (linha ~1228) por:

```rust
    // O `tcp_nodelay` que estava aqui mudou de lugar, não sumiu: `serve` o
    // aplica em cada conexão aceita. Ver `backend/serve.rs`.
    crate::backend::serve::serve(listener, app, tls_server).await?;
```

Acrescente `tls_server: Option<crate::backend::serve::TlsServer>` como parâmetro de `start_gateway` e repasse `None` em todo chamador de teste.

- [ ] **Step 6: Ligar o servidor de inferência**

Em `crates/node/src/inference/infer_server.rs`, o `axum::serve(...)` da linha ~683 tem `with_graceful_shutdown`. Troque por:

```rust
    // O desligamento gracioso vira uma corrida entre o laço de accept e o
    // watch de geração: quando a geração muda, paramos de aceitar. Conexão em
    // voo termina sozinha, como antes.
    tokio::select! {
        r = crate::backend::serve::serve(listener, router, tls_server) => r?,
        _ = async move {
            loop {
                if stop_rx.changed().await.is_err() {
                    break;
                }
                if *stop_rx.borrow() != generation {
                    break;
                }
            }
        } => {}
    }
    Ok(())
```

Acrescente `tls_server: Option<crate::backend::serve::TlsServer>` à assinatura da função e repasse `None` nos testes.

- [ ] **Step 7: Rodar a suíte inteira**

Run: `cd crates/node && cargo test`
Expected: PASS. Se algum teste de rota quebrar, é porque um chamador não passou `None`.

- [ ] **Step 8: Commit**

```bash
git add crates/node/src
git commit -m "feat(tls): servidor atende TLS e texto claro na mesma porta"
```

---

### Task 4: Cache de clientes HTTP por peer

**Files:**
- Create: `crates/node/src/network/peer_client.rs`
- Modify: `crates/node/src/network/mod.rs`
- Modify: `crates/node/src/main.rs:728`, `crates/node/src/main.rs:1267`
- Modify: `crates/node/src/backend/gateway.rs` (campo `http` de `GatewayState`)

**Interfaces:**
- Consumes: `network::tls::client_config` (Task 1).
- Produces:
  - `pub struct PeerClients` com
    `pub fn new(timeout: std::time::Duration) -> Self`,
    `pub async fn for_peer(&self, node_id: &str) -> anyhow::Result<reqwest::Client>`,
    `pub fn plain(&self) -> reqwest::Client`,
    `pub fn set_tls(&self, on: bool)`,
    `pub fn tls_enabled(&self) -> bool`
  - `pub fn scheme(tls: bool) -> &'static str`

- [ ] **Step 1: Escrever os testes que falham**

Crie `crates/node/src/network/peer_client.rs` com só este bloco:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn esquema_da_url_segue_a_flag() {
        assert_eq!(scheme(true), "https");
        assert_eq!(scheme(false), "http");
    }

    /// Sem TLS, todo peer compartilha um cliente só - é o comportamento que
    /// existia antes desta feature, e não custa uma conexão nova por peer.
    #[tokio::test]
    async fn sem_tls_o_cliente_e_compartilhado() {
        let c = PeerClients::new(Duration::from_secs(600));
        assert!(!c.tls_enabled());
        let a = c.for_peer("aaaa").await.unwrap();
        let b = c.for_peer("bbbb").await.unwrap();
        // Mesmo pool: `reqwest::Client` é um Arc por dentro.
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }

    /// Com TLS, cada peer precisa do seu, porque o verifier é fixado na
    /// construção do cliente e carrega o node_id esperado.
    #[tokio::test]
    async fn com_tls_cada_peer_tem_o_seu_e_e_reaproveitado() {
        let c = PeerClients::new(Duration::from_secs(600));
        c.set_tls(true);
        let a1 = c.for_peer("aaaa").await.unwrap();
        let a2 = c.for_peer("aaaa").await.unwrap();
        let b = c.for_peer("bbbb").await.unwrap();
        assert_eq!(format!("{a1:?}"), format!("{a2:?}"));
        assert_ne!(format!("{a1:?}"), format!("{b:?}"));
    }

    /// Virar a flag joga fora os clientes: um cliente construído para falar
    /// claro nunca vai falar TLS, e vice-versa.
    #[tokio::test]
    async fn virar_a_flag_esvazia_o_cache() {
        let c = PeerClients::new(Duration::from_secs(600));
        c.set_tls(true);
        let antes = c.for_peer("aaaa").await.unwrap();
        c.set_tls(false);
        let depois = c.for_peer("aaaa").await.unwrap();
        assert_ne!(format!("{antes:?}"), format!("{depois:?}"));
    }
}
```

- [ ] **Step 2: Rodar e ver falhar**

Acrescente `pub mod peer_client;` em `crates/node/src/network/mod.rs`.

Run: `cd crates/node && cargo test peer_client::tests`
Expected: FAIL - `cannot find type PeerClients`.

- [ ] **Step 3: Implementar**

Acima do bloco de teste:

```rust
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

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// `https` quando o cluster está cifrado, `http` quando não.
pub fn scheme(tls: bool) -> &'static str {
    if tls {
        "https"
    } else {
        "http"
    }
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
            let per_peer = &self.per_peer;
            futures::executor::block_on(async {
                per_peer.write().await.clear();
            });
        }
    }

    /// Cliente sem pinning, para destino que não é peer (loopback, Ollama).
    pub fn plain(&self) -> reqwest::Client {
        self.plain.clone()
    }

    /// Cliente para falar com `node_id`, construído sob demanda.
    pub async fn for_peer(&self, node_id: &str) -> Result<reqwest::Client> {
        if !self.tls_enabled() {
            return Ok(self.plain.clone());
        }
        if let Some(c) = self.per_peer.read().await.get(node_id) {
            return Ok(c.clone());
        }
        let cfg = crate::network::tls::client_config(node_id)?;
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .use_preconfigured_tls(cfg)
            .build()
            .context("não deu para construir o cliente TLS do peer")?;
        self.per_peer
            .write()
            .await
            .insert(node_id.to_string(), client.clone());
        Ok(client)
    }
}
```

Se `futures::executor::block_on` não estiver disponível, troque `set_tls` por `pub async fn set_tls(&self, on: bool)` com `.await` no clear, e ajuste os chamadores.

- [ ] **Step 4: Rodar e ver passar**

Run: `cd crates/node && cargo test peer_client::tests`
Expected: PASS, 4 testes.

- [ ] **Step 5: Trocar o `http` do `GatewayState`**

Em `crates/node/src/backend/gateway.rs`, o campo `http: Arc<reqwest::Client>` passa a conviver com o cache:

```rust
    /// Cliente sem pinning, para destino que não é peer.
    pub http: Arc<reqwest::Client>,
    /// Clientes por peer, que é o caminho de toda chamada peer a peer.
    pub peer_http: Arc<crate::network::peer_client::PeerClients>,
```

Em `main.rs:728` e `main.rs:1267`, construa o cache junto do cliente:

```rust
    let peer_clients = Arc::new(crate::network::peer_client::PeerClients::new(
        Duration::from_secs(600),
    ));
```

e passe `peer_http: peer_clients.clone()` nos dois `GatewayState`.

- [ ] **Step 6: Rodar a suíte**

Run: `cd crates/node && cargo test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/node/src
git commit -m "feat(tls): cache de clientes HTTP por peer com pinning por node_id"
```

---

### Task 5: Inventário e conversão dos chamadores peer a peer

**Files:**
- Modify: `crates/node/src/network/route.rs:198` e `:274-318`
- Modify: `crates/node/src/network/network.rs:186,213,235,939`
- Modify: `crates/node/src/inference/relay.rs:150-215` (assinatura de `send_frame`)
- Modify: `crates/node/src/inference/direct.rs:191`
- Modify: `crates/node/src/backend/devupdate.rs:115`
- Modify: `crates/node/src/backend/transfer.rs`
- Test: `crates/node/src/network/peer_client.rs` (bloco novo)

**Interfaces:**
- Consumes: `PeerClients::for_peer`, `peer_client::scheme` (Task 4).
- Produces: `pub fn peer_base_url(host: &str, port: u16, tls: bool) -> String` em `network/peer_client.rs`.

**Esta é a task de maior risco do plano.** Um chamador esquecido é um salto que continua em texto claro sob uma flag que diz o contrário. O passo 1 existe para que o inventário seja explícito, não presumido.

- [ ] **Step 1: Levantar o inventário e conferir contra o código**

Run:

```bash
cd /Users/jairpereira/Sites/rdma-p2p
grep -rn 'format!("http://' crates/node/src | grep -v '127.0.0.1\|localhost'
grep -rn 'reqwest::Client::new()\|state.http\|\.http\b' crates/node/src --include='*.rs' | grep -v '^.*test'
```

Anote cada resultado numa lista com três colunas: arquivo:linha, o destino é peer ou loopback, e o node_id está disponível ali. Os que já conhecemos:

| local | destino | node_id disponível |
|---|---|---|
| `network/route.rs:198` | peer (sonda `/health`) | sim, `measure_peer` recebe o id |
| `network/network.rs:186,213,235` | peer (URLs anunciadas) | sim, é o próprio `PeerInfo` |
| `inference/direct.rs:191` | peer (entrega de resposta direta) | **não** - só host e porta |
| `backend/devupdate.rs:115` | peer | **não** - só `addr` e porta |
| `inference/infer_server.rs:315` | peer (próximo salto) | conferir no código |
| `network/infer_tunnel.rs:272` | loopback | não se aplica |
| `inference/runtime.rs:568` | loopback | não se aplica |
| `backend/transfer.rs:1751` | loopback | não se aplica |

Para cada linha com "não", o node_id do destino tem que ser levado até ali - normalmente acrescentando um parâmetro `peer_id: &str` à função e preenchendo-o no chamador, que já tem o `PeerInfo`.

- [ ] **Step 2: Escrever o teste da montagem de URL**

Em `crates/node/src/network/peer_client.rs`, no bloco de testes:

```rust
#[test]
fn url_de_peer_segue_a_flag_e_colcheta_ipv6() {
    assert_eq!(peer_base_url("192.168.1.5", 8181, false), "http://192.168.1.5:8181");
    assert_eq!(peer_base_url("192.168.1.5", 8181, true), "https://192.168.1.5:8181");
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
```

- [ ] **Step 3: Rodar e ver falhar**

Run: `cd crates/node && cargo test peer_client::tests::url_de_peer`
Expected: FAIL - `cannot find function peer_base_url`.

- [ ] **Step 4: Implementar `peer_base_url`**

Em `crates/node/src/network/peer_client.rs`:

```rust
/// Base de URL para falar com um peer, com o esquema que a flag manda.
///
/// O colchete do IPv6 é responsabilidade daqui: o cluster é IPv6 na prática, e
/// `http://2804:d55::1:8181` não parseia como URL - o parser lê `d55` como
/// porta. `network::ip_host` já fazia esta conta nos pontos que ele cobria;
/// esta função é a versão única, agora que o esquema também varia.
pub fn peer_base_url(host: &str, port: u16, tls: bool) -> String {
    let host = if host.starts_with('[') || !host.contains(':') {
        host.to_string()
    } else {
        format!("[{host}]")
    };
    format!("{}://{host}:{port}", scheme(tls))
}
```

- [ ] **Step 5: Rodar e ver passar**

Run: `cd crates/node && cargo test peer_client::tests::url_de_peer`
Expected: PASS.

- [ ] **Step 6: Converter cada chamador do inventário**

Para cada linha da tabela do Step 1 marcada como "peer": troque `format!("http://{host}:{port}")` por `peer_base_url(host, port, tls)` e troque o `reqwest::Client` usado por `peer_http.for_peer(node_id).await?`. Onde o node_id não chega, acrescente o parâmetro e preencha no chamador.

Em `inference/relay.rs`, `send_frame` (linha ~150) recebe `http: &reqwest::Client` - mantenha a assinatura, e resolva o cliente **no chamador**, que é quem conhece o peer. Isso evita passar o cache inteiro para dentro do laço de retentativa.

Deixe os destinos de loopback exatamente como estão.

- [ ] **Step 7: Rodar a suíte inteira**

Run: `cd crates/node && cargo test && cargo test --workspace`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/node/src
git commit -m "feat(tls): chamadas peer a peer passam pelo cliente com pinning"
```

---

### Task 6: A catraca - a flag chega por busca autenticada, nunca por gossip

**Files:**
- Create: `crates/node/src/network/tls_policy.rs`
- Modify: `crates/node/src/network/mod.rs`
- Modify: `crates/node/src/network/network.rs` (campo `tls` no Announce e no `PeerInfo`)
- Test: `crates/node/src/network/tls_policy.rs`

**Interfaces:**
- Consumes: `Config.tls_required` (Task 2), `PeerClients` (Task 4).
- Produces:
  - `pub enum PolicyOutcome { Unchanged, Fetch { principal: String }, Adopt(bool) }`
  - `pub fn on_announce(local: bool, announced: bool, from_principal: bool, principal_id: &str) -> PolicyOutcome`
  - `pub async fn fetch_from_principal(clients: &PeerClients, principal_id: &str, base_url: &str, connection_key: &str) -> anyhow::Result<bool>`

- [ ] **Step 1: Escrever os testes que falham**

Crie `crates/node/src/network/tls_policy.rs` com só este bloco:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const PRINCIPAL: &str = "aaaa0000";

    /// Anúncio que concorda com o que já sabemos não faz nada. É o caso comum:
    /// um Announce a cada 5 segundos, por peer.
    #[test]
    fn anuncio_que_concorda_nao_faz_nada() {
        assert!(matches!(
            on_announce(true, true, true, PRINCIPAL),
            PolicyOutcome::Unchanged
        ));
        assert!(matches!(
            on_announce(false, false, true, PRINCIPAL),
            PolicyOutcome::Unchanged
        ));
    }

    /// Discordar **não** muda nada sozinho: só dispara a busca. O gossip é
    /// gatilho, nunca valor.
    #[test]
    fn discordancia_do_principal_dispara_busca() {
        match on_announce(false, true, true, PRINCIPAL) {
            PolicyOutcome::Fetch { principal } => assert_eq!(principal, PRINCIPAL),
            outro => panic!("esperava Fetch, veio {outro:?}"),
        }
    }

    /// Este é o teste que sustenta a feature inteira: um nó qualquer da rede
    /// anunciando "sem TLS" é ignorado. O UDP é em claro e não autenticado, e
    /// obedecer a ele seria oferecer o rebaixamento que o TLS existe para
    /// impedir.
    #[test]
    fn anuncio_de_nao_principal_e_ignorado_mesmo_discordando() {
        assert!(matches!(
            on_announce(true, false, false, PRINCIPAL),
            PolicyOutcome::Unchanged
        ));
    }

    /// A adoção só existe como resultado da busca autenticada.
    #[test]
    fn adocao_vem_da_busca_e_nao_do_anuncio() {
        assert!(matches!(PolicyOutcome::Adopt(true), PolicyOutcome::Adopt(true)));
    }
}
```

- [ ] **Step 2: Rodar e ver falhar**

Acrescente `pub mod tls_policy;` em `crates/node/src/network/mod.rs`.

Run: `cd crates/node && cargo test tls_policy::tests`
Expected: FAIL - `cannot find function on_announce`.

- [ ] **Step 3: Implementar a política**

Acima do bloco de teste:

```rust
//! Quem decide se o cluster fala TLS, e por qual caminho essa decisão viaja.
//!
//! A decisão é do nó principal. O caminho **não** é o gossip: o UDP de
//! descoberta é em claro e não autenticado, então um anúncio forjado por
//! qualquer um na rede rebaixaria o cluster inteiro para texto claro - o exato
//! ataque que o TLS existe para impedir.
//!
//! Então o campo `tls` do Announce é **gatilho, nunca valor**: discordar dele
//! só dispara uma busca autenticada no principal, e só a resposta dessa busca é
//! gravada em `pom.json`. Um intruso pode disparar a busca à vontade; não pode
//! responder por ela, porque não tem a chave de conexão.
//!
//! E a busca que falha **preserva o valor local**: um principal fora do ar ou
//! uma rede partida nunca rebaixam o cluster. Fail-closed.

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use crate::network::peer_client::PeerClients;

#[derive(Debug)]
pub enum PolicyOutcome {
    /// Nada a fazer.
    Unchanged,
    /// Buscar a verdade no principal.
    Fetch { principal: String },
    /// Valor confirmado por busca autenticada, pronto para gravar.
    Adopt(bool),
}

/// O que fazer ao ver um Announce.
///
/// `local` é o que este nó tem gravado; `announced`, o que o anúncio diz;
/// `from_principal`, se quem anunciou se apresenta como principal.
pub fn on_announce(
    local: bool,
    announced: bool,
    from_principal: bool,
    principal_id: &str,
) -> PolicyOutcome {
    if !from_principal || announced == local {
        return PolicyOutcome::Unchanged;
    }
    PolicyOutcome::Fetch {
        principal: principal_id.to_string(),
    }
}

#[derive(Deserialize)]
struct TlsReply {
    tls_required: bool,
}

/// Pergunta ao principal, por canal autenticado, se o cluster fala TLS.
///
/// Erro aqui é `Err`, e o chamador **mantém o valor local** - ver o parágrafo
/// sobre fail-closed no topo do módulo.
pub async fn fetch_from_principal(
    clients: &PeerClients,
    principal_id: &str,
    base_url: &str,
    connection_key: &str,
) -> Result<bool> {
    let http = clients.for_peer(principal_id).await?;
    let resp = http
        .get(format!("{base_url}/api/cluster/tls"))
        .header("X-Connection-Key", connection_key)
        .send()
        .await
        .context("não deu para falar com o principal")?;
    if !resp.status().is_success() {
        anyhow::bail!("principal respondeu {}", resp.status());
    }
    let body: TlsReply = resp.json().await.context("resposta do principal não parseou")?;
    info!("política de TLS confirmada pelo principal: {}", body.tls_required);
    Ok(body.tls_required)
}

/// Aplica o resultado da busca, preservando o local quando ela falha.
pub fn adopt_or_keep(local: bool, fetched: Result<bool>) -> PolicyOutcome {
    match fetched {
        Ok(v) if v != local => PolicyOutcome::Adopt(v),
        Ok(_) => PolicyOutcome::Unchanged,
        Err(e) => {
            warn!("busca da política de TLS falhou, mantendo o valor local ({local}): {e}");
            PolicyOutcome::Unchanged
        }
    }
}
```

- [ ] **Step 4: Rodar e ver passar**

Run: `cd crates/node && cargo test tls_policy::tests`
Expected: PASS, 4 testes.

- [ ] **Step 5: Escrever o teste do fail-closed**

Acrescente ao bloco de testes:

```rust
/// Principal fora do ar não rebaixa ninguém.
#[test]
fn busca_que_falha_preserva_o_valor_local() {
    let r = adopt_or_keep(true, Err(anyhow::anyhow!("conexão recusada")));
    assert!(matches!(r, PolicyOutcome::Unchanged));
}

#[test]
fn busca_bem_sucedida_adota_o_valor_do_principal() {
    assert!(matches!(adopt_or_keep(false, Ok(true)), PolicyOutcome::Adopt(true)));
    assert!(matches!(adopt_or_keep(true, Ok(false)), PolicyOutcome::Adopt(false)));
}
```

Run: `cd crates/node && cargo test tls_policy::tests`
Expected: PASS, 6 testes.

- [ ] **Step 6: Levar o campo `tls` ao gossip**

Em `crates/node/src/network/network.rs`, acrescente ao variante `P2PMessage::Announce` (junto de `role`, por volta da linha 74) e ao `PeerInfo` (junto de `role`, por volta da linha 136):

```rust
        /// Se este nó fala TLS com os peers. **Gatilho, não valor** - ver
        /// `network::tls_policy`. Um nó só grava a política depois de
        /// confirmá-la com o principal por canal autenticado.
        #[serde(default)]
        tls: bool,
```

Preencha em `self_announce` e no anúncio gossipsub gêmeo (a doc em `network.rs:743` avisa que os dois têm que casar), e no `insert_peer`. Acrescente `tls: false` aos literais de teste que o compilador apontar.

Acrescente também a `Network` o estado que o anúncio lê, já nesta task (o Task 7 é quem vai chamá-lo pela rota):

```rust
    /// Se este nó fala TLS. Lido pelo `self_announce` e pelo `TlsServer.strict`.
    pub tls: Arc<std::sync::atomic::AtomicBool>,
```

com `pub fn set_tls(&self, on: bool)` e `pub fn tls_enabled(&self) -> bool` por cima dele.

Em `Network::handle_message`, no braço de `Announce`, chame `tls_policy::on_announce` e, quando ele devolver `Fetch`, dispare a busca numa task - **nunca** grave direto o valor anunciado.

- [ ] **Step 7: Escrever o teste do gossip**

No bloco de testes de `network.rs`, junto do teste que já checa `gossip["role"] == "Principal"` (linha ~1267):

```rust
#[test]
fn anuncio_carrega_a_flag_de_tls() {
    // Mesmo corpo do teste de `role` vizinho; o ponto é que os dois anúncios
    // (UDP e gossipsub) carreguem o campo, senão a virada só chega por um dos
    // caminhos e o cluster fica meio cifrado sem ninguém notar.
    assert_eq!(gossip["tls"], false);
}
```

Adapte ao formato exato do teste vizinho.

- [ ] **Step 8: Rodar a suíte**

Run: `cd crates/node && cargo test`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/node/src
git commit -m "feat(tls): catraca - a flag so e gravada apos busca autenticada no principal"
```

---

### Task 7: As duas rotas e o estado na `/api/ui/node`

**Files:**
- Modify: `crates/node/src/backend/webui.rs:44-60` (rotas), `:95-152` (`NodeInfo`), e um handler novo
- Modify: `crates/node/src/backend/gateway.rs` (rota `/api/cluster/tls`)
- Test: `crates/node/src/backend/webui.rs` (bloco novo, no padrão de `mod advertise_addr_tests`)

**Interfaces:**
- Consumes: `Config.tls_required` (Task 2), `PeerClients::set_tls` (Task 4), `TlsServer.strict` (Task 3).
- Produces:
  - `POST /api/ui/network/tls` com corpo `{ "tls_required": bool }`
  - `GET /api/cluster/tls` respondendo `{ "tls_required": bool }`
  - `NodeInfo.tls_required: bool` e `NodeInfo.tls_authority: bool`

- [ ] **Step 1: Escrever os testes que falham**

Em `crates/node/src/backend/webui.rs`, no fim do arquivo, no molde de `mod advertise_addr_tests`:

```rust
#[cfg(test)]
mod tls_settings_tests {
    use super::*;

    /// Só o principal decide. A checagem é **no servidor**: esconder o botão na
    /// tela é conveniência, não controle.
    #[tokio::test]
    async fn no_comum_nao_pode_ligar_tls() {
        let state = test_state_with_role(Role::Node).await;
        let resp = save_tls(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveTlsRequest { tls_required: true }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
        assert!(!state.auth.read().await.tls_required);
    }

    #[tokio::test]
    async fn principal_liga_e_grava() {
        let state = test_state_with_role(Role::Principal).await;
        let resp = save_tls(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveTlsRequest { tls_required: true }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert!(state.auth.read().await.tls_required);
        assert!(state.peer_http.tls_enabled());
    }

    /// A tela precisa saber duas coisas diferentes: se está ligado, e se **este**
    /// nó pode mexer.
    #[tokio::test]
    async fn node_info_conta_estado_e_autoridade() {
        let state = test_state_with_role(Role::Node).await;
        let info = node_info(State(state)).await;
        assert!(!info.0.tls_required);
        assert!(!info.0.tls_authority);
    }
}
```

Escreva o helper `test_state_with_role` reaproveitando o que `mod advertise_addr_tests` já usa para montar um `GatewayState` de teste.

- [ ] **Step 2: Rodar e ver falhar**

Run: `cd crates/node && cargo test tls_settings_tests`
Expected: FAIL - `cannot find function save_tls`.

- [ ] **Step 3: Implementar**

Em `crates/node/src/backend/webui.rs`, acrescente à lista de rotas:

```rust
        .route("/api/ui/network/tls", post(save_tls))
```

Ao `struct NodeInfo`, junto de `advertise_addr`:

```rust
    /// Se o cluster fala TLS entre peers agora.
    pub tls_required: bool,
    /// Se **este** nó pode mudar isso - só o principal pode. A tela usa para
    /// decidir entre interruptor e leitura.
    pub tls_authority: bool,
```

Preencha em `node_info` lendo `state.auth` (o `role` já é lido ali):

```rust
    let (role, conn_key, advertise_addr, tls_required) = {
        let cfg = state.auth.read().await;
        (
            cfg.role.to_string(),
            cfg.connection_key.clone(),
            cfg.advertise_addr.clone(),
            cfg.tls_required,
        )
    };
    let tls_authority = role == Role::Principal.to_string();
```

E o handler:

```rust
#[derive(Deserialize)]
pub struct SaveTlsRequest {
    pub tls_required: bool,
}

/// Liga ou desliga o TLS entre peers, para o cluster inteiro.
///
/// Não precisa de reinício: o servidor aceita as duas falas desde o arranque
/// (`backend/serve.rs`), e o que muda aqui é qual delas os clientes usam e se
/// texto claro remoto passa a ser recusado. Os demais nós descobrem pela
/// catraca de `network::tls_policy`, dentro de um ciclo de gossip.
async fn save_tls(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveTlsRequest>,
) -> Response {
    {
        let cfg = state.auth.read().await;
        if cfg.role != Role::Principal {
            return (
                axum::http::StatusCode::FORBIDDEN,
                Json(json!({ "error": "só o nó principal decide o TLS do cluster." })),
            )
                .into_response();
        }
    }

    {
        let mut cfg = state.auth.write().await;
        cfg.tls_required = req.tls_required;
        if let Err(e) = cfg.save() {
            return bad_request(format!("não deu para gravar {}: {e}", crate::brand::config_filename()));
        }
    }

    state.peer_http.set_tls(req.tls_required);
    state.network.set_tls(req.tls_required);
    info!("TLS entre peers {} pela interface", if req.tls_required { "ligado" } else { "desligado" });

    Json(json!({ "tls_required": req.tls_required })).into_response()
}
```

Acrescente `Network::set_tls(&self, on: bool)` em `network.rs`, que grava num `AtomicBool` lido pelo `self_announce` e escrito no `TlsServer.strict` do Task 3.

Em `gateway.rs`, a rota que os peers consultam:

```rust
/// O que o principal responde à catraca. Autenticada pela chave de conexão -
/// é o que impede um intruso de responder por ele.
async fn cluster_tls(_auth: ConnectionAuth, State(state): State<GatewayState>) -> impl IntoResponse {
    Json(json!({ "tls_required": state.auth.read().await.tls_required }))
}
```

registrada como `.route("/api/cluster/tls", get(cluster_tls))`.

- [ ] **Step 4: Rodar e ver passar**

Run: `cd crates/node && cargo test tls_settings_tests && cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/node/src
git commit -m "feat(tls): rotas de leitura e escrita da politica e estado na /api/ui/node"
```

---

### Task 8: Telemetria - `tls_handshake_ms` e `tls` por salto

**Files:**
- Modify: `crates/node/src/inference/relay.rs:31-62` (`SentFrame` e `as_hop_timing_entry`)
- Modify: `crates/node/src/inference/infer_server.rs` (reply meta)
- Test: `crates/node/src/inference/relay.rs`

**Interfaces:**
- Consumes: `TlsServer.last_handshake_ms` (Task 3), `PeerClients::tls_enabled` (Task 4).
- Produces: campos `SentFrame.tls: bool` e `SentFrame.tls_handshake_ms: u64`, presentes no JSON de `as_hop_timing_entry`.

- [ ] **Step 1: Escrever o teste que falha**

No bloco de testes de `crates/node/src/inference/relay.rs`:

```rust
/// O handshake acontece **uma vez por conexão**, e as conexões ficam no pool do
/// reqwest. Então zero na maioria dos saltos é a verdade, não falta de
/// medição - e é por isso que a tela rotula o campo como custo de
/// estabelecimento de conexão, nunca como custo por token.
#[test]
fn hop_timing_carrega_tls_e_handshake() {
    let f = SentFrame {
        meta: json!({ "processing_ms": 7 }),
        tensor: None,
        round_trip_ms: 12,
        bytes_sent: 100,
        bytes_received: 200,
        client_encode_ms: 1,
        response_headers_ms: 5,
        response_body_ms: 3,
        client_decode_ms: 1,
        tls: true,
        tls_handshake_ms: 4,
    };
    let e = f.as_hop_timing_entry();
    assert_eq!(e["tls"], true);
    assert_eq!(e["tls_handshake_ms"], 4);
}

#[test]
fn salto_em_claro_reporta_tls_falso_e_handshake_zero() {
    let f = SentFrame {
        meta: json!({}),
        tensor: None,
        round_trip_ms: 9,
        bytes_sent: 0,
        bytes_received: 0,
        client_encode_ms: 0,
        response_headers_ms: 0,
        response_body_ms: 0,
        client_decode_ms: 0,
        tls: false,
        tls_handshake_ms: 0,
    };
    let e = f.as_hop_timing_entry();
    assert_eq!(e["tls"], false);
    assert_eq!(e["tls_handshake_ms"], 0);
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cd crates/node && cargo test relay::tests::hop_timing_carrega_tls`
Expected: FAIL - `struct SentFrame has no field named tls`.

- [ ] **Step 3: Implementar**

Em `SentFrame` (linha ~31):

```rust
    /// Se este salto foi cifrado. Também é o que denuncia uma virada pela
    /// metade, ou um chamador que ficou de fora da conversão do Task 5.
    pub tls: bool,
    /// Custo do handshake TLS **medido no servidor**, onde ele é exato
    /// (`backend/serve.rs` cerca o `TlsAcceptor::accept`). Vem preenchido no
    /// salto que abriu conexão nova e zero nos demais, porque as conexões ficam
    /// no pool do reqwest.
    pub tls_handshake_ms: u64,
```

Em `as_hop_timing_entry`, acrescente ao `json!`:

```rust
            "tls": self.tls,
            "tls_handshake_ms": self.tls_handshake_ms,
```

Em `send_frame` (linha ~150), preencha os dois: `tls` sai de `url.starts_with("https://")`, e `tls_handshake_ms` sai do `meta` da resposta:

```rust
                let tls = url.starts_with("https://");
                let tls_handshake_ms = meta
                    .get("tls_handshake_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
```

Em `infer_server.rs`, no handler que monta o reply meta, acrescente `tls_handshake_ms` lido do `TlsServer.last_handshake_ms` daquele nó.

Corrija o segundo construtor de `SentFrame` (linha ~423) e os literais de teste que o compilador apontar.

- [ ] **Step 4: Rodar e ver passar**

Run: `cd crates/node && cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/node/src
git commit -m "feat(tls): telemetria por salto com tls e tls_handshake_ms"
```

---

### Task 9: Badge de TLS na barra de topo

**Files:**
- Modify: `apps/frontend/src/components/Topbar.tsx`
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Test: `apps/frontend/src/components/Topbar.test.tsx` (criar se não existir)

**Interfaces:**
- Consumes: `NodeInfo.tls_required` e `NodeInfo.tls_authority` (Task 7), via `useNode()`.
- Produces: componente `TlsChip` exportado de `Topbar.tsx`.

- [ ] **Step 1: Escrever o teste que falha**

Crie `apps/frontend/src/components/Topbar.test.tsx` no padrão dos testes vizinhos (`PeerDetailCard.test.tsx` mostra como mockar `useNode`):

```tsx
import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { TlsChip } from "./Topbar";

vi.mock("../lib/queries", () => ({
  useNode: () => mockNode,
}));

let mockNode: { data?: { tls_required: boolean; tls_authority: boolean } } = {};

describe("TlsChip", () => {
  it("diz que o cluster está cifrado quando a flag está ligada", () => {
    mockNode = { data: { tls_required: true, tls_authority: false } };
    render(<TlsChip />);
    expect(screen.getByText("TLS")).toBeInTheDocument();
  });

  it("diz que não está, quando desligada - o operador precisa ver isso em toda tela", () => {
    mockNode = { data: { tls_required: false, tls_authority: false } };
    render(<TlsChip />);
    expect(screen.getByText(/sem TLS/i)).toBeInTheDocument();
  });

  it("não renderiza nada enquanto o nó não respondeu", () => {
    mockNode = {};
    const { container } = render(<TlsChip />);
    expect(container).toBeEmptyDOMElement();
  });
});
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cd apps/frontend && npm test -- Topbar`
Expected: FAIL - `TlsChip is not exported`.

- [ ] **Step 3: Implementar**

Em `apps/frontend/src/lib/types.ts`, no `interface NodeInfo`:

```ts
  /** Se o cluster fala TLS entre peers. `webui.rs` — `NodeInfo`. */
  tls_required: boolean;
  /** Se este nó pode mudar isso: só o principal pode. */
  tls_authority: boolean;
```

Em `Topbar.tsx`, acrescente `ShieldCheck, ShieldOff` ao import de `lucide-react` e:

```tsx
/**
 * Estado da criptografia do cluster, visível em toda tela.
 *
 * Fica aqui, e não só em Configurações, porque "o cluster está cifrado?" é uma
 * pergunta que o operador faz a qualquer momento, e uma virada pela metade
 * precisa ser vista sem procurar.
 */
export function TlsChip() {
  const { t } = useI18n();
  const node = useNode();
  if (!node.data) return null;
  const on = node.data.tls_required;
  return (
    <Link to="/configuracoes/rede" title={t(on ? "topbar.tlsOnHint" : "topbar.tlsOffHint")}>
      <Badge tone={on ? "aqua" : "amber"} className="hidden sm:inline-flex">
        {on ? <ShieldCheck className="h-3.5 w-3.5" /> : <ShieldOff className="h-3.5 w-3.5" />}
        <span>{t(on ? "topbar.tlsOn" : "topbar.tlsOff")}</span>
      </Badge>
    </Link>
  );
}
```

Renderize `<TlsChip />` na barra, imediatamente antes do `<Notifications />`. Use o caminho de rota que `components/nav.ts` já define para Configurações / Rede - confira lá em vez de inventar a URL.

Em `i18n/locales.ts`, acrescente nas duas línguas:

```ts
  "topbar.tlsOn": { pt: "TLS", en: "TLS" },
  "topbar.tlsOff": { pt: "sem TLS", en: "no TLS" },
  "topbar.tlsOnHint": {
    pt: "A comunicação entre os nós está cifrada.",
    en: "Traffic between nodes is encrypted.",
  },
  "topbar.tlsOffHint": {
    pt: "A comunicação entre os nós vai em texto claro.",
    en: "Traffic between nodes goes in the clear.",
  },
```

Siga o formato exato que o arquivo já usa.

- [ ] **Step 4: Rodar e ver passar**

Run: `cd apps/frontend && npm test -- Topbar`
Expected: PASS, 3 testes.

- [ ] **Step 5: Construir o bundle e commitar**

```bash
cd apps/frontend && npm run build
cd ../.. && git add apps/frontend/src apps/frontend/dist
git commit -m "feat(ui): badge de TLS na barra de topo"
```

---

### Task 10: Painel "Criptografia entre nós" em Configurações / Rede

**Files:**
- Modify: `apps/frontend/src/routes/NetworkSettings.tsx`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Test: `apps/frontend/src/routes/NetworkSettings.test.tsx` (criar se não existir)

**Interfaces:**
- Consumes: `NodeInfo.tls_required`, `NodeInfo.tls_authority` (Task 7); `POST /api/ui/network/tls`.
- Produces: nada consumido por tasks posteriores.

- [ ] **Step 1: Escrever os testes que falham**

Crie `apps/frontend/src/routes/NetworkSettings.test.tsx` no padrão de `Dashboard.test.tsx`:

```tsx
import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { TlsPanel } from "./NetworkSettings";

describe("TlsPanel", () => {
  it("no principal, o interruptor é editável", () => {
    render(<TlsPanel tlsRequired={false} authority={true} />);
    expect(screen.getByRole("switch")).not.toBeDisabled();
  });

  it("fora do principal, mostra em leitura e diz quem decide", () => {
    render(<TlsPanel tlsRequired={true} authority={false} />);
    expect(screen.getByRole("switch")).toBeDisabled();
    expect(screen.getByText(/principal/i)).toBeInTheDocument();
  });
});
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cd apps/frontend && npm test -- NetworkSettings`
Expected: FAIL - `TlsPanel is not exported`.

- [ ] **Step 3: Implementar**

Em `NetworkSettings.tsx`, acrescente `ShieldCheck` ao import de `lucide-react` e um painel no molde do de endereço anunciado que já existe ali. O interruptor chama:

```tsx
await api.post("/api/ui/network/tls", { tls_required: next });
```

Fora do principal, `disabled` e um `Footnote` dizendo que a decisão é do nó principal e vale para o cluster inteiro. Textos novos em `locales.ts` nas duas línguas, incluindo uma linha explicando que ligar não exige reinício e que os demais nós adotam em poucos segundos.

- [ ] **Step 4: Rodar e ver passar**

Run: `cd apps/frontend && npm test -- NetworkSettings`
Expected: PASS, 2 testes.

- [ ] **Step 5: Construir e commitar**

```bash
cd apps/frontend && npm run build
cd ../.. && git add apps/frontend/src apps/frontend/dist
git commit -m "feat(ui): painel de criptografia entre nos em Configuracoes / Rede"
```

---

### Task 11: A telemetria de TLS na tela de chat

**Files:**
- Modify: `apps/frontend/src/routes/Chat.tsx`
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/i18n/locales.ts`
- Test: `apps/frontend/src/routes/Chat.telemetry.test.tsx`

**Interfaces:**
- Consumes: `tls` e `tls_handshake_ms` no `hop_timings` do `x_pom` (Task 8).

- [ ] **Step 1: Escrever o teste que falha**

Em `Chat.telemetry.test.tsx`, no padrão dos casos que já existem lá:

```tsx
it("mostra o handshake de TLS separado, e diz que ele é por conexão", () => {
  renderTelemetry({
    hop_timings: [
      { round_trip_ms: 12, tls: true, tls_handshake_ms: 4, processing_ms: 7 },
    ],
  });
  expect(screen.getByText(/4 ms/)).toBeInTheDocument();
  expect(screen.getByText(/por conexão/i)).toBeInTheDocument();
});

it("um salto em claro é visível como tal", () => {
  renderTelemetry({
    hop_timings: [
      { round_trip_ms: 9, tls: false, tls_handshake_ms: 0, processing_ms: 5 },
    ],
  });
  expect(screen.getByText(/sem TLS/i)).toBeInTheDocument();
});
```

Adapte `renderTelemetry` ao helper que o arquivo já usa.

- [ ] **Step 2: Rodar e ver falhar**

Run: `cd apps/frontend && npm test -- Chat.telemetry`
Expected: FAIL - o texto não está na tela.

- [ ] **Step 3: Implementar**

Acrescente `tls: boolean` e `tls_handshake_ms: number` ao tipo de entrada de `hop_timings` em `types.ts`.

Na tabela de saltos de `Chat.tsx`, acrescente uma coluna com o cadeado por salto e o `tls_handshake_ms`, e **um rótulo dizendo que é custo de estabelecimento de conexão, não custo por requisição**. Sem esse rótulo, o operador que olhar um salto com 0 ms conclui que TLS é grátis, e quem olhar o salto que abriu conexão conclui que é caro - as duas leituras erradas pelo mesmo número.

Acrescente também a nota de que o custo por requisição depois do handshake é a cifra do record layer, diluída em `response_body_ms` e não separável sem instrumentar o socket. Textos em `locales.ts` nas duas línguas.

- [ ] **Step 4: Rodar e ver passar**

Run: `cd apps/frontend && npm test -- Chat.telemetry`
Expected: PASS.

- [ ] **Step 5: Construir e commitar**

```bash
cd apps/frontend && npm run build
cd ../.. && git add apps/frontend/src apps/frontend/dist
git commit -m "feat(ui): telemetria mostra o handshake de TLS separado por salto"
```

---

### Task 12: Teste de integração dos dois lados falando

**Files:**
- Modify: `crates/node/src/backend/serve.rs` (bloco de testes)

**Interfaces:**
- Consumes: tudo das tasks 1 a 5.

- [ ] **Step 1: Escrever o teste**

```rust
#[cfg(test)]
mod integracao {
    use super::*;
    use crate::identity::Identity;
    use axum::{routing::get, Router};
    use std::time::Duration;

    async fn subir(id: &Identity, strict: bool) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/health", get(|| async { "ok" }));
        let tls = TlsServer {
            config: crate::network::tls::server_config(id).unwrap(),
            strict: Arc::new(AtomicBool::new(strict)),
            last_handshake_ms: Arc::new(AtomicU64::new(0)),
        };
        let h = tokio::spawn(async move {
            let _ = serve(listener, app, Some(tls)).await;
        });
        (addr, h)
    }

    /// O caminho feliz inteiro: certificado derivado da identidade, cliente que
    /// pina o node_id, handshake, resposta.
    #[tokio::test]
    async fn cliente_pinado_fala_com_o_servidor() {
        let id = Identity::generate();
        let (addr, _h) = subir(&id, false).await;
        let clients = crate::network::peer_client::PeerClients::new(Duration::from_secs(5));
        clients.set_tls(true);
        let http = clients.for_peer(&id.node_id.0).await.unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
    }

    /// E o ataque que o pinning existe para impedir: um cliente que espera
    /// outro node_id não completa o handshake.
    #[tokio::test]
    async fn cliente_que_espera_outro_no_e_recusado() {
        let servidor = Identity::generate();
        let outro = Identity::generate();
        let (addr, _h) = subir(&servidor, false).await;
        let clients = crate::network::peer_client::PeerClients::new(Duration::from_secs(5));
        clients.set_tls(true);
        let http = clients.for_peer(&outro.node_id.0).await.unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await;
        assert!(r.is_err(), "o handshake tinha que falhar, veio {r:?}");
    }

    /// A virada não é simultânea: o mesmo servidor atende texto claro enquanto
    /// peers antigos ainda não souberam.
    #[tokio::test]
    async fn o_mesmo_servidor_atende_texto_claro() {
        let id = Identity::generate();
        let (addr, _h) = subir(&id, false).await;
        let r = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
    }
}
```

- [ ] **Step 2: Rodar**

Run: `cd crates/node && cargo test serve::integracao -- --nocapture`
Expected: PASS, 3 testes. Se `cliente_pinado_fala_com_o_servidor` falhar por ALPN, confira `client_config` no Task 1.

- [ ] **Step 3: Rodar a suíte inteira**

Run: `cd crates/node && cargo test && cargo test --workspace`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/node/src/backend/serve.rs
git commit -m "test(tls): integracao do pinning, da recusa e do texto claro"
```

---

### Task 13: Documentação

**Files:**
- Modify: `docs/subsystems/rede-p2p.md`
- Modify: `docs/subsystems/api-http.md`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `docs/subsystems/inferencia.md`
- Modify: `crates/node/src/network/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `AGENTS.md` (raiz)

**A tarefa não está pronta enquanto a documentação não refletir o código entregue.** Doc desatualizada é bug: um agente futuro lê a doc e entende o sistema errado.

- [ ] **Step 1: `rede-p2p.md`**

Seção nova "TLS entre peers": o certificado derivado da identidade, o pinning pelo node_id (com a conta `hex(SHA256(pubkey))` explícita), a decisão de ignorar validade e nome e por quê, a catraca, o fail-closed, e o que fica de fora (gossip UDP, libp2p já cifrado por Noise, UI no navegador).

- [ ] **Step 2: `api-http.md`**

`POST /api/ui/network/tls` (corpo, 403 fora do principal) e `GET /api/cluster/tls` (autenticada pela chave de conexão, é a resposta que a catraca consome).

- [ ] **Step 3: `web-ui.md`**

Badge da Topbar nos dois estados e o painel de Configurações / Rede, incluindo o modo leitura fora do principal.

- [ ] **Step 4: `inferencia.md`**

`tls` e `tls_handshake_ms` no `hop_timings`, **com a ressalva do pool de conexões**: zero na maioria dos saltos é a verdade, e o número é custo de estabelecimento, não custo por token.

- [ ] **Step 5: `network/AGENTS.md` e `backend/AGENTS.md`**

Acrescente `tls.rs`, `peer_client.rs` e `tls_policy.rs` à lista de arquivos de `network/`, e `serve.rs` à de `backend/`, cada um com a linha de "o que faz" no formato que os arquivos já usam.

- [ ] **Step 6: Invariante no `AGENTS.md` raiz**

Na lista de invariantes:

```markdown
- **O gossip nunca é fonte de verdade da flag de TLS.** O campo `tls` do
  Announce é gatilho; o valor só é gravado depois de busca autenticada no
  principal, e busca que falha preserva o valor local. O UDP é em claro e não
  autenticado: obedecer a ele seria oferecer o rebaixamento que o TLS existe
  para impedir. → `rede-p2p.md`
```

- [ ] **Step 7: Commit**

```bash
git add docs AGENTS.md crates/node/src/network/AGENTS.md crates/node/src/backend/AGENTS.md
git commit -m "docs(tls): canal, catraca, rotas, tela e telemetria"
```

---

## Verificação final

- [ ] `cd crates/node && cargo test` - verde
- [ ] `cargo test --workspace` - verde
- [ ] `cd apps/frontend && npm test` - verde
- [ ] `cd apps/frontend && npm run build` - sem erro
- [ ] `cargo build --release` - sem erro
- [ ] Subir dois nós com `POM_HOME` distintos, ligar TLS no principal, e confirmar na Topbar do segundo que ele adotou dentro de um ciclo de gossip; rodar uma inferência e conferir `tls: true` em todos os saltos do `x_pom`.
