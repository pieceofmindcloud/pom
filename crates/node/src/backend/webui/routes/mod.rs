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
    /// The network's principal only. A secondary admin does not reach these:
    /// everything that governs the network (members, admissions, settings,
    /// keys, the trust chain) comes from the principal.
    AdminOnly,
    /// Any admin of the trust chain, principal or secondary: putting models
    /// in and out of service. It is the one thing a secondary admin may do
    /// that a node may not.
    DeployAdmin,
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
        prefix: "/api/cluster/member-left",
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
    // Plugins são locais de cada nó (ver `crate::plugins`): ativar uma
    // licença, cadastrar um repositório GitHub, instalar, atualizar e usar a
    // interface de um plugin é ato de qualquer perfil sobre a própria máquina.
    // A regra casa por prefixo de segmento e vale para todo método, inclusive
    // sob `/api/ui/plugins/:code/...` e no proxy de interfaces.
    RouteRule {
        prefix: "/api/ui/plugins",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    // Atualização do binário `pom` em si - fronteira separada dos plugins de
    // propósito, ver `crate::pom_core_update`.
    RouteRule {
        prefix: "/api/ui/pom-update",
        method: Some(Method::GET),
        scope: RouteScope::CommonOrInternal,
    },
    RouteRule {
        prefix: "/api/ui/pom-update",
        method: Some(Method::POST),
        scope: RouteScope::AdminOnly,
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
        prefix: "/api/ui/network/firewall",
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
    // Catching up with the trust chain is a node acting on itself.
    RouteRule {
        prefix: "/api/ui/network/trust-sync",
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
    // Qual projetor e qual MTP pertencem a cada modelo: lido por quem vê o
    // catálogo, gravado só por administrador (e só no principal).
    RouteRule {
        prefix: "/api/ui/model-sidecar-links",
        method: Some(Method::GET),
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/model-sidecar-links",
        method: Some(Method::POST),
        scope: RouteScope::AdminOnly,
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
    // The machine's own name: its operator's to choose, whatever its role.
    RouteRule {
        prefix: "/api/ui/network/node-name",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    // A relay option is a decision about this machine's own bandwidth: it
    // belongs to whoever runs the machine, whatever its role in the network.
    RouteRule {
        prefix: "/api/ui/network/relay",
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
    // Joining replaces the network this machine is in; like leaving, it is
    // the operator's own machine and no profile needs a licence for it.
    RouteRule {
        prefix: "/api/ui/network/join",
        method: Some(Method::POST),
        scope: RouteScope::InferNodeAdmin,
    },
    // Who gets into the network is an administrator's decision.
    RouteRule {
        prefix: "/api/ui/network/admissions",
        method: None,
        scope: RouteScope::AdminOnly,
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
        prefix: "/api/ui/network/auto-discovery",
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
        scope: RouteScope::DeployAdmin,
    },
    RouteRule {
        prefix: "/api/ui/deployments",
        method: Some(Method::GET),
        scope: RouteScope::NodeAdmin,
    },
    RouteRule {
        prefix: "/api/ui/deployments",
        method: None,
        scope: RouteScope::DeployAdmin,
    },
    RouteRule {
        prefix: "/api/ui/apply",
        method: None,
        scope: RouteScope::DeployAdmin,
    },
    RouteRule {
        prefix: "/api/ui/launch",
        method: None,
        scope: RouteScope::DeployAdmin,
    },
    RouteRule {
        prefix: "/api/ui/undeploy",
        method: None,
        scope: RouteScope::DeployAdmin,
    },
    RouteRule {
        prefix: "/api/ui/stop-inference",
        method: None,
        scope: RouteScope::DeployAdmin,
    },
    RouteRule {
        prefix: "/api/ui/unload",
        method: None,
        scope: RouteScope::DeployAdmin,
    },
    // Emptying a machine's KV cache deletes what its conversations saved.
    RouteRule {
        prefix: "/api/ui/kv-cache",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    RouteRule {
        prefix: "/api/ui/reconfigure",
        method: None,
        scope: RouteScope::DeployAdmin,
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

/// Routes an inference node refuses whoever asks, an admin included: they
/// are how a machine comes to hold and serve a model - receiving files,
/// slicing, loading a slice, being a deployment target. The role is what the
/// network admitted the machine as, and no request changes that.
const INFER_NODE_REFUSES: &[&str] = &[
    "/api/ui/apply",
    "/api/ui/launch",
    "/api/ui/deploy",
    "/api/ui/reconfigure",
    "/api/ui/slice",
    "/api/ui/slice-ranges",
    "/api/transfer",
];

pub(crate) fn infer_node_refuses(path: &str) -> bool {
    INFER_NODE_REFUSES
        .iter()
        .any(|prefix| is_under(path, prefix))
}

pub(crate) const fn profile_can_access(
    profile: crate::profile::NodeProfile,
    scope: RouteScope,
) -> bool {
    match scope {
        RouteScope::AdminOnly | RouteScope::DeployAdmin => {
            matches!(profile, crate::profile::NodeProfile::Admin)
        }
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
        assert_eq!(scope_of("/api/ui/plugins"), RouteScope::InferNodeAdmin);
        assert_eq!(
            scope_of("/api/ui/plugins/github-repositories"),
            RouteScope::InferNodeAdmin
        );
        assert_eq!(
            scope_of("/api/ui/plugins/dev-sources"),
            RouteScope::InferNodeAdmin
        );
        assert_eq!(
            scope_of("/api/ui/pom-update/status"),
            RouteScope::CommonOrInternal
        );
        // Plugins são locais de cada nó: ativar, cadastrar repositório,
        // instalar e atualizar é permitido a todo perfil.
        for path in [
            "/api/ui/plugins/activate",
            "/api/ui/plugins/checkout",
            "/api/ui/plugins/logs/refresh",
            "/api/ui/plugins/logs/deactivate",
            "/api/ui/plugins/logs/update/check",
            "/api/ui/plugins/logs/update/apply",
            "/api/ui/plugins/github-repositories",
            "/api/ui/plugins/github-repositories/repository-id/refresh",
            "/api/ui/plugins/github-repositories/repository-id/install",
            "/api/ui/plugins/logs/preferences",
        ] {
            let scope = route_scope(&Method::POST, path);
            assert_eq!(scope, RouteScope::InferNodeAdmin, "rota={path}");
            assert!(profile_can_access(crate::profile::NodeProfile::Node, scope));
            assert!(profile_can_access(crate::profile::NodeProfile::Infer, scope));
        }
        for path in [
            "/api/ui/pom-update/check",
            "/api/ui/pom-update/apply",
        ] {
            assert_eq!(
                route_scope(&Method::POST, path),
                RouteScope::AdminOnly,
                "rota={path}"
            );
        }
        assert_eq!(
            route_scope(
                &Method::DELETE,
                "/api/ui/plugins/github-repositories/repository-id"
            ),
            RouteScope::InferNodeAdmin
        );
        // O proxy de interfaces de plugin atende qualquer método e herda a
        // regra por prefixo: aberto a todo perfil em todos os métodos.
        for method in [
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ] {
            assert_eq!(
                route_scope(
                    &method,
                    "/api/ui/plugins/deepseek_harness/proxy/api/remote.mux"
                ),
                RouteScope::InferNodeAdmin,
                "método={method}"
            );
        }
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
            scope_of("/api/ui/network/relay"),
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
        assert_eq!(
            scope_of("/api/ui/network/firewall"),
            RouteScope::InferNodeAdmin
        );
        assert_eq!(
            scope_of("/api/ui/network/auto-discovery"),
            RouteScope::AdminOnly
        );
        assert_eq!(scope_of("/api/ui/deployments"), RouteScope::NodeAdmin);
        assert_eq!(
            route_scope(&Method::POST, "/api/ui/deployments"),
            RouteScope::DeployAdmin
        );
        // Deploying is the one admin scope a secondary admin shares; what
        // governs the network stays with the principal.
        for path in ["/api/ui/deploy", "/api/ui/undeploy", "/api/ui/apply", "/api/ui/unload"] {
            assert_eq!(scope_of(path), RouteScope::DeployAdmin, "{path}");
        }
        for path in ["/api/ui/network/admissions", "/admin/authority/resign", "/api/ui/release"] {
            assert_eq!(scope_of(path), RouteScope::AdminOnly, "{path}");
        }
        assert!(profile_can_access(
            crate::profile::NodeProfile::Admin,
            RouteScope::DeployAdmin
        ));
        assert!(!profile_can_access(
            crate::profile::NodeProfile::Node,
            RouteScope::DeployAdmin
        ));
        // Changing a role in place is not a route any more: with no rule,
        // the old paths fall to the closed default.
        assert_eq!(
            scope_of("/api/ui/network/request-node"),
            RouteScope::AdminOnly
        );
        assert_eq!(
            scope_of("/api/ui/network/relinquish-node"),
            RouteScope::AdminOnly
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
    fn development_plugin_routes_are_open_to_every_profile() {
        use crate::profile::NodeProfile::{Admin, Infer, Node};

        // A local build installs on this machine only, like any plugin;
        // `POM_DESENV` is what turns it on (checked by the handlers).

        for (method, path) in [
            (Method::GET, "/api/ui/plugins/dev-sources"),
            (
                Method::POST,
                "/api/ui/plugins/dev-sources/pom-plugin-base/rebuild-install",
            ),
        ] {
            let scope = route_scope(&method, path);
            assert_eq!(scope, RouteScope::InferNodeAdmin, "{method} {path}");
            assert!(profile_can_access(Admin, scope));
            assert!(profile_can_access(Node, scope));
            assert!(profile_can_access(Infer, scope));
        }
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
    fn an_inference_node_refuses_everything_that_would_make_it_serve_a_model() {
        for path in [
            "/api/ui/apply",
            "/api/ui/launch",
            "/api/ui/deploy",
            "/api/ui/deploy/node",
            "/api/ui/slice",
            "/api/transfer/offer",
            "/api/transfer/download",
        ] {
            assert!(infer_node_refuses(path), "{path} should be refused");
        }
        // Running inference, reading its own state and leaving stay open.
        for path in [
            "/v1/chat/completions",
            "/api/generate",
            "/api/ui/node",
            "/api/ui/unload",
            "/api/ui/leave",
            "/api/ui/applying",
            "/api/transferred",
        ] {
            assert!(!infer_node_refuses(path), "{path} should stay open");
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

        assert!(profile_can_access(Infer, scope_of("/api/ui/infer/heads")));
        assert!(!profile_can_access(Infer, scope_of("/api/ui/models")));
    }
}
