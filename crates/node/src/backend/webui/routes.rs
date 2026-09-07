use super::*;
pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/node", get(node_info))
        .route(
            "/api/ui/access",
            get(admin_access_info).post(save_admin_access),
        )
        .route("/api/ui/models", get(models))
        .route("/api/ui/models/delete", post(delete_model))
        .route("/api/ui/cluster", get(cluster))
        .route("/api/ui/cluster/models", get(cluster_models))
        .route("/api/ui/peer/:node_id/runtime", get(peer_runtime))
        .route("/api/ui/network/rtt", get(network_rtt))
        .route("/api/ui/chat/telemetry", post(chat_telemetry))
        .route(
            "/api/ui/chat/history",
            get(chat_history)
                .post(save_chat_history)
                .delete(clear_chat_history),
        )
        .route("/api/ui/chat/history/:id", delete(delete_chat_history))
        .route("/api/ui/paths", get(get_paths).post(save_paths))
        .route("/api/ui/thermal", get(get_thermal).post(save_thermal))
        .route(
            "/api/ui/cache-types",
            get(get_cache_types).post(save_cache_types),
        )
        .route("/api/ui/network/advertise-addr", post(save_advertise_addr))
        .route("/api/ui/network/tls", post(save_tls))
        .route("/api/ui/network/logs", post(save_cluster_logs))
        .route("/api/ui/cluster-logs", get(cluster_logs))
        .route("/api/ui/plan", post(plan_split_endpoint))
        .route(
            "/api/ui/deploy/capacity-preview",
            post(capacity_preview_endpoint),
        )
        .route("/api/ui/apply", post(apply))
        .route("/api/ui/launch", post(launch))
        .route("/api/ui/model-identity", post(model_identity))
        .route("/api/ui/model-metadata", post(model_metadata))
        .route("/api/ui/model-layer-options", post(model_layer_options))
        .route("/api/ui/slice-ranges", post(slice_ranges))
        .route("/api/ui/stop-inference", post(stop_inference))
        .route("/api/ui/slice", post(slice_locally))
        .route("/api/ui/slice/plan", post(slice_plan))
        .route("/api/ui/tp/check", post(tp_check))
        .route("/api/ui/jobs", get(list_jobs))
        .route("/api/ui/jobs/:id", get(job_status))
        .route("/api/ui/unload", post(unload_model))
        .route("/api/ui/reconfigure", post(reconfigure))
        .route("/api/ui/release", post(release_slice))
        .route("/api/ui/release/candidate", get(release_candidate))
        .route("/api/ui/leave", post(leave_network))
        .route("/api/ui/rejoin", post(rejoin_network))
        .route("/api/ui/requests", get(crate::reqlog::list_requests))
        .route("/api/ui/requests/stats", get(crate::reqlog::stats_requests))
    // `/api/setup/key` e `/api/setup/save` NAO entram aqui de proposito.
    //
    // Elas nao tem autenticacao - nao podem ter, e o momento em que a chave do
    // cluster nasce - e so fazem sentido antes de existir configuracao. Num no
    // ja configurado elas eram um portao aberto: `GET /api/setup/key` troca o
    // `connection_key` em memoria, entao qualquer um alcancando a porta do
    // gateway rotacionava a credencial de operador, derrubava a chave legitima
    // (403) e ficava com acesso total a /admin, /api/ui/apply e /v1.
    // Reproduzido em 2026-08-08 num no local; com o no atendendo IPv6 publico
    // isso deixou de ser teorico.
    //
    // Quem precisa delas e o modo setup, que monta o proprio roteador com estas
    // duas rotas e a setup.html (ver `start_setup_mode` em main.rs). O unico
    // consumidor e aquela pagina, servida so enquanto nao ha `pom.json`.
}

