use super::super::*;

/// Rotas administrativas e de ciclo de vida. A autorização efetiva continua
/// no middleware do gateway e nas políticas operacionais dos handlers.
pub(super) fn routes() -> Router<GatewayState> {
    Router::new()
        .route(
            "/api/ui/access",
            get(admin_access_info).post(save_admin_access),
        )
        .route("/api/ui/network/tls", post(save_tls))
        .route("/api/ui/network/settings", post(save_network_settings))
        .route("/api/ui/network/trust-share", get(trust_share))
        .route("/api/ui/network/quic", post(save_quic))
        .route("/api/ui/network/logs", post(save_cluster_logs))
        .route("/api/ui/network/chat", post(save_network_chat))
        .route(
            "/api/ui/network/auto-discovery",
            get(auto_discovery_status).post(save_auto_discovery),
        )
        .route("/api/ui/cluster-logs", get(cluster_logs))
        .route(
            "/api/ui/deploy/capacity-preview",
            post(capacity_preview_endpoint),
        )
        .route("/api/ui/apply", post(apply))
        .route("/api/ui/launch", post(launch))
        .route("/api/ui/stop-inference", post(stop_inference))
        .route("/api/ui/unload", post(unload_model))
        .route("/api/ui/reconfigure", post(reconfigure))
        .route("/api/ui/release", post(release_slice))
        .route("/api/ui/release/candidate", get(release_candidate))
        .route("/api/ui/network/leave", post(leave_network_and_reset))
        .route("/api/ui/leave", post(leave_network))
        .route("/api/ui/rejoin", post(rejoin_network))
        .route("/api/ui/requests", get(crate::reqlog::list_requests))
        .route("/api/ui/requests/stats", get(crate::reqlog::stats_requests))
        .route(
            "/api/ui/plugins/activate",
            post(crate::plugins::activate_plugin),
        )
        .route(
            "/api/ui/plugins/:code/refresh",
            post(crate::plugins::refresh_plugin),
        )
        .route(
            "/api/ui/plugins/:code/deactivate",
            post(crate::plugins::deactivate_plugin),
        )
        .route(
            "/api/ui/plugins/:code/update/check",
            post(crate::plugins::check_plugin_update),
        )
        .route(
            "/api/ui/plugins/:code/update/apply",
            post(crate::plugins::apply_plugin_update),
        )
        // Interface web servida pelo próprio plugin (ver `crate::plugin_proxy`).
        // Uploads atravessam o proxy, então o corpo não tem o teto padrão.
        .route(
            "/api/ui/plugins/:code/proxy",
            axum::routing::any(crate::plugin_proxy::plugin_proxy)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/api/ui/plugins/:code/proxy/*path",
            axum::routing::any(crate::plugin_proxy::plugin_proxy)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/api/ui/pom-update/check",
            post(crate::pom_core_update::check_pom_update),
        )
        .route(
            "/api/ui/pom-update/apply",
            post(crate::pom_core_update::apply_pom_update),
        )
        .merge(crate::deploy::routes())
        .merge(crate::model_provider_backend::routes())
        .merge(crate::devupdate::router())
}
