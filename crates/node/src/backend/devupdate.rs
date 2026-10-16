//! Update and restart a node through the interface, in development.
//!
//! This is remote code execution: whoever holds the connection key gains the
//! ability to run a build and swap the machine's binary. That is why the gate
//! is an environment variable, set per process by the owner of the machine,
//! and not a config field - the config is rewritten by the interface itself,
//! which would make the gate switchable over the network.

/// Name of the variable that opens the feature (`POM_DEV_UPDATE`), derived from the brand.
pub fn env_gate() -> String {
    crate::brand::env_var("DEV_UPDATE")
}

/// Variable that says which features this machine compiles with.
pub fn env_features() -> String {
    crate::brand::env_var("BUILD_FEATURES")
}

/// Does this node accept update requests?
pub fn enabled() -> bool {
    enabled_from(crate::brand::env("DEV_UPDATE").ok().as_deref())
}

fn enabled_from(v: Option<&str>) -> bool {
    v == Some("1")
}

use crate::gateway::GatewayState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::json;

/// The gate's refusal, or `None` when it is open.
fn gate_rejection() -> Option<(StatusCode, String)> {
    if enabled() {
        return None;
    }
    Some((
        StatusCode::FORBIDDEN,
        format!(
            "remote update is off on this node. Start the process with {}=1 to enable it.",
            env_gate()
        ),
    ))
}

fn refuse(r: (StatusCode, String)) -> Response {
    (r.0, Json(json!({ "error": r.1 }))).into_response()
}

/// Slack between the HTTP response leaving and the process dying.
///
/// The body is a tiny JSON; on a LAN, writing it to the connection takes
/// microseconds. 300ms is generous slack for that even through a local
/// reverse proxy, without being so long that the operator feels a delay in
/// the restart. The two sides of the error are asymmetric: going past this
/// value only costs the operator a little more waiting; falling short of it
/// cuts the client's connection before the byte arrives, and the request
/// looks like it failed when it actually worked.
const RESTART_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

pub fn router() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/dev/restart", post(restart_endpoint))
        .route("/api/ui/dev/update", post(update_endpoint))
}

/// Cargo arguments, with this machine's features.
fn build_args(features: Option<&str>) -> Vec<String> {
    let mut v = vec!["build".to_string(), "--release".to_string()];
    match features.map(str::trim) {
        Some(f) if !f.is_empty() => {
            v.push("--features".into());
            v.push(f.to_string());
        }
        _ => {}
    }
    v
}

/// Gateway URLs of the peers running on the same machine as this node.
///
/// The input is each live peer's `(hostname, addr, gateway_port)`. A peer
/// without a gateway port stays out: there is no way to ask it anything. A
/// repeated hostname is not enough - two VMs called `ubuntu` are not the same
/// machine - so `same_host` must also approve the address; whoever decides the
/// network logic stays outside this function so it remains testable without
/// sockets.
///
/// The URL is built with `network::ip_host`, the same function behind
/// `PeerInfo::gateway_url()` - not a reimplementation with `rsplit_once`, which
/// breaks on IPv6 without brackets.
///
/// Returns `(node_id, url)`: the sibling is on the same machine, but the POST
/// goes out through the interface address and not through loopback, so in a
/// TLS cluster it needs that node's certificate pinned like any other call
/// between peers. See `serve::plain_allowed`.
fn sibling_urls(
    my_hostname: &str,
    peers: &[(&str, &str, Option<u16>, &str)],
    same_host: impl Fn(&str) -> bool,
) -> Vec<(String, String)> {
    peers
        .iter()
        .filter(|(h, _, _, _)| *h == my_hostname)
        .filter(|(_, addr, _, _)| {
            // `0.0.0.0`/`::` is the sentinel "address not yet rewritten"
            // (`Network::announce_addr`) of a peer that arrived through libp2p; without
            // this explicit discard the POST would go to this very node, which
            // would restart before the job was marked as `done`.
            let host = crate::network::ip_host(addr);
            host != "0.0.0.0" && host != "::" && host != "[::]" && same_host(addr)
        })
        .filter_map(|(_, addr, port, node_id)| {
            let port = (*port)?;
            Some((
                node_id.to_string(),
                crate::peer_client::peer_base_url(
                    &crate::network::ip_host(addr),
                    port,
                    crate::peer_client::tls_enabled(),
                ),
            ))
        })
        .collect()
}

