use super::*;
use axum::http::Method;

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
    /// `None` vale para todo método. Um método explícito só casa com ele, e
    /// existe porque uma rota administrativa pode ter uma leitura de perfil
    /// mais largo que a sua escrita: `GET /admin/nodes` é o retrato da rede
    /// que a tela Rede precisa, enquanto bloquear, remover e promover uma
    /// máquina continuam sendo ato de administrador sob o mesmo prefixo.
    method: Option<Method>,
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
        method: None,
        scope: RouteScope::OutOfScope,
    },
    // Rotas públicas/comuns do gateway e sondas de política entre nós.
    RouteRule {
        prefix: "/api/access",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/public/",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/peers",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/health",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/handshake",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/cluster/tls",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/cluster/quic",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/cluster/logs",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/cluster/network-chat",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/cluster/network-settings",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/internal/",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/v1/relay-result",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/v1/remote-stream-result",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    // APIs compatíveis com inferência. Os callbacks acima vencem por
    // especificidade.
    RouteRule {
        prefix: "/api/generate",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/generate",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/v1/",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    // Rotas comuns da UI. Os handlers ainda reduzem os dados do cluster para
    // infer.
    RouteRule {
        prefix: "/api/ui/node",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/cluster",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/peer/",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/network/rtt",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/network/info",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/network/geo-reference",
        method: Some(Method::GET),
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/network-chat",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/chat/",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/chat/history",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/chat-key",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/network/admin-authority",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    // Ancorar a raiz de confiança é o trabalho anterior a existir autoridade:
    // um `profile: admin` por ancorar não passa em `AdminOnly`, e quem recebe
    // a âncora costuma ser `node` ou `infer`. As travas reais são de origem e
    // de estado, no handler; ver `webui/trust.rs`.
    RouteRule {
        prefix: "/api/ui/network/trust-anchor",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/network/request-node",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/network/relinquish-node",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/infer/heads",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    // Operações locais do nó e do modelo. O endereço anunciado é a descrição
    // desta própria máquina, por isso também é permitido ao perfil `infer`.
    RouteRule {
        prefix: "/api/ui/models",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/paths",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/thermal",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/cache-types",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/network/advertise-addr",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/network/settings",
        method: Some(Method::POST),
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/trust-share",
        method: Some(Method::GET),
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/leave",
        method: Some(Method::POST),
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/plan",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/model-identity",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/model-metadata",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/model-layer-options",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/slice-ranges",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/slice",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/tp/check",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/jobs",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/model-provider/",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/transfer/",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    // Operações administrativas e de ciclo de vida.
    RouteRule {
        prefix: "/admin/",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    // As API keys são a credencial de quem atende inferência, e todo perfil
    // atende: `/v1` é `InferNodeAdmin`. Um `infer` troca a própria chave pela
    // tela de API KEYS, que é o destino do rodapé dele. Negar aqui deixava os
    // dois perfis sem como emitir o token que o próprio nó exige.
    RouteRule {
        prefix: "/admin/api-keys",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    // Ler quem está na rede é o que a tela Rede mostra; bloquear, desbloquear,
    // remover e promover uma máquina são POST/DELETE e continuam sob
    // `/admin/`, isto é, `AdminOnly`.
    RouteRule {
        prefix: "/admin/nodes",
        method: Some(Method::GET),
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/access",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/tls",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/quic",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/logs",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/cluster-logs",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/deploy",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/deployments",
        method: Some(Method::GET),
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/deployments",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/apply",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/launch",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/undeploy",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/stop-inference",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/unload",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/reconfigure",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/release",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    // Sair da rede é abdicar do próprio posto, não um privilégio: qualquer
    // perfil desliga a própria máquina a qualquer momento, e voltar é a outra
    // metade do mesmo interruptor. Mandar **outra** máquina sair continua
    // sendo ato de administrador, e essa trava é do alvo do pedido, não do
    // caminho: fica em `webui::lifecycle::leave_network`.
    RouteRule {
        prefix: "/api/ui/leave",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/rejoin",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/requests",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/dev/",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/admin/authority",
        method: None,
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

/// O escopo desta requisição. Vence a regra mais específica: primeiro o
/// prefixo mais longo, e no empate a que nomeia o método - assim uma leitura
/// pode ser mais larga que a escrita no mesmo caminho sem que o resto do
/// prefixo deixe de ser administrativo.
pub(crate) fn route_scope(method: &Method, path: &str) -> RouteScope {
    ROUTE_RULES
        .iter()
        .filter(|rule| is_under(path, rule.prefix))
        .filter(|rule| rule.method.as_ref().is_none_or(|allowed| allowed == method))
        .max_by_key(|rule| (rule.prefix.len(), rule.method.is_some()))
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

    /// Atalho de leitura: quase toda regra da tabela é indiferente ao método.
    fn scope_of(path: &str) -> RouteScope {
        route_scope(&Method::GET, path)
    }

    #[test]
    fn route_policy_exposes_the_four_declared_scopes() {
        assert_eq!(
            route_scope(&Method::DELETE, "/admin/nodes/abc"),
            RouteScope::AdminOnly
        );
        assert_eq!(scope_of("/api/ui/requests/stats"), RouteScope::AdminOnly);
        assert_eq!(scope_of("/api/ui/models"), RouteScope::NodeAdmin);
        assert_eq!(
            scope_of("/api/ui/model-provider/status"),
            RouteScope::NodeAdmin
        );
        assert_eq!(
            scope_of("/api/ui/network/advertise-addr"),
            RouteScope::InferNodeAdmin
        );
        assert_eq!(
            scope_of("/api/ui/network/admin-authority"),
            RouteScope::CommonOrInternal
        );
        assert_eq!(
            scope_of("/api/ui/network/trust-anchor"),
            RouteScope::CommonOrInternal
        );
        assert_eq!(
            scope_of("/api/ui/network/geo-reference"),
            RouteScope::InferNodeAdmin
        );
        assert_eq!(scope_of("/api/ui/deployments"), RouteScope::NodeAdmin);
        assert_eq!(
            route_scope(&Method::POST, "/api/ui/deployments"),
            RouteScope::AdminOnly
        );
        assert_eq!(
            scope_of("/api/ui/network/request-node"),
            RouteScope::InferNodeAdmin
        );
        assert_eq!(
            scope_of("/api/ui/network/relinquish-node"),
            RouteScope::NodeAdmin
        );
        assert_eq!(scope_of("/api/ui/infer/heads"), RouteScope::InferNodeAdmin);
        assert!(profile_can_access(
            crate::profile::NodeProfile::Admin,
            scope_of("/api/ui/infer/heads")
        ));
        assert_eq!(scope_of("/v1/chat/completions"), RouteScope::InferNodeAdmin);
        assert_eq!(scope_of("/api/ui/node"), RouteScope::CommonOrInternal);
        assert_eq!(
            scope_of("/api/ui/chat/capabilities"),
            RouteScope::CommonOrInternal
        );
        assert_eq!(
            scope_of("/api/ui/chat/history/abc"),
            RouteScope::InferNodeAdmin
        );
        assert_eq!(scope_of("/api/ui/chat-key"), RouteScope::InferNodeAdmin);
        assert_eq!(
            scope_of("/api/internal/api-keys/validate"),
            RouteScope::CommonOrInternal
        );
    }

    #[test]
    fn route_policy_matches_segments_not_similar_prefixes() {
        assert_eq!(scope_of("/api/ui/models-old"), RouteScope::AdminOnly);
        assert_eq!(scope_of("/admin"), RouteScope::AdminOnly);
        assert_eq!(scope_of("/api/ui"), RouteScope::AdminOnly);
        assert_eq!(
            scope_of("/api/ui/requests/stats/today"),
            RouteScope::AdminOnly
        );
        assert_eq!(scope_of("/api/new-control"), RouteScope::AdminOnly);
        assert_eq!(
            scope_of("/api/ui/chat/telemetry"),
            RouteScope::CommonOrInternal
        );
    }

    /// A tela Rede do perfil `node` lê o retrato da rede; mudar o que ela
    /// mostra continua sendo ato de administrador, e as duas coisas moram sob
    /// o mesmo prefixo.
    #[test]
    fn reading_the_roster_is_wider_than_administering_it() {
        assert_eq!(
            route_scope(&Method::GET, "/admin/nodes"),
            RouteScope::NodeAdmin
        );
        assert_eq!(
            route_scope(&Method::POST, "/admin/nodes/abc/block"),
            RouteScope::AdminOnly
        );
        assert_eq!(
            route_scope(&Method::POST, "/admin/nodes/abc/unblock"),
            RouteScope::AdminOnly
        );
        assert_eq!(
            route_scope(&Method::POST, "/admin/nodes/abc/promote-admin"),
            RouteScope::AdminOnly
        );
        assert_eq!(
            route_scope(&Method::DELETE, "/admin/nodes/abc"),
            RouteScope::AdminOnly
        );
        assert_eq!(
            route_scope(&Method::POST, "/admin/regenerate-key"),
            RouteScope::AdminOnly
        );
    }

    /// A API key é a credencial de quem atende `/v1`, e todo perfil atende.
    #[test]
    fn every_profile_can_mint_the_key_its_own_inference_demands() {
        for method in [Method::GET, Method::POST, Method::DELETE] {
            assert_eq!(
                route_scope(&method, "/admin/api-keys"),
                RouteScope::InferNodeAdmin,
                "método={method}"
            );
        }
        assert_eq!(
            route_scope(&Method::DELETE, "/admin/api-keys/abc"),
            RouteScope::InferNodeAdmin
        );
    }

    /// Sair da rede é abdicar: nenhum perfil precisa de licença para isso.
    #[test]
    fn leaving_and_rejoining_belong_to_every_profile() {
        use crate::profile::NodeProfile::{Admin, Infer, Node};

        for path in ["/api/ui/leave", "/api/ui/rejoin"] {
            let scope = route_scope(&Method::POST, path);
            assert_eq!(scope, RouteScope::InferNodeAdmin, "rota={path}");
            for profile in [Infer, Node, Admin] {
                assert!(profile_can_access(profile, scope), "{profile} em {path}");
            }
        }
    }

    #[test]
    fn setup_is_explicitly_out_of_scope() {
        assert_eq!(scope_of("/api/setup/save"), RouteScope::OutOfScope);
    }

    #[test]
    fn every_profile_has_a_permitted_and_a_refused_scope() {
        use crate::profile::NodeProfile::{Admin, Infer, Node};

        assert!(profile_can_access(Admin, scope_of("/admin/api-keys")));
        assert!(!profile_can_access(Admin, scope_of("/api/setup/save")));

        assert!(profile_can_access(Node, scope_of("/api/ui/models")));
        assert!(!profile_can_access(
            Node,
            route_scope(&Method::POST, "/admin/regenerate-key")
        ));

        assert!(profile_can_access(
            Infer,
            scope_of("/api/ui/network/request-node")
        ));
        assert!(!profile_can_access(Infer, scope_of("/api/ui/models")));
    }
}
