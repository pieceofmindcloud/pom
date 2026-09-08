use super::super::*;

/// Rotas disponíveis para inferência: pedidos de entrada e chat da UI.
pub(super) fn routes() -> Router<GatewayState> {
    Router::new()
        .route(
            "/api/ui/chat/history",
            get(chat_history)
                .post(save_chat_history)
                .delete(clear_chat_history),
        )
        .route("/api/ui/chat/history/:id", delete(delete_chat_history))
        .route("/api/ui/chat-key", post(crate::admin::chat_key))
}
