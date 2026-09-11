//! Os dois lados de ancorar a confiança de admin, vistos pela interface.
//!
//! **Onde isto vive e por quê.** A rota é `CommonOrInternal`, fora do escopo
//! `AdminOnly`, porque antes de ancorar um nó com `profile: admin` **não** tem
//! capacidade efetiva de admin: `require_admin_for_surface` recusa `AdminOnly`
//! exatamente nesse estado. Pôr a tela atrás do perfil de administrador a
//! tornaria inalcançável no único momento em que ela serve para alguma coisa.
//! Pelo mesmo motivo o nó que recebe a âncora pode ser `node` ou `infer`.
//!
//! **O que é seguro mostrar.** A impressão digital é pública: é o valor que o
//! operador leva consigo para conferir noutra máquina, e conhecê-la não
//! concede nada a ninguém. Chave privada nenhuma passa por aqui.
//!
//! **O que trava a instalação.** Três recusas, todas no servidor:
//!
//! 1. só pelo loopback, medido no endereço real do socket
//!    (`serve::PeerAddr`). Não é `admin_exposure`, que pode estar aberta à
//!    rede, e não é cabeçalho de proxy: `X-Forwarded-For` não prova nada e
//!    não entra nesta decisão;
//! 2. só enquanto o nó ainda não tem âncora - trocar a raiz é reancoragem
//!    manual na máquina (`pom admin-anchor --replace`), nunca um gesto de
//!    interface;
//! 3. só com a impressão digital digitada pelo operador batendo com a chave
//!    ancorada da cadeia colada. O arquivo não se auto-valida.
//!
//! A validação da cadeia e a conferência da impressão digital são as do
//! `admin_trust`, as mesmas que `pom admin-anchor` usa. Se o caminho do
//! comando e o da interface divergirem, um deles está errado.
//!
//! **Por que a chave de conexão não é exigida aqui.** As rotas vizinhas de
//! autoridade a exigem; estas não, de propósito. A fronteira continua sendo a
//! mesma de toda a superfície administrativa (`require_admin_for_surface` ->
//! `AdminAuth`: loopback em modo local, token humano em modo de rede). Somar
//! aquela credencial não protege contra a ameaça que a trava de loopback
//! existe para conter - quem alcança o navegador no loopback alcança o
//! `localStorage` da mesma origem, onde ela está - e custa exatamente o que
//! esta tela não pode perder: um navegador que ainda não a recebeu veria
//! "missing Authorization header" no lugar da impressão digital, e o operador
//! ficaria outra vez dependente do terminal.

use super::*;
use axum::Extension;

/// O estado de ancoragem desta máquina, como a interface o desenha.
#[derive(Debug, Serialize)]
pub struct TrustAnchorView {
    /// Se este nó já tem raiz de confiança válida.
    pub anchored: bool,
    /// Impressão digital SHA-256 da chave ancorada. `None` por ancorar.
    pub fingerprint: Option<String>,
    pub cluster_id: Option<String>,
    /// A máquina cuja chave é a raiz, para o operador reconhecê-la.
    pub anchor_node_id: Option<String>,
    /// Se a raiz é a chave **deste** nó, e não uma que ele recebeu.
    pub is_anchor: bool,
    /// Se esta requisição chegou pelo loopback. Endereço real do socket.
    pub loopback: bool,
    /// Se instalar a âncora por aqui é possível agora.
    pub can_install: bool,
    /// Arquivo que o operador copia do admin para cá.
    pub trust_filename: String,
}

#[derive(Debug, Deserialize)]
pub struct InstallAnchorRequest {
    /// Conteúdo colado do arquivo de confiança vindo do admin.
    pub document: String,
    /// Impressão digital que o operador trouxe pelo canal dele e digitou.
    /// Nunca é pré-preenchida a partir de `document`: é ela que faz a
    /// conferência valer alguma coisa.
    pub fingerprint: String,
}

