use super::super::*;

/// Rotas próprias da promoção de um nó de inferência para nó comum.
pub(super) fn routes() -> Router<GatewayState> {
    Router::new().route("/api/ui/network/promote", post(promote_self_to_node))
}
