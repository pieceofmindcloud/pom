use super::super::*;

/// Rotas de operação da máquina local e de preparação de modelos. O
/// middleware mantém a autorização para `node` e `admin` independente deste
/// agrupamento de apresentação.
pub(super) fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/models", get(models))
        .route("/api/ui/models/delete", post(delete_model))
        .route("/api/ui/paths", get(get_paths).post(save_paths))
        .route("/api/ui/thermal", get(get_thermal).post(save_thermal))
        .route(
            "/api/ui/cache-types",
            get(get_cache_types).post(save_cache_types),
        )
        .route("/api/ui/network/advertise-addr", post(save_advertise_addr))
        .route("/api/ui/network/relay", get(get_relay).post(save_relay))
        .route("/api/ui/plan", post(plan_split_endpoint))
        .route("/api/ui/model-identity", post(model_identity))
        .route("/api/ui/model-metadata", post(model_metadata))
        .route("/api/ui/model-layer-options", post(model_layer_options))
        .route("/api/ui/slice-ranges", post(slice_ranges))
        .route("/api/ui/slice", post(slice_locally))
        .route("/api/ui/slice/plan", post(slice_plan))
        .route("/api/ui/tp/check", post(tp_check))
        .route("/api/ui/jobs", get(list_jobs))
        .route("/api/ui/jobs/:id", get(job_status))
}
