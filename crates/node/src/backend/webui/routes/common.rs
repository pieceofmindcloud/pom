use super::super::*;

/// UI routes that belong to every profile. The authorization middleware
/// remains the real barrier; this file is the readable organization of the
/// router.
pub(super) fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/node", get(node_info))
        .route("/api/ui/connection", get(connection_status))
        .route("/api/ui/plugins", get(crate::plugins::list_plugins))
        .route("/api/ui/quota", get(crate::quota::my_quota))
        .route("/api/ui/quota/reset", post(crate::quota::reset_my_session))
        .route("/api/ui/quota/request", post(crate::quota::request_more))
        .route("/api/ui/plugins/ui", get(crate::plugin_ui::list_plugin_ui))
        .route("/api/ui/plugin-catalog", get(crate::plugins::plugin_catalog))
        .route(
            "/api/ui/plugin-catalog/:code",
            get(crate::plugins::plugin_catalog_entry),
        )
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
            "/api/ui/plugins/:code/open",
            post(crate::plugin_ui::plugin_open),
        )
        .route(
            "/api/ui/plugins/:code/close",
            post(crate::plugin_ui::plugin_close),
        )
        .route(
            "/api/internal/plugins/services",
            get(crate::rag::list_services),
        )
        .route(
            "/api/internal/plugins/:code/peer/:op",
            post(crate::plugin_services::plugin_peer_call),
        )
        .route("/api/plugin-host/node", get(crate::plugin_host_api::node))
        .route("/api/plugin-host/peers", get(crate::plugin_host_api::peers))
        .route(
            "/api/plugin-host/peer",
            post(crate::plugin_host_api::peer_call),
        )
        .route("/api/plugin-host/sign", post(crate::plugin_host_api::sign))
        .route(
            "/api/plugin-host/catalog/images",
            get(crate::plugin_host_api::catalog_images),
        )
        .route(
            "/api/plugin-host/catalog/refresh",
            post(crate::plugin_host_api::catalog_refresh),
        )
        .route(
            "/api/plugin-host/components",
            get(crate::plugin_host_api::components),
        )
        .route(
            "/api/plugin-host/image-job/begin",
            post(crate::plugin_host_api::image_job_begin),
        )
        .route(
            "/api/plugin-host/image-job/complete",
            post(crate::plugin_host_api::image_job_complete),
        )
        .route(
            "/api/plugin-host/component/:deploy_id/:component/:op",
            post(crate::plugin_host_api::component_call).layer(
                axum::extract::DefaultBodyLimit::max(crate::components::MAX_COMPONENT_BODY),
            ),
        )
        .route(
            "/api/internal/plugins/:code/component/:deploy_id/:component/:op",
            post(crate::components::internal_component_route).layer(
                axum::extract::DefaultBodyLimit::max(crate::components::MAX_COMPONENT_BODY),
            ),
        )
        .route(
            "/api/internal/plugins/:code/engine/generate",
            post(crate::plugin_engine::engine_generate_route).layer(
                axum::extract::DefaultBodyLimit::max(crate::plugin_engine::MAX_GENERATION_BODY),
            ),
        )
        .route(
            "/api/internal/plugins/:code/engine/session/chunk",
            post(crate::plugin_session::engine_session_chunk_route).layer(
                axum::extract::DefaultBodyLimit::max(crate::plugin_session::SESSION_PIECE + 4096),
            ),
        )
        .route(
            "/api/internal/plugins/:code/engine/session/apply",
            post(crate::plugin_session::engine_session_apply_route),
        )
        .route(
            "/api/internal/plugins/:code/engine/session/piece",
            post(crate::plugin_session::engine_session_piece_route),
        )
        .route(
            "/api/internal/plugins/:code/engine/session/offer",
            get(crate::plugin_session::engine_session_offer_route),
        )
        .route(
            "/api/internal/plugins/:code/engine/session/file",
            get(crate::plugin_session::engine_session_file_route),
        )
        .route(
            "/api/internal/plugins/:code/engine/link/:deploy",
            get(crate::plugin_stage_link::link_secret_route),
        )
        .route(
            "/api/internal/plugins/:code/engine/link/:deploy/:conn",
            post(crate::plugin_stage_link::link_frame_route).layer(
                axum::extract::DefaultBodyLimit::max(crate::plugin_stage_link::MAX_FRAME),
            ),
        )
        .route(
            "/api/internal/plugins/:code/engine/link/:deploy/:conn/:seq",
            post(crate::plugin_stage_link::link_frame_seq_route).layer(
                axum::extract::DefaultBodyLimit::max(crate::plugin_stage_link::MAX_FRAME),
            ),
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
            get(super::super::sidecars::sidecar_links)
                .post(super::super::sidecars::save_sidecar_link),
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
        .route("/api/ui/tls/ca", get(super::super::https::ca_status))
        .route("/api/ui/tls/ca.crt", get(super::super::https::ca_certificate))
        .route("/api/ui/tls/ca/install", post(super::super::https::ca_install))
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
        .route("/api/ui/networks/pinned", get(pinned_networks))
        .route("/api/ui/networks/known", get(known_networks))
        .route("/api/ui/networks/custom", post(import_custom_network))
        .route(
            "/api/ui/networks/custom/:fingerprint",
            delete(remove_custom_network),
        )
        .route("/api/ui/network-chat/lobby/info", get(lobby_info))
        .route("/api/ui/network-chat/lobby/latency", get(lobby_latency))
        .route(
            "/api/ui/network-chat/lobby/admission",
            post(lobby_admission),
        )
        .route("/api/ui/network-chat/lobby/requests", get(lobby_requests))
        .route(
            "/api/ui/network-chat/lobby/requests/forget",
            post(lobby_request_forget),
        )
        // Anchoring admin trust is the only job that needs a screen
        // **before** any authority exists: see the header of `webui/trust.rs`.
        .route(
            "/api/ui/network/trust-anchor",
            get(trust_anchor).post(install_trust_anchor),
        )
}