/// Does this address belong to the machine where this process is running?
///
/// Connects a UDP socket (no traffic, it only fixes the route) to the peer's IP and
/// compares the resulting local address with the peer's: if the peer is this
/// very host, the OS routing returns the same IP. An address that does not
/// resolve to a valid IP is never the same machine.
fn same_machine(addr: &str) -> bool {
    let host = crate::network::ip_host(addr);
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => crate::transfer::local_ip_towards(ip) == host,
        Err(_) => false,
    }
}

async fn git(root: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("could not run git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Optional body of `POST /api/ui/dev/update`.
///
/// `Option<Json<T>>` instead of `Json<T>`: an empty body (the screen's button today
/// sends `{}`, but a `curl` without any `-d` sends a zero body) would make the
/// `Json<T>` extractor fail before the handler even runs, and axum's error
/// message is not the one this endpoint wants to show. With `Option`, an absent body
/// and a `{}` body both arrive as `node_id: None` - runs here.
#[derive(Debug, Default, serde::Deserialize)]
struct UpdateRequest {
    node_id: Option<String>,
}

/// Is this request to be forwarded to another peer, or executed here?
///
/// Pure function to stay testable without bringing up a network: without `node_id`, or with the
/// `node_id` of this very node, it runs here; any other id forwards.
fn should_forward(requested_node_id: Option<&str>, my_node_id: &str) -> bool {
    matches!(requested_node_id, Some(id) if id != my_node_id)
}

async fn update_endpoint(
    _auth: crate::auth::ConnectionAuth,
    State(state): State<GatewayState>,
    body: Option<Json<UpdateRequest>>,
) -> Response {
    let requested_node_id = body.and_then(|Json(b)| b.node_id);

    // The gate protects the machine that will run the build, and forwarding runs
    // nothing here. Checking it before the forwarding decision kept the panel of an
    // ordinary node from updating a peer that has the gate open - and the screen draws the
    // button from the **target's** `dev_update`, so the button appeared and this
    // node answered 403. Whoever forwards does not need the permission; whoever executes
    // does, and it is the destination that checks it.
    if let Some(target) =
        requested_node_id.filter(|id| should_forward(Some(id), &state.network.node_id))
    {
        let peers = state.network.alive_peers().await;
        let Some(base) = peers
            .iter()
            .find(|p| p.node_id == target)
            .and_then(|p| p.gateway_url())
        else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": format!(
                        "peer {target} is not announcing a gateway or is no longer seen on the network"
                    )
                })),
            )
                .into_response();
        };
        let url = format!("{base}/api/ui/dev/update");
        let client = match state.peer_http.for_peer(&target) {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": format!("no client for peer {target}: {e}") })),
                )
                    .into_response()
            }
        };
        let request = match state
            .authorize_control_request(
                client.post(&url).timeout(crate::webui::PEER_TIMEOUT),
                "POST",
                "/api/ui/dev/update",
                &[],
            )
            .await
        {
            Ok(request) => request,
            Err(error) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": error.to_string() })),
                )
                    .into_response()
            }
        };
        return match request.send().await {
            Ok(resp) => {
                let status = resp.status();
                let peer_body = resp.text().await.unwrap_or_else(|e| {
                    format!("{{\"error\":\"response from peer {target} without a readable body: {e}\"}}")
                });
                (status, peer_body).into_response()
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": format!("could not forward the update to {target}: {e}")
                })),
            )
                .into_response(),
        };
    }

    // From here down it is execution on this machine, and then the gate rules.
    if let Some(r) = gate_rejection() {
        return refuse(r);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let job = crate::webui::new_job(
        &id,
        "update",
        "Update this machine",
        "checking the repository",
    );
    crate::webui::jobs().write().await.insert(id.clone(), job);

    let job_id = id.clone();
    tokio::spawn(async move {
        let root = crate::config::project_root();

        async fn fail(job_id: &str, msg: String) {
            let mut jobs = crate::webui::jobs().write().await;
            if let Some(j) = jobs.get_mut(job_id) {
                j.state = "failed";
                j.message = msg;
                j.finished_at = Some(chrono::Utc::now().to_rfc3339());
            }
        }

        // ── Checks ───────────────────────────────────────────────────────────
        let branch = match git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
            Ok(b) => b,
            Err(e) => return fail(&job_id, e).await,
        };
        let status = match git(&root, &["status", "--porcelain"]).await {
            Ok(s) => s,
            Err(e) => return fail(&job_id, e).await,
        };
        if let Some(msg) = preflight(&branch, &status).message() {
            return fail(&job_id, msg).await;
        }

        // ── Pull ─────────────────────────────────────────────────────────────
        step(&job_id, "pulling the code").await;
        if let Err(e) = git(&root, &["pull", "--ff-only", "origin", "main"]).await {
            return fail(&job_id, e).await;
        }

        // ── Build ────────────────────────────────────────────────────────────
        step(&job_id, "building").await;
        let features = std::env::var(env_features()).ok();
        let args = build_args(features.as_deref());
        let out =
            tokio::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
                .args(&args)
                .current_dir(&root)
                .stdin(std::process::Stdio::null())
                .output()
                .await;
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let tail: String = err
                    .lines()
                    .rev()
                    .take(30)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                return fail(
                    &job_id,
                    format!("the build failed. Nobody restarted.\n{tail}"),
                )
                .await;
            }
            Err(e) => return fail(&job_id, format!("could not run cargo: {e}")).await,
        }

        // ── Restarts ─────────────────────────────────────────────────────────
        step(&job_id, "restarting the nodes of this machine").await;
        let me = crate::sysmon::sample().await.hostname;
        let peers = state.network.alive_peers().await;
        let tuples: Vec<(&str, &str, Option<u16>, &str)> = peers
            .iter()
            .map(|p| {
                (
                    p.hostname.as_str(),
                    p.addr.as_str(),
                    p.gateway_port,
                    p.node_id.as_str(),
                )
            })
            .collect();
        let targets = sibling_urls(&me, &tuples, same_machine);

        // Someone else's machine: a sibling that does not answer must not
        // vanish silently, or the operator is left with a node running an
        // old binary and no clue. But no sibling answering does not
        // stop this node from restarting - each machine annotates itself on its
        // own.
        let mut restarted = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for (node_id, url) in &targets {
            let Ok(client) = state.peer_http.for_peer(node_id) else {
                failed.push(url.clone());
                continue;
            };
            let Ok(request) = state
                .authorize_control_request(
                    client
                        .post(format!("{url}/api/ui/dev/restart"))
                        .timeout(std::time::Duration::from_secs(10)),
                    "POST",
                    "/api/ui/dev/restart",
                    &[],
                )
                .await
            else {
                failed.push(url.clone());
                continue;
            };
            match request.send().await {
                Ok(resp) if resp.status().is_success() => restarted += 1,
                Ok(resp) => {
                    tracing::warn!("sibling {url} refused the restart: {}", resp.status());
                    failed.push(url.clone());
                }
                Err(e) => {
                    tracing::warn!("could not restart sibling {url}: {e}");
                    failed.push(url.clone());
                }
            }
        }

        let siblings_msg = if targets.is_empty() {
            String::new()
        } else if failed.is_empty() {
            format!(" {restarted} sibling(s) of this machine also restarted.")
        } else {
            format!(
                " {restarted} sibling(s) of this machine restarted, {} failed: {}.",
                failed.len(),
                failed.join(", ")
            )
        };

        {
            let mut jobs = crate::webui::jobs().write().await;
            if let Some(j) = jobs.get_mut(&job_id) {
                j.state = "done";
                j.step = "pronto".into();
                j.message = format!("code updated; this machine is restarting.{siblings_msg}");
                j.finished_at = Some(chrono::Utc::now().to_rfc3339());
            }
        }

        // This node last, so the job gets to be marked as ready.
        //
        // Nothing to annotate here: whoever maintains the annotation is the runtime, and it already
        // reflects this node's truth (`Pending` since the apply, `Serving` since
        // `/health` answered). Rewriting it at this point could only
        // lie - stamping as serving a slice that never loaded is the
        // infinite loop of 2026-08-15 coming in through another door.
        tokio::time::sleep(RESTART_DELAY).await;
        let e = crate::respawn::respawn_self();
        tracing::error!("respawn failed, the node stays up: {e}");
    });

    Json(json!({ "job_id": id })).into_response()
}

