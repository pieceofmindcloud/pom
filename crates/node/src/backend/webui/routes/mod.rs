use super::*;

mod admin;
mod common;
mod infer;
mod node;

/// Escopo de perfil anexado a uma rota do gateway. `CommonOrInternal` também é
/// a faixa explícita para protocolos cujo chamador é outro nó e prova a
/// credencial interna compartilhada. Setup e o listener separado de inferência
/// são representados de propósito para não desaparecerem do inventário por
/// omissão.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteScope {
    AdminOnly,
    NodeAdmin,
    InferNodeAdmin,
    CommonOrInternal,
    OutOfScope,
}

struct RouteRule {
    prefix: &'static str,
    scope: RouteScope,
}

/// Uma tabela legível para a fronteira de perfis do backend. Os arquivos de
/// rotas acima organizam o router da UI para leitura humana; esta tabela é o que
/// o middleware do gateway aplica, incluindo rotas registradas por módulos
/// vizinhos do backend.
const ROUTE_RULES: &[RouteRule] = &[
    // Fora desta tarefa de propósito. Setup tem seu próprio router antes de
    // existir um perfil, e o listener de inferência tem porta e middleware
    // separados. Na porta 8181, as rotas fora deste mapa são:
    // /v1/prefill, /v1/step, /v1/step-stream, /v1/link, /v1/session/*,
    // /v1/tp/reduce e /health.
    RouteRule {
        prefix: "/api/setup/",
        scope: RouteScope::OutOfScope,
    },
    // Rotas públicas/comuns do gateway e sondas de política entre nós.
    RouteRule {
        prefix: "/api/access",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/peers",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/health",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/handshake",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/cluster/tls",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/cluster/logs",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/internal/",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/v1/relay-result",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/v1/remote-stream-result",
        scope: RouteScope::CommonOrInternal,
    },
    // APIs compatíveis com inferência. Os callbacks acima vencem por
    // especificidade.
    RouteRule {
        prefix: "/api/generate",
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/generate",
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/v1/",
        scope: RouteScope::InferNodeAdmin,
    },
    // Rotas comuns da UI. Os handlers ainda reduzem os dados do cluster para
    // infer.
    RouteRule {
        prefix: "/api/ui/node",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/cluster",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/peer/",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/network/rtt",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/chat/",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/chat/history",
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/chat-key",
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/network/admin-authority",
        scope: RouteScope::CommonOrInternal,
    },
    // Ancorar a raiz de confiança é o trabalho anterior a existir autoridade:
    // um `profile: admin` por ancorar não passa em `AdminOnly`, e quem recebe
    // a âncora costuma ser `node` ou `infer`. As travas reais são de origem e
    // de estado, no handler; ver `webui/trust.rs`.
    RouteRule {
        prefix: "/api/ui/network/trust-anchor",
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/network/request-node",
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/network/relinquish-node",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/infer/heads",
        scope: RouteScope::InferNodeAdmin,
    },
    // Operações locais do nó e do modelo.
    RouteRule {
        prefix: "/api/ui/models",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/paths",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/thermal",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/cache-types",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/network/advertise-addr",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/plan",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/model-identity",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/model-metadata",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/model-layer-options",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/slice-ranges",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/slice",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/tp/check",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/jobs",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/model-provider/",
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/transfer/",
        scope: RouteScope::NodeAdmin,
    },
    // Operações administrativas e de ciclo de vida.
    RouteRule {
        prefix: "/admin/",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/access",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/tls",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/logs",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/cluster-logs",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/deploy",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/deployments",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/apply",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/launch",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/undeploy",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/stop-inference",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/unload",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/reconfigure",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/release",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/leave",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/rejoin",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/requests",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/dev/",
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/admin/authority",
        scope: RouteScope::AdminOnly,
    },
];

