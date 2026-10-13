use super::*;
use axum::http::Method;

mod admin;
mod common;
mod infer;
mod node;

/// Profile scope attached to a gateway route. `CommonOrInternal` is also
/// the explicit band for protocols whose caller is another node and proves the
/// shared internal credential. Setup and the separate inference listener
/// are represented on purpose so they do not vanish from the inventory by
/// omission.
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
    /// `None` holds for every method. An explicit method only matches it, and
    /// exists because an administrative route may have a profile read that is
    /// wider than its write: `GET /admin/nodes` is the snapshot of the network
    /// that the Network screen needs, while blocking, removing and promoting a
    /// machine remain an administrator's act under the same prefix.
    method: Option<Method>,
    scope: RouteScope,
}

/// A readable table for the backend's profile boundary. The route files
/// above organize the UI router for human reading; this table is what
/// the gateway middleware applies, including routes registered by neighbouring
/// backend modules.
const ROUTE_RULES: &[RouteRule] = &[
    // Out of this task on purpose. Setup has its own router before a
    // profile exists, and the inference listener has its own port and middleware.
    // On port 8181, the routes outside this map are:
    // /v1/prefill, /v1/step, /v1/step-stream, /v1/link, /v1/session/*,
    // /v1/tp/reduce e /health.
    RouteRule {
        prefix: "/api/setup/",
        method: None,
        scope: RouteScope::OutOfScope,
    },
    // Public/common gateway routes and policy probes between nodes.
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
    // Inference-compatible APIs. The callbacks above win by
    // specificity.
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
    // Common UI routes. The handlers still reduce the cluster data for
    // infer.
    RouteRule {
        prefix: "/api/ui/node",
        method: None,
        scope: RouteScope::CommonOrInternal,
    },
    // Plugins are local to each node (see `crate::plugins`): activating a
    // license, registering a GitHub repository, installing, updating and using a
    // plugin's interface is an act of any profile on its own machine.
    // The rule matches by segment prefix and holds for every method, including
    // under `/api/ui/plugins/:code/...` and in the interface proxy.
    RouteRule {
        prefix: "/api/ui/plugins",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    // Update of the `pom` binary itself - a boundary separate from plugins on
    // purpose, see `crate::pom_core_update`.
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
        prefix: "/api/ui/network/geo/",
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
    // Anchoring the trust root is the work that precedes any authority existing:
    // an unanchored `profile: admin` does not pass `AdminOnly`, and whoever receives
    // the anchor is usually `node` or `infer`. The real locks are of origin and
    // of state, in the handler; see `webui/trust.rs`.
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
    // Local operations of the node and the model. The announced address is the description
    // of this very machine, so it is also allowed to the `infer` profile.
    RouteRule {
        prefix: "/api/ui/models",
        method: None,
        scope: RouteScope::NodeAdmin,
    },
    // Which projector and which MTP belong to each model: read by whoever sees the
    // catalog, written only by an administrator (and only on the principal).
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
    // The operator's own list of networks: this one and the imported ones.
    RouteRule {
        prefix: "/api/ui/networks",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    // The first run guide belongs to whoever sits at this machine.
    RouteRule {
        prefix: "/api/ui/guide",
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
    // Administrative and lifecycle operations.
    RouteRule {
        prefix: "/admin/",
        method: None,
        scope: RouteScope::AdminOnly,
    },
    // API keys are the credential of whoever serves inference, and every profile
    // serves: `/v1` is `InferNodeAdmin`. An `infer` swaps its own key through the
    // API KEYS screen, which is the destination of its footer. Denying here left the
    // two profiles with no way to issue the token the node itself demands.
    RouteRule {
        prefix: "/admin/api-keys",
        method: None,
        scope: RouteScope::InferNodeAdmin,
    },
    // Reading who is on the network is what the Network screen shows; blocking, unblocking,
    // removing and promoting a machine are POST/DELETE and remain under
    // `/admin/`, that is, `AdminOnly`.
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
    // Leaving the network is abdicating one's own post, not a privilege: any
    // profile shuts down its own machine at any time, and coming back is the other
    // half of the same switch. Telling **another** machine to leave remains
    // an administrator's act, and that lock belongs to the request's target, not to the
    // path: it lives in `webui::lifecycle::leave_network`.
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

/// The scope of this request. The most specific rule wins: first the
/// longest prefix, and on a tie the one that names the method - so a read
/// can be wider than the write on the same path without the rest of the
/// prefix ceasing to be administrative.
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
                // New control routes fail closed until they receive a
                // profile in this table.
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

/// Builds the control surface without making the file organization the
/// only security barrier. The profile policy stays in this module and the
/// gateway middleware applies it afterwards, on the shared HTTP path.
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

    /// Read shortcut: almost every rule in the table is indifferent to the method.
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
        // Plugins are local to each node: activating, registering a repository,
        // installing and updating is allowed to every profile.
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
            assert!(profile_can_access(
                crate::profile::NodeProfile::Infer,
                scope
            ));
        }
        for path in ["/api/ui/pom-update/check", "/api/ui/pom-update/apply"] {
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
        // The plugin interface proxy serves any method and inherits the
        // prefix rule: open to every profile on every method.
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
                "method={method}"
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
            scope_of("/api/ui/network/geo/nearest"),
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
        for path in [
            "/api/ui/deploy",
            "/api/ui/undeploy",
            "/api/ui/apply",
            "/api/ui/unload",
        ] {
            assert_eq!(scope_of(path), RouteScope::DeployAdmin, "{path}");
        }
        for path in [
            "/api/ui/network/admissions",
            "/admin/authority/resign",
            "/api/ui/release",
        ] {
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
        // `POM_DEV_MODE` is what turns it on (checked by the handlers).

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

    /// The Network screen of the `node` profile reads the network snapshot; changing what it
    /// shows remains an administrator's act, and the two things live under
    /// the same prefix.
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

    /// The API key is the credential of whoever serves `/v1`, and every profile serves.
    #[test]
    fn every_profile_can_mint_the_key_its_own_inference_demands() {
        for method in [Method::GET, Method::POST, Method::DELETE] {
            assert_eq!(
                route_scope(&method, "/admin/api-keys"),
                RouteScope::InferNodeAdmin,
                "method={method}"
            );
        }
        assert_eq!(
            route_scope(&Method::DELETE, "/admin/api-keys/abc"),
            RouteScope::InferNodeAdmin
        );
    }

    /// Leaving the network is abdicating: no profile needs a license for it.
    #[test]
    fn leaving_and_rejoining_belong_to_every_profile() {
        use crate::profile::NodeProfile::{Admin, Infer, Node};

        for path in ["/api/ui/leave", "/api/ui/rejoin"] {
            let scope = route_scope(&Method::POST, path);
            assert_eq!(scope, RouteScope::InferNodeAdmin, "rota={path}");
            for profile in [Infer, Node, Admin] {
                assert!(profile_can_access(profile, scope), "{profile} on {path}");
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
