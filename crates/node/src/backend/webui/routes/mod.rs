use super::*;
use axum::http::Method;

mod admin;
mod common;
mod visitor;
mod worker;

/// Profile scope attached to a gateway route. `HandlerAuth` is also
/// the explicit band for protocols whose caller is another node and proves the
/// shared internal credential. Setup and the separate inference listener
/// are represented on purpose so they do not vanish from the inventory by
/// omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteScope {
    /// The network's principal only. A secondary admin does not reach these:
    /// everything that governs the network (members, admissions, settings,
    /// keys, the trust chain) comes from the principal.
    PrincipalOnly,
    /// Any admin of the trust chain, principal or secondary: putting models
    /// in and out of service. It is the one thing a secondary admin may do
    /// that a worker may not.
    AnyAdmin,
    /// A machine that serves models: an admin, or a worker the principal
    /// admitted as one. Never a visitor (catalog, planning, transfers).
    ServingMember,
    /// Every member, visitors included: inference, and leaving the network.
    AnyMember,
    /// Every profile, but only from this machine's own interface: the
    /// operator acting on their own node (API keys, plugins, the machine's
    /// name, relay and address, its networks, its logs). Another node never
    /// reaches these, whoever signed the request - every member holds the
    /// connection key, visitors included.
    LocalOnly,
    /// The handler authenticates the caller itself: public routes, policy
    /// probes and the sync protocols between nodes.
    HandlerAuth,
    /// Served by another router: the setup wizard, or the inference listener
    /// on its own port.
    OtherRouter,
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
        scope: RouteScope::OtherRouter,
    },
    // Public/common gateway routes and policy probes between nodes.
    RouteRule {
        prefix: "/api/access",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    // A plugin's sign-in flow (`crate::user_session::auth_http`): public by
    // design, it is how a person without a session gets one.
    RouteRule {
        prefix: "/auth/",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/public/",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/peers",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/health",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/handshake",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/cluster/quic",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/cluster/logs",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/cluster/network-chat",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/cluster/member-left",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/cluster/network-settings",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/internal/",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    // A plugin calling back into its own node, with the secret its host got
    // in `host.configure` (`crate::plugin_host_api`).
    RouteRule {
        prefix: "/api/plugin-host/",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/v1/relay-result",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/v1/remote-stream-result",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    // Inference-compatible APIs. The callbacks above win by
    // specificity.
    RouteRule {
        prefix: "/api/generate",
        method: None,
        scope: RouteScope::AnyMember,
    },
    RouteRule {
        prefix: "/generate",
        method: None,
        scope: RouteScope::AnyMember,
    },
    RouteRule {
        prefix: "/v1/",
        method: None,
        scope: RouteScope::AnyMember,
    },
    // Common UI routes. The handlers still reduce the cluster data for
    // infer.
    RouteRule {
        prefix: "/api/ui/node",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    // Plugins are local to each node (see `crate::plugins`): activating a
    // license, registering a GitHub repository, installing, updating and using a
    // plugin's interface is an act of any profile on its own machine.
    // The rule matches by segment prefix and holds for every method, including
    // under `/api/ui/plugins/:code/...` and in the interface proxy.
    RouteRule {
        prefix: "/api/ui/plugins",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // The public plugin store, read by the node so the browser never leaves
    // its own origin (`crate::plugins::plugin_catalog`).
    RouteRule {
        prefix: "/api/ui/plugin-catalog",
        method: Some(Method::GET),
        scope: RouteScope::LocalOnly,
    },
    // Update of the `pom` binary itself - a boundary separate from plugins on
    // purpose, see `crate::pom_core_update`.
    RouteRule {
        prefix: "/api/ui/pom-update",
        method: Some(Method::GET),
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/ui/pom-update",
        method: Some(Method::POST),
        scope: RouteScope::PrincipalOnly,
    },
    RouteRule {
        prefix: "/api/ui/cluster",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/ui/peer/",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/ui/network/rtt",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/ui/network/info",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/firewall",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // The local CA of this machine (`webui/https.rs`): every profile reads it
    // and downloads its certificate; installing it also demands loopback.
    RouteRule {
        prefix: "/api/ui/tls",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/geo/",
        method: Some(Method::GET),
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/network-chat",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    // The lobby is this machine knocking on another network's door: asking
    // to be admitted, chatting there, forgetting a request. Its operator's
    // act, never another member's.
    RouteRule {
        prefix: "/api/ui/network-chat/lobby",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/chat/",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/ui/chat/history",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // The session's own identity, sign-out and a signed-in person's own API
    // keys: answered by `crate::user_session` inside the gate, local to this
    // node like every session.
    RouteRule {
        prefix: "/api/ui/me",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // A person's seat on a demonstration node (`crate::demo_slots`).
    RouteRule {
        prefix: "/api/ui/demo/session",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/logout",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/my-keys",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/chat-key",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // This machine's own way into its network: its operator's, nobody else's.
    RouteRule {
        prefix: "/api/ui/connection",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // Charging visitors: the accounts and grants are the principal's; a
    // machine's own balance on the network it visits is its operator's.
    RouteRule {
        prefix: "/api/ui/payments",
        method: None,
        scope: RouteScope::PrincipalOnly,
    },
    RouteRule {
        prefix: "/api/ui/payments/balance",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/payments/request",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // A node's own token budget: reading it, spending a session reset,
    // asking the admins for more. A machine's own operator route.
    RouteRule {
        prefix: "/api/ui/quota",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // Catching up with the trust chain is a node acting on itself.
    RouteRule {
        prefix: "/api/ui/network/trust-sync",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/admin-authority",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    // Anchoring the trust root is the work that precedes any authority existing:
    // an unanchored `profile: admin` does not pass `PrincipalOnly`, and whoever receives
    // the anchor is usually `node` or `infer`. The real locks are of origin and
    // of state, in the handler; see `webui/trust.rs`.
    RouteRule {
        prefix: "/api/ui/network/trust-anchor",
        method: None,
        scope: RouteScope::HandlerAuth,
    },
    RouteRule {
        prefix: "/api/ui/infer/heads",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // Local operations of the node and the model. The announced address is the description
    // of this very machine, so it is also allowed to the `infer` profile.
    RouteRule {
        prefix: "/api/ui/models",
        method: None,
        scope: RouteScope::ServingMember,
    },
    // Which projector and which MTP belong to each model: read by whoever sees the
    // catalog, written only by an administrator (and only on the principal).
    RouteRule {
        prefix: "/api/ui/model-sidecar-links",
        method: Some(Method::GET),
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/model-sidecar-links",
        method: Some(Method::POST),
        scope: RouteScope::PrincipalOnly,
    },
    RouteRule {
        prefix: "/api/ui/paths",
        method: None,
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/thermal",
        method: None,
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/cache-types",
        method: None,
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/network/advertise-addr",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // The operator's own list of networks: this one and the imported ones.
    RouteRule {
        prefix: "/api/ui/networks",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // The first run guide belongs to whoever sits at this machine.
    RouteRule {
        prefix: "/api/ui/guide",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // The machine's own name: its operator's to choose, whatever its role.
    RouteRule {
        prefix: "/api/ui/network/node-name",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // A relay option is a decision about this machine's own bandwidth: it
    // belongs to whoever runs the machine, whatever its role in the network.
    RouteRule {
        prefix: "/api/ui/network/relay",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/settings",
        method: Some(Method::POST),
        scope: RouteScope::PrincipalOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/trust-share",
        method: Some(Method::GET),
        scope: RouteScope::PrincipalOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/leave",
        method: Some(Method::POST),
        scope: RouteScope::LocalOnly,
    },
    // Joining replaces the network this machine is in; like leaving, it is
    // the operator's own machine and no profile needs a licence for it.
    RouteRule {
        prefix: "/api/ui/network/join",
        method: Some(Method::POST),
        scope: RouteScope::LocalOnly,
    },
    // Who gets into the network is any admin's decision: a secondary's goes
    // to the principal, which keeps the list.
    RouteRule {
        prefix: "/api/ui/network/admissions",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/plan",
        method: None,
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/model-identity",
        method: None,
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/model-metadata",
        method: None,
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/model-layer-options",
        method: None,
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/slice-ranges",
        method: None,
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/slice",
        method: None,
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/tp/check",
        method: None,
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/jobs",
        method: None,
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/model-provider/",
        method: None,
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/transfer/",
        method: None,
        scope: RouteScope::ServingMember,
    },
    // Administrative and lifecycle operations.
    RouteRule {
        prefix: "/admin/",
        method: None,
        scope: RouteScope::PrincipalOnly,
    },
    // API keys are the credential of whoever serves inference, and every profile
    // serves: `/v1` is `AnyMember`. An `infer` swaps its own key through the
    // API KEYS screen, which is the destination of its footer. Denying here left the
    // two profiles with no way to issue the token the node itself demands.
    RouteRule {
        prefix: "/admin/api-keys",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    // Reading who is on the network is what the Network screen shows; blocking, unblocking,
    // removing and promoting a machine are POST/DELETE and remain under
    // `/admin/`, that is, `PrincipalOnly`.
    RouteRule {
        prefix: "/admin/nodes",
        method: Some(Method::GET),
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/access",
        method: None,
        scope: RouteScope::PrincipalOnly,
    },
    // The panel password is this machine's own credential, on any profile,
    // and no other node changes it.
    RouteRule {
        prefix: "/api/ui/access/password",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/tls",
        method: None,
        scope: RouteScope::PrincipalOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/quic",
        method: None,
        scope: RouteScope::PrincipalOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/relay-policy",
        method: None,
        scope: RouteScope::PrincipalOnly,
    },
    RouteRule {
        prefix: "/api/ui/network/logs",
        method: None,
        // Every admin decides it; a secondary's request is passed on to the
        // principal, which owns the policy the nodes adopt.
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/network/auto-discovery",
        method: None,
        scope: RouteScope::PrincipalOnly,
    },
    RouteRule {
        prefix: "/api/ui/cluster-logs",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/deploy",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/deployments",
        method: Some(Method::GET),
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/deployments",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/apply",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/components",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/image-deploy",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/image-undeploy",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    // Reading what image models the network serves is the Dashboard's, on a
    // worker too (it asks each peer, so no local image plugin is needed);
    // changing them stays an admin's.
    RouteRule {
        prefix: "/api/ui/image-deployments",
        method: Some(Method::GET),
        scope: RouteScope::ServingMember,
    },
    RouteRule {
        prefix: "/api/ui/image-deployments",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/launch",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/undeploy",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/stop-inference",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/unload",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    // Emptying a machine's KV cache deletes what its conversations saved.
    RouteRule {
        prefix: "/api/ui/kv-cache",
        method: None,
        scope: RouteScope::PrincipalOnly,
    },
    RouteRule {
        prefix: "/api/ui/reconfigure",
        method: None,
        scope: RouteScope::AnyAdmin,
    },
    RouteRule {
        prefix: "/api/ui/release",
        method: None,
        scope: RouteScope::PrincipalOnly,
    },
    // Leaving the network is abdicating one's own post, not a privilege: any
    // profile shuts down its own machine at any time, and coming back is the other
    // half of the same switch. Telling **another** machine to leave remains
    // an administrator's act, and that lock belongs to the request's target, not to the
    // path: it lives in `webui::lifecycle::leave_network`.
    RouteRule {
        prefix: "/api/ui/leave",
        method: None,
        scope: RouteScope::AnyMember,
    },
    RouteRule {
        prefix: "/api/ui/rejoin",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/requests",
        method: None,
        scope: RouteScope::LocalOnly,
    },
    RouteRule {
        prefix: "/api/ui/dev/",
        method: None,
        scope: RouteScope::PrincipalOnly,
    },
    RouteRule {
        prefix: "/admin/authority",
        method: None,
        scope: RouteScope::PrincipalOnly,
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
                RouteScope::OtherRouter
            } else if path.starts_with("/api/") || path.starts_with("/admin/") {
                // New control routes fail closed until they receive a
                // profile in this table.
                RouteScope::PrincipalOnly
            } else if path.starts_with("/v1/") {
                RouteScope::AnyMember
            } else {
                RouteScope::HandlerAuth
            }
        })
}

/// The scope a route takes when another node asks, where it is narrower
/// than for this machine's own operator. A worker downloads models and walks
/// its folders for itself (`ServingMember`), but making another machine
/// download or rescan is an admin's act: a worker signing that request for a
/// neighbour is refused. Reading a machine's downloads stays open to the same
/// callers as before.
pub(crate) fn node_to_node_scope(method: &Method, path: &str, scope: RouteScope) -> RouteScope {
    let admin_only = (method == Method::POST && is_under(path, "/api/ui/model-provider/downloads"))
        || is_under(path, "/api/ui/models/rescan");
    if admin_only && scope == RouteScope::ServingMember {
        RouteScope::AnyAdmin
    } else {
        scope
    }
}

/// Routes an inference node refuses whoever asks, an admin included: they
/// are how a machine comes to hold and serve a model - receiving files,
/// slicing, loading a slice, being a deployment target. The role is what the
/// network admitted the machine as, and no request changes that.
const VISITOR_REFUSES: &[&str] = &[
    "/api/ui/apply",
    "/api/ui/components",
    "/api/ui/launch",
    "/api/ui/deploy",
    "/api/ui/reconfigure",
    "/api/ui/slice",
    "/api/ui/slice-ranges",
    "/api/transfer",
];

pub(crate) fn visitor_refuses(path: &str) -> bool {
    VISITOR_REFUSES
        .iter()
        .any(|prefix| is_under(path, prefix))
}

pub(crate) const fn profile_can_access(
    profile: crate::profile::NodeProfile,
    scope: RouteScope,
) -> bool {
    match scope {
        RouteScope::PrincipalOnly | RouteScope::AnyAdmin => {
            matches!(profile, crate::profile::NodeProfile::Admin)
        }
        RouteScope::ServingMember => matches!(
            profile,
            crate::profile::NodeProfile::Worker | crate::profile::NodeProfile::Admin
        ),
        RouteScope::AnyMember
        | RouteScope::LocalOnly
        | RouteScope::HandlerAuth => true,
        RouteScope::OtherRouter => false,
    }
}

/// Builds the control surface without making the file organization the
/// only security barrier. The profile policy stays in this module and the
/// gateway middleware applies it afterwards, on the shared HTTP path.
pub fn routes() -> Router<GatewayState> {
    Router::new()
        .merge(common::routes())
        .merge(visitor::routes())
        .merge(worker::routes())
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
    fn another_node_needs_an_admin_to_download_or_rescan_here() {
        let post = |path: &str| node_to_node_scope(&Method::POST, path, route_scope(&Method::POST, path));
        assert_eq!(post("/api/ui/model-provider/downloads"), RouteScope::AnyAdmin);
        assert_eq!(post("/api/ui/model-provider/downloads/ab/cancel"), RouteScope::AnyAdmin);
        assert_eq!(post("/api/ui/models/rescan"), RouteScope::AnyAdmin);
        assert_eq!(
            node_to_node_scope(&Method::GET, "/api/ui/model-provider/downloads", scope_of("/api/ui/model-provider/downloads")),
            RouteScope::ServingMember
        );
        assert_eq!(post("/api/ui/models/delete"), RouteScope::ServingMember);
        // The local operator's scope is unchanged.
        assert_eq!(route_scope(&Method::POST, "/api/ui/models/rescan"), RouteScope::ServingMember);
    }

    #[test]
    fn route_policy_exposes_the_four_declared_scopes() {
        assert_eq!(
            route_scope(&Method::DELETE, "/admin/nodes/abc"),
            RouteScope::PrincipalOnly
        );
        assert_eq!(scope_of("/api/ui/requests/stats"), RouteScope::LocalOnly);
        // The panel password is the machine's own; the exposure stays the principal's.
        assert_eq!(scope_of("/api/ui/access/password"), RouteScope::LocalOnly);
        assert_eq!(scope_of("/api/ui/access"), RouteScope::PrincipalOnly);
        assert_eq!(scope_of("/api/ui/plugins"), RouteScope::LocalOnly);
        assert_eq!(scope_of("/api/ui/plugin-catalog"), RouteScope::LocalOnly);
        assert_eq!(
            scope_of("/api/ui/plugin-catalog/logs"),
            RouteScope::LocalOnly
        );
        assert_eq!(
            scope_of("/api/ui/plugins/github-repositories"),
            RouteScope::LocalOnly
        );
        assert_eq!(
            scope_of("/api/ui/plugins/dev-sources"),
            RouteScope::LocalOnly
        );
        assert_eq!(
            scope_of("/api/ui/pom-update/status"),
            RouteScope::HandlerAuth
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
            assert_eq!(scope, RouteScope::LocalOnly, "rota={path}");
            assert!(profile_can_access(crate::profile::NodeProfile::Worker, scope));
            assert!(profile_can_access(
                crate::profile::NodeProfile::Visitor,
                scope
            ));
        }
        for path in ["/api/ui/pom-update/check", "/api/ui/pom-update/apply"] {
            assert_eq!(
                route_scope(&Method::POST, path),
                RouteScope::PrincipalOnly,
                "rota={path}"
            );
        }
        assert_eq!(
            route_scope(
                &Method::DELETE,
                "/api/ui/plugins/github-repositories/repository-id"
            ),
            RouteScope::LocalOnly
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
                RouteScope::LocalOnly,
                "method={method}"
            );
        }
        assert_eq!(scope_of("/api/ui/models"), RouteScope::ServingMember);
        assert_eq!(
            scope_of("/api/ui/model-provider/status"),
            RouteScope::ServingMember
        );
        assert_eq!(
            scope_of("/api/ui/network/advertise-addr"),
            RouteScope::LocalOnly
        );
        assert_eq!(
            scope_of("/api/ui/network/relay"),
            RouteScope::LocalOnly
        );
        assert_eq!(
            scope_of("/api/ui/network/admin-authority"),
            RouteScope::HandlerAuth
        );
        assert_eq!(
            scope_of("/api/ui/network/trust-anchor"),
            RouteScope::HandlerAuth
        );
        assert_eq!(
            scope_of("/api/ui/network/geo/nearest"),
            RouteScope::LocalOnly
        );
        assert_eq!(
            scope_of("/api/ui/network/firewall"),
            RouteScope::LocalOnly
        );
        assert_eq!(
            scope_of("/api/ui/network/auto-discovery"),
            RouteScope::PrincipalOnly
        );
        assert_eq!(scope_of("/api/ui/deployments"), RouteScope::ServingMember);
        assert_eq!(scope_of("/api/ui/image-deployments"), RouteScope::ServingMember);
        assert_eq!(
            route_scope(&Method::POST, "/api/ui/deployments"),
            RouteScope::AnyAdmin
        );
        // Deploying is the one admin scope a secondary admin shares; what
        // governs the network stays with the principal.
        for path in [
            "/api/ui/deploy",
            "/api/ui/undeploy",
            "/api/ui/apply",
            "/api/ui/unload",
            "/api/ui/network/admissions",
        ] {
            assert_eq!(scope_of(path), RouteScope::AnyAdmin, "{path}");
        }
        for path in [
            "/admin/authority/resign",
            "/api/ui/release",
        ] {
            assert_eq!(scope_of(path), RouteScope::PrincipalOnly, "{path}");
        }
        assert!(profile_can_access(
            crate::profile::NodeProfile::Admin,
            RouteScope::AnyAdmin
        ));
        assert!(!profile_can_access(
            crate::profile::NodeProfile::Worker,
            RouteScope::AnyAdmin
        ));
        // Changing a role in place is not a route any more: with no rule,
        // the old paths fall to the closed default.
        assert_eq!(
            scope_of("/api/ui/network/request-node"),
            RouteScope::PrincipalOnly
        );
        assert_eq!(
            scope_of("/api/ui/network/relinquish-node"),
            RouteScope::PrincipalOnly
        );
        assert_eq!(scope_of("/api/ui/infer/heads"), RouteScope::LocalOnly);
        assert!(profile_can_access(
            crate::profile::NodeProfile::Admin,
            scope_of("/api/ui/infer/heads")
        ));
        assert_eq!(scope_of("/v1/chat/completions"), RouteScope::AnyMember);
        assert_eq!(scope_of("/api/ui/node"), RouteScope::HandlerAuth);
        assert_eq!(
            scope_of("/api/ui/chat/capabilities"),
            RouteScope::HandlerAuth
        );
        assert_eq!(
            scope_of("/api/ui/chat/history/abc"),
            RouteScope::LocalOnly
        );
        assert_eq!(scope_of("/api/ui/chat-key"), RouteScope::LocalOnly);
        assert_eq!(scope_of("/api/ui/connection"), RouteScope::LocalOnly);
        assert_eq!(scope_of("/api/ui/quota/reset"), RouteScope::LocalOnly);
        assert_eq!(
            scope_of("/api/internal/api-keys/validate"),
            RouteScope::HandlerAuth
        );
    }

    #[test]
    fn development_plugin_routes_are_open_to_every_profile() {
        use crate::profile::NodeProfile::{Admin, Visitor, Worker};

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
            assert_eq!(scope, RouteScope::LocalOnly, "{method} {path}");
            assert!(profile_can_access(Admin, scope));
            assert!(profile_can_access(Worker, scope));
            assert!(profile_can_access(Visitor, scope));
        }
    }

    #[test]
    fn route_policy_matches_segments_not_similar_prefixes() {
        assert_eq!(scope_of("/api/ui/models-old"), RouteScope::PrincipalOnly);
        assert_eq!(scope_of("/admin"), RouteScope::PrincipalOnly);
        assert_eq!(scope_of("/api/ui"), RouteScope::PrincipalOnly);
        assert_eq!(
            scope_of("/api/ui/requests/stats/today"),
            RouteScope::LocalOnly
        );
        assert_eq!(scope_of("/api/new-control"), RouteScope::PrincipalOnly);
        assert_eq!(
            scope_of("/api/ui/chat/telemetry"),
            RouteScope::HandlerAuth
        );
        assert_eq!(scope_of("/api/plugin-host/sign"), RouteScope::HandlerAuth);
        assert_eq!(scope_of("/api/plugin-hosts"), RouteScope::PrincipalOnly);
    }

    /// The Network screen of the `node` profile reads the network snapshot; changing what it
    /// shows remains an administrator's act, and the two things live under
    /// the same prefix.
    #[test]
    fn reading_the_roster_is_wider_than_administering_it() {
        assert_eq!(
            route_scope(&Method::GET, "/admin/nodes"),
            RouteScope::ServingMember
        );
        assert_eq!(
            route_scope(&Method::POST, "/admin/nodes/abc/block"),
            RouteScope::PrincipalOnly
        );
        assert_eq!(
            route_scope(&Method::POST, "/admin/nodes/abc/unblock"),
            RouteScope::PrincipalOnly
        );
        assert_eq!(
            route_scope(&Method::POST, "/admin/nodes/abc/promote-admin"),
            RouteScope::PrincipalOnly
        );
        assert_eq!(
            route_scope(&Method::DELETE, "/admin/nodes/abc"),
            RouteScope::PrincipalOnly
        );
        assert_eq!(
            route_scope(&Method::POST, "/admin/regenerate-key"),
            RouteScope::PrincipalOnly
        );
    }

    /// The API key is the credential of whoever serves `/v1`, and every profile serves.
    #[test]
    fn every_profile_can_mint_the_key_its_own_inference_demands() {
        for method in [Method::GET, Method::POST, Method::DELETE] {
            assert_eq!(
                route_scope(&method, "/admin/api-keys"),
                RouteScope::LocalOnly,
                "method={method}"
            );
        }
        assert_eq!(
            route_scope(&Method::DELETE, "/admin/api-keys/abc"),
            RouteScope::LocalOnly
        );
    }

    /// Leaving the network is abdicating: no profile needs a license for it.
    #[test]
    fn leaving_and_rejoining_belong_to_every_profile() {
        use crate::profile::NodeProfile::{Admin, Visitor, Worker};

        // Leaving stays reachable by the principal's forwarded, signed leave;
        // rejoining is only ever this machine's own act.
        for (path, expected) in [
            ("/api/ui/leave", RouteScope::AnyMember),
            ("/api/ui/rejoin", RouteScope::LocalOnly),
        ] {
            let scope = route_scope(&Method::POST, path);
            assert_eq!(scope, expected, "rota={path}");
            for profile in [Visitor, Worker, Admin] {
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
            assert!(visitor_refuses(path), "{path} should be refused");
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
            assert!(!visitor_refuses(path), "{path} should stay open");
        }
    }

    #[test]
    fn setup_is_explicitly_out_of_scope() {
        assert_eq!(scope_of("/api/setup/save"), RouteScope::OtherRouter);
    }

    #[test]
    fn every_profile_has_a_permitted_and_a_refused_scope() {
        use crate::profile::NodeProfile::{Admin, Visitor, Worker};

        assert!(profile_can_access(Admin, scope_of("/admin/api-keys")));
        assert!(!profile_can_access(Admin, scope_of("/api/setup/save")));

        assert!(profile_can_access(Worker, scope_of("/api/ui/models")));
        assert!(!profile_can_access(
            Worker,
            route_scope(&Method::POST, "/admin/regenerate-key")
        ));

        assert!(profile_can_access(Visitor, scope_of("/api/ui/infer/heads")));
        assert!(!profile_can_access(Visitor, scope_of("/api/ui/models")));
    }
}