async fn step(job_id: &str, s: &str) {
    let mut jobs = crate::webui::jobs().write().await;
    if let Some(j) = jobs.get_mut(job_id) {
        j.step = s.to_string();
    }
}

async fn restart_endpoint(
    _auth: crate::auth::ConnectionAuth,
    State(_state): State<GatewayState>,
) -> Response {
    if let Some(r) = gate_rejection() {
        return refuse(r);
    }

    // The slice is not annotated here: the runtime already keeps it annotated, with the state
    // it actually reached. A serving node comes back serving; a node whose
    // slice never loaded comes back empty, saying why.

    tokio::spawn(async {
        tokio::time::sleep(RESTART_DELAY).await;
        let e = crate::respawn::respawn_self();
        tracing::error!("respawn failed, the node stays up: {e}");
    });

    Json(json!({ "restarting": true })).into_response()
}

/// What keeps this machine from being updated right now.
#[derive(Debug, PartialEq)]
pub enum Preflight {
    Ok,
    WrongBranch(String),
    DetachedOrNoBranch,
    DirtyTree(Vec<String>),
}

/// Decides from the output of `git rev-parse --abbrev-ref HEAD` and of
/// `git status --porcelain`.
pub fn preflight(branch: &str, status_porcelain: &str) -> Preflight {
    let branch = branch.trim();
    if branch.is_empty() || branch == "HEAD" {
        return Preflight::DetachedOrNoBranch;
    }
    if branch != "main" {
        return Preflight::WrongBranch(branch.to_string());
    }

    // `??` is an untracked file: the pull does not touch it.
    let dirty: Vec<String> = status_porcelain
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("??"))
        .filter_map(|l| l.get(3..).map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    if !dirty.is_empty() {
        return Preflight::DirtyTree(dirty);
    }
    Preflight::Ok
}

