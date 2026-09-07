use super::super::*;

/// Rotas disponíveis para inferência: auto-promoção e chat da UI.
pub(super) fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/network/promote", post(promote_self_to_node))
        .route(
            "/api/ui/chat/history",
            get(chat_history)
                .post(save_chat_history)
                .delete(clear_chat_history),
        )
        .route("/api/ui/chat/history/:id", delete(delete_chat_history))
        .route("/api/ui/chat-key", post(crate::admin::chat_key))
}