/// Materials an administrator can hand to the operator of another node.
/// The trust document is public chain data; the connection key is returned
/// only for a network that still uses the private shared secret.
#[derive(Debug, Serialize)]
pub struct TrustShareView {
    pub filename: String,
    pub document: String,
    pub connection_key: Option<String>,
    pub visibility: crate::config::NetworkVisibility,
}

#[derive(Debug)]
pub(crate) enum AnchorValidationError {
    InvalidDocument(anyhow::Error),
    FingerprintMismatch,
}

/// Shared validation path for the configured gateway and the loopback-only
/// setup router. The HTTP handlers own their transport policy; this function
/// owns the document and fingerprint rules.
pub(crate) fn validate_transported_anchor(
    body: &InstallAnchorRequest,
) -> Result<(crate::admin_trust::AdminTrustDocument, String), AnchorValidationError> {
    let document = crate::admin_trust::parse_transported(&body.document)
        .map_err(AnchorValidationError::InvalidDocument)?;
    let expected = crate::admin_trust::anchor_fingerprint(&document);
    if !crate::admin_trust::fingerprint_matches(&body.fingerprint, &expected) {
        return Err(AnchorValidationError::FingerprintMismatch);
    }
    Ok((document, expected))
}

/// Shared durable adoption path for the configured gateway and setup router.
pub(crate) fn adopt_transported_anchor(
    trust: &crate::admin_trust::AdminTrust,
    body: &InstallAnchorRequest,
) -> anyhow::Result<String> {
    let (document, expected) = match validate_transported_anchor(body) {
        Ok(value) => value,
        Err(AnchorValidationError::InvalidDocument(error)) => return Err(error),
        Err(AnchorValidationError::FingerprintMismatch) => {
            anyhow::bail!("a impressão digital conferida não corresponde à âncora desta cadeia")
        }
    };
    trust.adopt_anchor(document, &body.fingerprint)?;
    Ok(expected)
}

/// Recusa com um código estável ao lado da frase, para a interface traduzir a
/// mensagem em vez de mostrar o texto do servidor cru.
fn refusal(status: StatusCode, reason: &'static str, message: &str) -> Response {
    (status, Json(json!({ "error": message, "reason": reason }))).into_response()
}

fn is_loopback(peer: Option<&crate::serve::PeerAddr>) -> bool {
    // Sem endereço de socket não há prova de origem: falha fechada.
    peer.is_some_and(|peer| crate::auth::is_loopback_addr(peer.0))
}

pub(super) async fn trust_anchor(
    peer: Option<Extension<crate::serve::PeerAddr>>,
    State(state): State<GatewayState>,
) -> impl IntoResponse {
    let loopback = is_loopback(peer.as_ref().map(|Extension(peer)| peer));
    let anchored = state.admin_trust.is_anchored();
    let document = state.admin_trust.document();
    let (fingerprint, cluster_id, anchor_node_id, is_anchor) = if anchored {
        (
            Some(crate::admin_trust::anchor_fingerprint(&document)),
            Some(document.anchor.cluster_id.clone()),
            Some(document.anchor.principal.node_id.clone()),
            document.anchor.principal == state.authority_principal,
        )
    } else {
        (None, None, None, false)
    };
    Json(TrustAnchorView {
        anchored,
        fingerprint,
        cluster_id,
        anchor_node_id,
        is_anchor,
        loopback,
        can_install: !anchored && !state.admin_trust.anchor_file_exists() && loopback,
        trust_filename: crate::config::admin_trust_filename(),
    })
}