fn is_under(path: &str, prefix: &str) -> bool {
    let base = prefix.strip_suffix('/').unwrap_or(prefix);
    path == base
        || path
            .strip_prefix(base)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

pub(crate) fn route_scope(path: &str) -> RouteScope {
    ROUTE_RULES
        .iter()
        .filter(|rule| is_under(path, rule.prefix))
        .max_by_key(|rule| rule.prefix.len())
        .map(|rule| rule.scope)
        .unwrap_or_else(|| {
            if path.starts_with("/api/setup/") {
                RouteScope::OutOfScope
            } else if path.starts_with("/api/") || path.starts_with("/admin/") {
                // Rotas novas de controle falham fechadas até receberem um
                // perfil nesta tabela.
                RouteScope::AdminOnly
            } else if path.starts_with("/v1/") {
                RouteScope::InferNodeAdmin
            } else {
                RouteScope::CommonOrInternal
            }
        })
}

pub(crate) const fn profile_can_access(
    profile: crate::profile::NodeProfile,
    scope: RouteScope,
) -> bool {
    match scope {
        RouteScope::AdminOnly => matches!(profile, crate::profile::NodeProfile::Admin),
        RouteScope::NodeAdmin => matches!(
            profile,
            crate::profile::NodeProfile::Node | crate::profile::NodeProfile::Admin
        ),
        RouteScope::InferNodeAdmin | RouteScope::CommonOrInternal => true,
        RouteScope::OutOfScope => false,
    }
}

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

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn route_policy_exposes_the_four_declared_scopes() {
        assert_eq!(route_scope("/admin/nodes"), RouteScope::AdminOnly);
        assert_eq!(route_scope("/api/ui/requests/stats"), RouteScope::AdminOnly);
        assert_eq!(route_scope("/api/ui/models"), RouteScope::NodeAdmin);
        assert_eq!(
            route_scope("/api/ui/model-provider/status"),
            RouteScope::NodeAdmin
        );
        assert_eq!(
            route_scope("/api/ui/network/admin-authority"),
            RouteScope::CommonOrInternal
        );
        assert_eq!(
            route_scope("/api/ui/network/trust-anchor"),
            RouteScope::CommonOrInternal
        );
        assert_eq!(
            route_scope("/api/ui/network/request-node"),
            RouteScope::InferNodeAdmin
        );
        assert_eq!(
            route_scope("/api/ui/network/relinquish-node"),
            RouteScope::NodeAdmin
        );
        assert_eq!(
            route_scope("/api/ui/infer/heads"),
            RouteScope::InferNodeAdmin
        );
        assert_eq!(
            route_scope("/v1/chat/completions"),
            RouteScope::InferNodeAdmin
        );
        assert_eq!(route_scope("/api/ui/node"), RouteScope::CommonOrInternal);
        assert_eq!(
            route_scope("/api/ui/chat/history/abc"),
            RouteScope::InferNodeAdmin
        );
        assert_eq!(route_scope("/api/ui/chat-key"), RouteScope::InferNodeAdmin);
        assert_eq!(
            route_scope("/api/internal/api-keys/validate"),
            RouteScope::CommonOrInternal
        );
    }

    #[test]
    fn route_policy_matches_segments_not_similar_prefixes() {
        assert_eq!(route_scope("/api/ui/models-old"), RouteScope::AdminOnly);
        assert_eq!(route_scope("/admin"), RouteScope::AdminOnly);
        assert_eq!(route_scope("/api/ui"), RouteScope::AdminOnly);
        assert_eq!(
            route_scope("/api/ui/requests/stats/today"),
            RouteScope::AdminOnly
        );
        assert_eq!(route_scope("/api/new-control"), RouteScope::AdminOnly);
        assert_eq!(
            route_scope("/api/ui/chat/telemetry"),
            RouteScope::CommonOrInternal
        );
    }

    #[test]
    fn setup_is_explicitly_out_of_scope() {
        assert_eq!(route_scope("/api/setup/save"), RouteScope::OutOfScope);
    }

    #[test]
    fn every_profile_has_a_permitted_and_a_refused_scope() {
        use crate::profile::NodeProfile::{Admin, Infer, Node};

        assert!(profile_can_access(Admin, route_scope("/admin/api-keys")));
        assert!(!profile_can_access(Admin, route_scope("/api/setup/save")));

        assert!(profile_can_access(Node, route_scope("/api/ui/models")));
        assert!(!profile_can_access(Node, route_scope("/admin/api-keys")));

        assert!(profile_can_access(
            Infer,
            route_scope("/api/ui/network/request-node")
        ));
        assert!(!profile_can_access(Infer, route_scope("/api/ui/models")));
    }
}
