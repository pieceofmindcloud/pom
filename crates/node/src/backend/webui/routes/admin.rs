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
        .route("/api/ui/network/logs", post(save_cluster_logs))
        .route("/api/ui/cluster-logs", get(cluster_logs))
        .route(
            "/api/ui/deploy/capacity-preview",
            post(capacity_preview_endpoint),
        )
        .route("/api/ui/stop-inference", post(stop_inference))
        .route("/api/ui/unload", post(unload_model))
        .route("/api/ui/reconfigure", post(reconfigure))
        .route("/api/ui/release", post(release_slice))
        .route("/api/ui/release/candidate", get(release_candidate))
        .route("/api/ui/leave", post(leave_network))
        .route("/api/ui/rejoin", post(rejoin_network))
        .route("/api/ui/requests", get(crate::reqlog::list_requests))
        .route("/api/ui/requests/stats", get(crate::reqlog::stats_requests))
        .merge(crate::deploy::routes())
        .merge(crate::model_provider_backend::routes())
        .merge(crate::devupdate::router())
}