pub(super) async fn install_trust_anchor(
    peer: Option<Extension<crate::serve::PeerAddr>>,
    State(state): State<GatewayState>,
    Json(body): Json<InstallAnchorRequest>,
) -> Response {
    if !is_loopback(peer.as_ref().map(|Extension(peer)| peer)) {
        // Quem alcança o navegador escolheria quem administra a rede. A
        // exposição administrativa configurada não serve de trava aqui: ela
        // pode estar aberta à rede de propósito.
        warn!("recusada instalação de âncora administrativa vinda de fora do loopback");
        return refusal(
            StatusCode::FORBIDDEN,
            "not_loopback",
            "a âncora administrativa só é instalada a partir desta máquina",
        );
    }
    if state.admin_trust.is_anchored() || state.admin_trust.anchor_file_exists() {
        return refusal(
            StatusCode::CONFLICT,
            "already_anchored",
            "este nó já tem raiz de confiança; trocar a raiz é reancoragem manual na máquina",
        );
    }
    let (document, expected) = match validate_transported_anchor(&body) {
        Ok(value) => value,
        Err(AnchorValidationError::InvalidDocument(error)) => {
            return refusal(
                StatusCode::BAD_REQUEST,
                "invalid_document",
                &format!("{error:#}"),
            );
        }
        Err(AnchorValidationError::FingerprintMismatch) => {
            return refusal(
                StatusCode::BAD_REQUEST,
                "fingerprint_mismatch",
                "a impressão digital conferida não corresponde à âncora desta cadeia",
            );
        }
    };
    // As recusas acima existem para dar a frase certa ao operador; a que não
    // pode ser contornada é esta, que refaz as três sob o mesmo cadeado antes
    // de escrever. Chegar aqui com erro é corrida ou disco, não navegador.
    match state.admin_trust.adopt_anchor(document, &body.fingerprint) {
        Ok(()) => {
            state
                .network
                .set_policy_principal_pubkey(state.admin_trust.current_primary().pubkey);
            info!(
                "âncora administrativa instalada pelo operador; impressão digital conferida {expected}"
            );
            Json(json!({ "anchored": true, "fingerprint": expected })).into_response()
        }
        Err(error) => refusal(
            StatusCode::CONFLICT,
            "install_failed",
            &format!("{error:#}"),
        ),
    }
}

