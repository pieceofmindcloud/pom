//! Atualizar e reiniciar um nó pela interface, em desenvolvimento.
//!
//! Isto é execução remota de código: quem tem a chave de conexão passa a poder
//! rodar um build e trocar o binário da máquina. Por isso o portão é uma
//! variável de ambiente, ligada por processo por quem é dono da máquina, e não
//! um campo de config — a config é reescrita pela própria interface, o que
//! deixaria o portão ligável pela rede.

/// Nome da variável que abre o recurso.
pub const ENV_GATE: &str = "RDMA_P2P_DEV_UPDATE";

/// Variável que diz com que features esta máquina compila.
pub const ENV_FEATURES: &str = "RDMA_P2P_BUILD_FEATURES";

/// Este nó aceita pedidos de atualização?
pub fn enabled() -> bool {
    enabled_from(std::env::var(ENV_GATE).ok().as_deref())
}

fn enabled_from(v: Option<&str>) -> bool {
    v == Some("1")
}

use crate::gateway::GatewayState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::json;

/// A recusa do portão, ou `None` quando ele está aberto.
fn gate_rejection() -> Option<(StatusCode, String)> {
    if enabled() {
        return None;
    }
    Some((
        StatusCode::FORBIDDEN,
        format!(
            "atualização remota desligada neste nó. Suba o processo com \
             {ENV_GATE}=1 para habilitar."
        ),
    ))
}

fn refuse(r: (StatusCode, String)) -> Response {
    (r.0, Json(json!({ "error": r.1 }))).into_response()
}

/// Folga entre a resposta HTTP sair e o processo morrer.
///
/// O corpo é um JSON minúsculo; numa LAN, escrevê-lo na conexão leva
/// microssegundos. 300ms é folga generosa para isso mesmo passando por um
/// proxy reverso local, sem ser tão longo que o operador sinta demora no
/// reinício. Os dois lados do erro são assimétricos: passar deste valor só
/// custa ao operador esperar um pouco mais; ficar aquém dele corta a conexão
/// do cliente antes do byte chegar, e o pedido parece ter falhado quando na
/// verdade funcionou.
const RESTART_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

pub fn router() -> Router<GatewayState> {
    Router::new().route("/api/ui/dev/restart", post(restart_endpoint))
}

async fn restart_endpoint(
    _auth: crate::auth::ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    if let Some(r) = gate_rejection() {
        return refuse(r);
    }

    // Anota a fatia antes de sair, para voltar servindo o mesmo.
    if let Some(cfg) = state.runtime.config().await {
        if let Err(e) = crate::lastslice::save(&cfg) {
            tracing::warn!("não deu para anotar a fatia antes do reinício: {e}");
        }
    }

    tokio::spawn(async {
        tokio::time::sleep(RESTART_DELAY).await;
        let e = crate::respawn::respawn_self();
        tracing::error!("respawn falhou, o nó segue no ar: {e}");
    });

    Json(json!({ "restarting": true })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O portão é por processo e não é ligável pela rede — diferente da config,
    /// que a própria interface reescreve. Só "1" liga: um "true" ou "yes" que
    /// alguém escreveu achando que ligava não deve abrir execução remota.
    #[test]
    fn so_o_valor_1_liga_o_portao() {
        assert!(enabled_from(Some("1")));
        assert!(!enabled_from(Some("0")));
        assert!(!enabled_from(Some("true")));
        assert!(!enabled_from(Some("")));
        assert!(!enabled_from(None));
    }

    /// Com o portão fechado a rota não existe para efeitos práticos: responde
    /// 403 sem tocar em nada.
    #[test]
    fn portao_fechado_recusa_com_403() {
        // Num ambiente de teste a variável não está ligada, então há recusa.
        let r = gate_rejection().expect("sem a variável, tem que recusar");
        assert_eq!(r.0, axum::http::StatusCode::FORBIDDEN);
        assert!(
            r.1.contains(ENV_GATE),
            "a mensagem tem que dizer qual variável liga: {}",
            r.1
        );
    }
}
