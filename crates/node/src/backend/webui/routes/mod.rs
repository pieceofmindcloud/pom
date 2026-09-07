use super::*;

mod admin;
mod common;
mod infer;
mod node;

/// Monta a superfície de controle sem fazer da organização dos arquivos a
/// única barreira de segurança. A política de perfil fica neste módulo e o
/// middleware do gateway a aplica depois, no caminho HTTP compartilhado.
pub fn routes() -> Router<GatewayState> {
    Router::new()
        .merge(common::routes())
        .merge(infer::routes())
        .merge(node::routes())
        .merge(admin::routes())
}
