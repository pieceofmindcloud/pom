use super::super::*;

/// Rotas da UI que pertencem a todos os perfis. O middleware de autorização
/// continua sendo a barreira real; este arquivo é a organização legível do
/// router.
pub(super) fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/node", get(node_info))
        .route("/api/ui/plugins", get(crate::plugins::list_plugins))
        .route("/api/ui/plugins/ui", get(crate::plugin_ui::list_plugin_ui))
        .route(
            "/api/ui/plugins/events",
            get(crate::plugins::list_plugin_events),
        )
        .route(
            "/api/ui/plugins/:code/assets/*path",
            get(crate::plugin_ui::plugin_asset),
        )
        .route(
            "/api/ui/plugins/:code/rpc/:op",
            post(crate::plugin_ui::plugin_rpc),
        )
        .route(
            "/api/internal/plugins/services",
            get(crate::rag::list_services),
        )
        .route(
            "/api/internal/plugins/:code/peer/:op",
            post(crate::plugin_services::plugin_peer_call),
        )
        .route(
            "/api/ui/pom-update/status",
            get(crate::pom_core_update::pom_update_status),
        )
        .route("/api/ui/cluster", get(cluster))
        .route("/api/ui/chat/capabilities", get(chat_capabilities))
        .route("/api/ui/cluster/models", get(cluster_models))
        .route(
            "/api/ui/model-sidecar-links",
            get(super::super::sidecars::sidecar_links).post(super::super::sidecars::save_sidecar_link),
        )
        .route("/api/ui/peer/:node_id/runtime", get(peer_runtime))
        .route("/api/ui/network/rtt", get(network_rtt))
        .route("/api/ui/network/info", get(network_info))
        .route(
            "/api/ui/network/firewall",
            get(crate::windows_firewall::status),
        )
        .route(
            "/api/ui/network/firewall/retry",
            post(crate::windows_firewall::retry),
        )
        .route("/api/ui/network/geo/countries", get(geo_countries))
        .route("/api/ui/network/geo/cities", get(geo_cities))
        .route("/api/ui/network/geo/nearest", get(geo_nearest))
        .route("/api/ui/chat/telemetry", post(chat_telemetry))
        .route(
            "/api/ui/network-chat",
            get(network_chat_snapshot).post(network_chat_action),
        )
        .route(
            "/api/ui/network-chat/file/:message_id",
            get(network_chat_file),
        )
        .route(
            "/api/ui/network-chat/lobby",
            get(lobby_fetch).post(lobby_post),
        )
        .route("/api/ui/network-chat/lobby/info", get(lobby_info))
        .route("/api/ui/network-chat/lobby/admission", post(lobby_admission))
        .route("/api/ui/network-chat/lobby/requests", get(lobby_requests))
        .route(
            "/api/ui/network-chat/lobby/requests/forget",
            post(lobby_request_forget),
        )
        // Ancorar a confiança de admin é o único trabalho que precisa de tela
        // **antes** de existir autoridade: ver o cabeçalho de `webui/trust.rs`.
        .route(
            "/api/ui/network/trust-anchor",
            get(trust_anchor).post(install_trust_anchor),
        )
}
