use super::super::*;

/// Routes available for inference and for requesting entry into processing.
pub(super) fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/infer/heads", get(inference_heads))
        .route(
            "/api/ui/chat/history",
            get(chat_history)
                .post(save_chat_history)
                .delete(clear_chat_history),
        )
        .route("/api/ui/chat/history/:id", delete(delete_chat_history))
        .route("/api/ui/chat-key", post(crate::admin::chat_key))
}