pub(crate) async fn trust_share(State(state): State<GatewayState>) -> Response {
    let profile = state.auth.read().await.profile;
    if state
        .admin_trust
        .effective_authority(&state.authority_principal, profile)
        .is_none()
    {
        return refusal(
            StatusCode::FORBIDDEN,
            "admin_authority_required",
            "somente um administrador autorizado pode compartilhar a confiança",
        );
    }

    let (visibility, connection_key) = {
        let config = state.auth.read().await;
        (
            config.network.visibility,
            (config.network.visibility != crate::config::NetworkVisibility::Public)
                .then(|| config.connection_key.clone())
                .filter(|key| !key.is_empty()),
        )
    };
    let document = state.admin_trust.document();
    let document = match serde_json::to_string_pretty(&document) {
        Ok(document) => document,
        Err(error) => {
            return refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "trust_document_unavailable",
                &format!("não foi possível serializar a raiz de confiança: {error}"),
            )
        }
    };
    Json(TrustShareView {
        filename: crate::config::admin_trust_filename(),
        document,
        connection_key,
        visibility,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    /// O router real, com o middleware de perfil que o gateway aplica. Os
    /// testes passam por ele de propósito: a trava tem de valer no caminho
    /// HTTP inteiro, não só dentro do handler.
    fn app(state: GatewayState) -> Router {
        Router::new()
            .merge(crate::webui::routes())
            .with_state(state.clone())
            .layer(axum::middleware::from_fn_with_state(
                state,
                crate::gateway::require_admin_for_surface,
            ))
    }

    /// Uma cadeia assinada como a que o operador copia do admin, com a
    /// impressão digital que ele leva consigo.
    fn cadeia_do_admin() -> (String, String) {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let principal = crate::admin_trust::AuthorityPrincipal::from_signing_key(
            "ksia",
            "peer-ksia",
            &signing_key,
        );
        let trust = crate::admin_trust::AdminTrust::bootstrap_primary_in_memory(
            "cluster-do-operador",
            principal,
            &signing_key,
        )
        .expect("cadeia raiz de teste");
        let document = trust.document();
        let fingerprint = crate::admin_trust::anchor_fingerprint(&document);
        (
            serde_json::to_string(&document).expect("cadeia serializa"),
            fingerprint,
        )
    }

    async fn por_ancorar(node_id: &str) -> GatewayState {
        let mut state = state_with(node_id, vec![]).await;
        state.admin_trust = Arc::new(crate::admin_trust::AdminTrust::empty());
        state
    }

    #[tokio::test]
    async fn admin_can_download_trust_and_private_connection_secret() {
        let state = state_with("ksia", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;
        let mut request = HttpRequest::get("/api/ui/network/trust-share")
            .header("authorization", "Bearer chave")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
        let response = app(state).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_of(response).await;
        assert!(body["filename"].as_str().unwrap().ends_with("trust.json"));
        assert!(body["document"].as_str().unwrap().contains("anchor"));
        assert_eq!(body["connection_key"], "chave");
    }

    fn instalar(peer: &str, document: &str, fingerprint: &str) -> HttpRequest<Body> {
        let mut request = HttpRequest::post("/api/ui/network/trust-anchor")
            .header("authorization", "Bearer chave")
            .header("content-type", "application/json")
            // Um cabeçalho de proxy é escrito pelo cliente. Ele viaja em todos
            // estes testes justamente para provar que não pesa em nada.
            .header("x-forwarded-for", "127.0.0.1")
            .header("x-real-ip", "127.0.0.1")
            .body(Body::from(
                serde_json::json!({ "document": document, "fingerprint": fingerprint }).to_string(),
            ))
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr(peer.parse().unwrap()));
        request
    }

    /// A trava é o endereço real do socket, e não `admin_exposure`: aqui a
    /// superfície administrativa está deliberadamente aberta à rede, o
    /// requisitante traz o token humano correto - e a instalação continua
    /// recusada. Usar a exposição configurada como trava seria um buraco.
    #[tokio::test]
    async fn a_ancora_nao_e_instalada_de_fora_do_loopback() {
        let state = por_ancorar("salem").await;
        {
            let mut cfg = state.auth.write().await;
            cfg.admin_exposure = crate::config::AdminExposure::Network;
            cfg.admin_access_token = Some("token-do-operador".into());
        }
        let (document, fingerprint) = cadeia_do_admin();

        let mut request = instalar("192.0.2.7:4321", &document, &fingerprint);
        request.headers_mut().insert(
            crate::auth::ADMIN_ACCESS_TOKEN_HEADER,
            "token-do-operador".parse().unwrap(),
        );
        let response = app(state.clone()).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_of(response).await["reason"], "not_loopback");
        assert!(
            !state.admin_trust.is_anchored(),
            "uma recusa não pode deixar autoridade instalada"
        );
    }

    /// Trocar a raiz não é gesto de interface. Um nó já ancorado recusa e diz
    /// que a reancoragem é manual, na máquina.
    #[tokio::test]
    async fn um_no_ja_ancorado_recusa_a_instalacao() {
        // `state_with` já nasce ancorado na própria chave.
        let state = state_with("ksia", vec![]).await;
        let ancora_original = state.admin_trust.document().anchor;
        let (document, fingerprint) = cadeia_do_admin();

        let response = app(state.clone())
            .oneshot(instalar("127.0.0.1:4321", &document, &fingerprint))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(body_of(response).await["reason"], "already_anchored");
        assert_eq!(
            state.admin_trust.document().anchor,
            ancora_original,
            "a âncora vigente não pode ser substituída pela interface"
        );
    }

    /// O arquivo não se auto-valida: o que o valida é o valor que o operador
    /// trouxe pelo canal dele. Errar esse valor é recusa.
    #[tokio::test]
    async fn a_impressao_digital_errada_e_recusada() {
        let state = por_ancorar("salem").await;
        let (document, fingerprint) = cadeia_do_admin();
        let errada = format!("{}0", &fingerprint[1..]);
        assert_ne!(errada, fingerprint);

        let response = app(state.clone())
            .oneshot(instalar("127.0.0.1:4321", &document, &errada))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_of(response).await["reason"], "fingerprint_mismatch");
        assert!(
            !state.admin_trust.is_anchored(),
            "impressão digital errada não pode fabricar autoridade"
        );
    }

    /// O caminho feliz do lado que recebe: colar o arquivo e digitar a
    /// impressão digital certa ancora o nó.
    #[tokio::test]
    async fn a_impressao_digital_certa_instala_a_ancora() {
        let state = por_ancorar("salem").await;
        let (document, fingerprint) = cadeia_do_admin();

        // Espaços e caixa são transcrição humana, não outro valor.
        let digitada = format!("  {}  ", fingerprint.to_uppercase());
        let response = app(state.clone())
            .oneshot(instalar("127.0.0.1:4321", &document, &digitada))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await["fingerprint"], fingerprint);
        assert!(state.admin_trust.is_anchored());
        assert_eq!(
            state.admin_trust.document().anchor.cluster_id,
            "cluster-do-operador"
        );
    }

    /// A dobra desta tarefa: um `profile: admin` por ancorar não tem
    /// capacidade efetiva de admin. A tela da impressão digital tem de ficar
    /// fora do escopo `AdminOnly`, senão o operador nunca a alcança - e é
    /// exatamente ali que ele precisa dela.
    #[tokio::test]
    async fn um_admin_por_ancorar_alcanca_a_tela_da_impressao_digital() {
        let state = por_ancorar("ksia").await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;

        let mut recusada = HttpRequest::get("/api/ui/requests")
            .header("authorization", "Bearer chave")
            .body(Body::empty())
            .unwrap();
        recusada
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
        assert_eq!(
            app(state.clone()).oneshot(recusada).await.unwrap().status(),
            StatusCode::FORBIDDEN,
            "sem âncora, o perfil admin não abre a superfície administrativa"
        );

        let mut alcancada = HttpRequest::get("/api/ui/network/trust-anchor")
            .header("authorization", "Bearer chave")
            .body(Body::empty())
            .unwrap();
        alcancada
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
        let response = app(state).oneshot(alcancada).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let view = body_of(response).await;
        assert_eq!(view["anchored"], false);
        assert_eq!(view["can_install"], true);
        assert!(view["fingerprint"].is_null());
    }

    /// A outra metade da dobra: a tela também não pode depender da chave de
    /// conexão do cluster. Um navegador que ainda não a recebeu tem de ver a
    /// impressão digital, senão o operador volta ao terminal - que é
    /// exatamente o que esta tarefa existe para evitar.
    #[tokio::test]
    async fn a_tela_nao_depende_da_chave_de_conexao_do_cluster() {
        let state = state_with("ksia", vec![]).await;

        let mut sem_credencial = HttpRequest::get("/api/ui/network/trust-anchor")
            .body(Body::empty())
            .unwrap();
        sem_credencial
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
        let response = app(state).oneshot(sem_credencial).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await["anchored"], true);
    }

    /// O lado da ksia: um nó ancorado mostra a impressão digital que o
    /// operador leva consigo, e diz que a raiz é a dele.
    #[tokio::test]
    async fn um_no_ancorado_mostra_a_impressao_digital_da_ancora() {
        let state = state_with("ksia", vec![]).await;
        let esperada = crate::admin_trust::anchor_fingerprint(&state.admin_trust.document());

        let mut request = HttpRequest::get("/api/ui/network/trust-anchor")
            .header("authorization", "Bearer chave")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
        let view = body_of(app(state).oneshot(request).await.unwrap()).await;

        assert_eq!(view["anchored"], true);
        assert_eq!(view["fingerprint"], esperada);
        assert_eq!(view["cluster_id"], "test-cluster");
        assert_eq!(view["is_anchor"], true);
        assert_eq!(view["can_install"], false);
    }
}