impl Preflight {
    /// The message that goes to the screen, or `None` when it is fine to proceed.
    pub fn message(&self) -> Option<String> {
        match self {
            Preflight::Ok => None,
            Preflight::DetachedOrNoBranch => Some(
                "the repository of this machine is not on a branch (detached HEAD). The update only runs on main."
                    .into(),
            ),
            Preflight::WrongBranch(b) => Some(format!(
                "the repository of this machine is on branch '{b}', not on main. Someone may be working here — the update does not switch branches."
            )),
            Preflight::DirtyTree(files) => Some(format!(
                "there are uncommitted changes on this machine: {}. The update does not discard work, so it stops here.",
                files.join(", ")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate is per process and cannot be switched on over the network - unlike the config,
    /// which the interface itself rewrites. Only "1" turns it on: a "true" or "yes" that
    /// someone wrote thinking it turned it on must not open remote execution.
    #[test]
    fn only_the_value_1_opens_the_gate() {
        assert!(enabled_from(Some("1")));
        assert!(!enabled_from(Some("0")));
        assert!(!enabled_from(Some("true")));
        assert!(!enabled_from(Some("")));
        assert!(!enabled_from(None));
    }

    /// With the gate closed the route does not exist for practical purposes: it answers
    /// 403 without touching anything.
    #[test]
    fn a_closed_gate_refuses_with_403() {
        // In a test environment the variable is not set, so there is a refusal.
        let r = gate_rejection().expect("without the variable, it must refuse");
        assert_eq!(r.0, axum::http::StatusCode::FORBIDDEN);
        assert!(
            r.1.contains(&env_gate()),
            "the message must say which variable turns it on: {}",
            r.1
        );
    }

    /// On a machine where someone is coding, the button fails and explains, instead
    /// of tidying someone else's house. A `git pull --ff-only origin main` on a
    /// working branch would silently advance it to main.
    #[test]
    fn refuses_a_branch_other_than_main() {
        let p = preflight("fm/minha-feature", "");
        assert!(matches!(p, Preflight::WrongBranch(ref b) if b == "fm/minha-feature"));
        assert!(p.message().unwrap().contains("fm/minha-feature"));
    }

    #[test]
    fn refuses_a_detached_head() {
        // `git rev-parse --abbrev-ref HEAD` returns literally "HEAD".
        assert!(matches!(
            preflight("HEAD", ""),
            Preflight::DetachedOrNoBranch
        ));
        assert!(matches!(preflight("", ""), Preflight::DetachedOrNoBranch));
    }

    /// A dirty tree stops before touching anything, and says which files.
    /// A dirty-tree build corresponds to no commit, and the build_commit
    /// stamp would become a lie.
    #[test]
    fn refuses_a_dirty_tree_naming_the_files() {
        let status = " M src/llm.rs\n M src/webui.rs\n";
        let p = preflight("main", status);
        match &p {
            Preflight::DirtyTree(files) => {
                assert_eq!(
                    files,
                    &vec!["src/llm.rs".to_string(), "src/webui.rs".to_string()]
                );
            }
            other => panic!("expected a dirty tree, got {other:?}"),
        }
        assert!(p.message().unwrap().contains("src/llm.rs"));
    }

    /// An untracked file does not count: the pull does not touch it, and if it collides with a
    /// file from the remote git itself aborts without overwriting.
    #[test]
    fn an_untracked_file_does_not_block() {
        assert!(matches!(
            preflight("main", "?? gateway.rs\n"),
            Preflight::Ok
        ));
        assert!(preflight("main", "").message().is_none());
    }

    /// The build command must carry the machine's features. Without this
    /// ksia would lose CUDA on the first click - the same mistake already made by hand.
    #[test]
    fn the_build_carries_the_machine_features() {
        assert_eq!(build_args(None), vec!["build", "--release"]);
        assert_eq!(build_args(Some("")), vec!["build", "--release"]);
        assert_eq!(
            build_args(Some("cuda")),
            vec!["build", "--release", "--features", "cuda"]
        );
        assert_eq!(
            build_args(Some("metal")),
            vec!["build", "--release", "--features", "metal"]
        );
    }

    /// Without `node_id`, with this node's `node_id`, and with another peer's: the three
    /// answers that decide between executing here and forwarding.
    #[test]
    fn should_forward_only_with_another_peer_node_id() {
        assert!(
            !should_forward(None, "meu-id"),
            "without node_id, runs here"
        );
        assert!(
            !should_forward(Some("meu-id"), "meu-id"),
            "node_id equal to mine, runs here"
        );
        assert!(
            should_forward(Some("outro-id"), "meu-id"),
            "node_id of another peer, forwards"
        );
    }

    /// The siblings are the peers of the same machine, and this node is not in the list of
    /// peers - it restarts last, on its own.
    #[test]
    fn siblings_are_the_peers_with_the_same_hostname() {
        let targets = sibling_urls(
            "ksia",
            &[
                ("ksia", "[2804:d55::1]:42777", Some(8090), "id-a"),
                ("ksia", "[2804:d55::1]:42778", Some(8094), "id-b"),
                ("tiago-pc", "[2804:f30::9]:42777", Some(8080), "id-c"),
                ("ksia", "[2804:d55::1]:42779", None, "id-d"),
            ],
            |_| true,
        );
        assert_eq!(
            targets,
            vec![
                ("id-a".to_string(), "http://[2804:d55::1]:8090".to_string()),
                ("id-b".to_string(), "http://[2804:d55::1]:8094".to_string()),
            ],
            "another machine stays out, and a peer without gateway_port cannot be called"
        );
    }

    /// A repeated hostname is not enough: two VMs called `ksia` are not the same
    /// machine. `same_host` is what really decides that; here we simulate the
    /// refusal for one of the two addresses.
    #[test]
    fn the_same_hostname_on_a_different_machine_is_left_out() {
        let targets = sibling_urls(
            "ksia",
            &[
                ("ksia", "[2804:d55::1]:42777", Some(8090), "id-a"),
                ("ksia", "203.0.113.9:42777", Some(8091), "id-b"),
            ],
            |addr| addr.starts_with("[2804:d55::1]"),
        );
        assert_eq!(
            targets,
            vec![("id-a".to_string(), "http://[2804:d55::1]:8090".to_string())],
            "the VM at 203.0.113.9 has the same hostname but is not this machine"
        );
    }

    /// `0.0.0.0`/`::` is the sentinel of a peer that arrived through libp2p whose
    /// address has not yet been rewritten with the real origin. Matching as a
    /// sibling, the POST would go to this very node - which would restart before the
    /// job was marked as `done`. That is why the discard is explicit and does not
    /// depend on `same_host` approving or not.
    #[test]
    fn address_0_0_0_0_is_left_out() {
        let targets = sibling_urls(
            "ksia",
            &[
                ("ksia", "0.0.0.0:42777", Some(8090), "id-a"),
                ("ksia", "[::]:42778", Some(8091), "id-b"),
            ],
            |_| true,
        );
        assert!(
            targets.is_empty(),
            "an unrewritten sentinel cannot become a target: {targets:?}"
        );
    }
}
