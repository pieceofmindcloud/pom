use super::super::*;

/// Rotas da UI que pertencem a todos os perfis. O middleware de autorização
/// continua sendo a barreira real; este arquivo é a organização legível do
/// router.
pub(super) fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/node", get(node_info))
        .route("/api/ui/cluster", get(cluster))
        .route("/api/ui/cluster/models", get(cluster_models))
        .route("/api/ui/peer/:node_id/runtime", get(peer_runtime))
        .route("/api/ui/network/rtt", get(network_rtt))
        .route("/api/ui/chat/telemetry", post(chat_telemetry))
        // Ancorar a confiança de admin é o único trabalho que precisa de tela
        // **antes** de existir autoridade: ver o cabeçalho de `webui/trust.rs`.
        .route(
            "/api/ui/network/trust-anchor",
            get(trust_anchor).post(install_trust_anchor),
        )
}
