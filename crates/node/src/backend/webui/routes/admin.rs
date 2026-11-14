use super::super::*;

/// Administrative and lifecycle routes. The effective authorization remains
/// in the gateway middleware and in the handlers' operational policies.
pub(super) fn routes() -> Router<GatewayState> {
    Router::new()
        .route(
            "/api/ui/access",
            get(admin_access_info).post(save_admin_access),
        )
        .route("/api/ui/access/password", post(change_admin_password))
        .route("/api/ui/network/settings", post(save_network_settings))
        .route("/api/ui/network/trust-share", get(trust_share))
        .route("/api/ui/network/quic", post(save_quic))
        .route("/api/ui/network/relay-policy", post(save_relay_policy))
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
        .route(
            "/api/ui/components/apply",
            post(crate::components::apply_route),
        )
        .route(
            "/api/ui/components/release",
            post(crate::components::release_route),
        )
        .route(
            "/api/ui/image-deploy",
            post(crate::components::image_deploy_route),
        )
        .route(
            "/api/ui/image-deploy/plan",
            post(crate::image_plan::plan_route),
        )
        .route(
            "/api/ui/image-deploy/:deploy_id/add",
            post(crate::image_plan::add_route),
        )
        .route(
            "/api/ui/image-deploy/:deploy_id/remove",
            post(crate::image_plan::remove_route),
        )
        .route(
            "/api/ui/image-deploy/:deploy_id/redeploy",
            post(crate::image_plan::redeploy_route),
        )
        .route(
            "/api/ui/image-undeploy",
            post(crate::components::image_undeploy_route),
        )
        .route(
            "/api/ui/image-deployments",
            axum::routing::get(crate::components::image_deployments_route),
        )
        .route("/api/ui/launch", post(launch))
        .route("/api/ui/stop-inference", post(stop_inference))
        .route("/api/ui/unload", post(unload_model))
        .route("/api/ui/kv-cache/clear", post(clear_kv_cache))
        .route("/api/ui/reconfigure", post(reconfigure))
        .route("/api/ui/release", post(release_slice))
        .route("/api/ui/release/candidate", get(release_candidate))
        .route("/api/ui/network/leave", post(leave_network_and_reset))
        .route("/api/ui/network/join", post(join_network))
        .route(
            "/api/ui/network/admissions",
            get(admissions).post(decide_admission),
        )
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
        .route(
            "/api/ui/plugins/dev-sources",
            get(crate::plugins::list_development_plugin_sources),
        )
        .route(
            "/api/ui/plugins/dev-sources/:repository/rebuild-install",
            post(crate::plugins::rebuild_install_development_plugin),
        )
        .route(
            "/api/ui/plugins/github-repositories",
            get(crate::plugins::list_github_repositories)
                .post(crate::plugins::add_github_repository),
        )
        .route(
            "/api/ui/plugins/github-repositories/:id/refresh",
            post(crate::plugins::refresh_github_repository),
        )
        .route(
            "/api/ui/plugins/github-repositories/:id/install",
            post(crate::plugins::install_github_repository_release),
        )
        .route(
            "/api/ui/plugins/github-repositories/:id",
            axum::routing::delete(crate::plugins::remove_github_repository),
        )
        .route(
            "/api/ui/plugins/checkout",
            post(crate::plugins::start_plugin_checkout),
        )
        .route(
            "/api/ui/plugins/:code/preferences",
            post(crate::plugins::save_plugin_preferences),
        )
        // Web interface served by the plugin itself (see `crate::plugin_proxy`).
        // Uploads cross the proxy, so the body does not have the default cap.
        .route(
            "/api/ui/plugins/:code/proxy",
            axum::routing::any(crate::plugin_proxy::plugin_proxy)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/api/ui/plugins/:code/proxy/",
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
