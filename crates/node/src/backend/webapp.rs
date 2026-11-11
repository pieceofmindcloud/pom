//! Serving the CODAI-SERVER interface off disk.
//!
//! The bundle lives wherever `config::ui_dist_dir()` points - by default
//! `<project_root>/apps/frontend/dist`, built independently with
//! `npm run build` in `apps/frontend`. Nothing is compiled into the binary by
//! default, so editing the interface needs no Rust build: run `npm run build`
//! (or point `POM_UI_DIST` at a Vite watch build) and reload the browser. The
//! `embed-ui` feature compiles the bundle into the executable instead; the
//! Windows release uses it, so its package has no `apps/` folder.
//!
//! The routing rule is the part worth reading. The single-page app is mounted
//! at `/admin-ui/`, while the root redirects there without exposing a bundle
//! from the old root. Reloading a browser route such as
//! `/admin-ui/config/keys/api` answers with `index.html` so the client router
//! can take it from there. API routes are kept outside this mount, so a mistyped
//! `/api/ui/nodes` still gets a JSON 404 instead of a web page with status 200.

use std::path::{Component, Path, PathBuf};

use axum::{
    body::Body,
    extract::Path as AxumPath,
    http::{header, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::{any, get},
    Router,
};

/// Path prefixes the node owns. Nothing under these ever falls back to the app.
///
/// Kept next to the fallback because the two only make sense together: this is
/// the list that decides whether a miss is "an unknown API route" (404, JSON)
/// or "a page the client router knows about" (index.html).
const API_PREFIXES: &[&str] = &["api/", "v1/", "admin/", "peers", "health", "generate"];

/// Generic over the router state because these handlers need none, and the two
/// callers carry different state: the full gateway, and the setup-mode router
/// that exists before there is any cluster to hold state about.
pub fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/", get(root))
        .route("/admin-ui", get(index))
        .route("/admin-ui/", get(index))
        .route("/admin-ui/*path", any(asset))
        .fallback(fallback)
}

/// The query string goes along: a sign-in or a demo answer can land on `/`
/// with something the screen reads (`?demo_cooldown=`), and dropping it sent
/// a demo guest whose time ran out straight back to the guest sign-in, in a
/// reload loop.
async fn root(uri: axum::http::Uri) -> Redirect {
    match uri.query().filter(|query| !query.is_empty()) {
        Some(query) => Redirect::temporary(&format!("/admin-ui/?{query}")),
        None => Redirect::temporary("/admin-ui/"),
    }
}

async fn index() -> Response {
    serve_file("index.html")
        .await
        .unwrap_or_else(|| missing_bundle(&crate::config::ui_dist_dir()))
}

async fn asset(AxumPath(path): AxumPath<String>, uri: Uri, method: axum::http::Method) -> Response {
    if method != axum::http::Method::GET {
        return StatusCode::NOT_FOUND.into_response();
    }

    let trimmed = path
        .strip_prefix("admin-ui/")
        .unwrap_or(path.as_str())
        .trim_start_matches('/');

    if let Some(hit) = serve_file(trimmed).await {
        return hit;
    }

    // An unknown path under an API prefix is a 404, not the app: a client that
    // asked for JSON must not be handed a web page with a 200.
    if API_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            format!(r#"{{"error":"rota desconhecida: {}"}}"#, uri.path()),
        )
            .into_response();
    }

    // A miss that still names a file extension is a genuinely absent asset —
    // usually a stale bundle reference. Saying so beats serving the shell and
    // letting the browser fail on an HTML file it expected to be JavaScript.
    if trimmed
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'))
    {
        return (StatusCode::NOT_FOUND, "asset not found").into_response();
    }

    serve_file("index.html")
        .await
        .unwrap_or_else(|| missing_bundle(&crate::config::ui_dist_dir()))
}

/// Serve the SPA only after no concrete API or UI route matched. Keeping this
/// as a router fallback is important: a GET-only wildcard otherwise claims the
/// path of every unknown POST/DELETE and turns a missing route into 405.
async fn fallback(method: axum::http::Method, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/').to_string();
    asset(AxumPath(path), uri, method).await
}

/// Join a request path onto the bundle directory without ever leaving it.
///
/// Unlike the old embedded lookup (a fixed compile-time file list), this reads
/// real files at a path built from user input, so `..` has to be rejected
/// explicitly rather than just failing to match — `/../../etc/passwd` must not
/// resolve outside `base`.
fn resolve(base: &Path, path: &str) -> Option<PathBuf> {
    let mut full = base.to_path_buf();
    for part in Path::new(path).components() {
        match part {
            Component::Normal(seg) => full.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(full)
}

/// The bundle compiled into the executable, in a build with `embed-ui`. The path is
/// relative to this crate, so it names the repository's `apps/frontend/dist`.
#[cfg(feature = "embed-ui")]
static EMBEDDED_UI: include_dir::Dir<'static> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../apps/frontend/dist");

/// Serves one file of the interface. It comes from disk, or in an `embed-ui` build
/// from the executable. `POM_UI_DIST` always means a bundle on disk, so a developer
/// pointing at a Vite watch build still sees it.
async fn serve_file(path: &str) -> Option<Response> {
    #[cfg(feature = "embed-ui")]
    if crate::config::ui_dist_override().is_none() {
        return serve_embedded(path);
    }
    serve(&crate::config::ui_dist_dir(), path).await
}

#[cfg(feature = "embed-ui")]
fn serve_embedded(path: &str) -> Option<Response> {
    // Same rule as `resolve`: a `..` or an absolute path never leaves the bundle.
    let mut key = PathBuf::new();
    for part in Path::new(path).components() {
        match part {
            Component::Normal(seg) => key.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let file = EMBEDDED_UI.get_file(&key)?;
    let mime = mime_guess::from_path(&key).first_or_octet_stream();
    Some(bundle_response(
        Body::from(axum::body::Bytes::from_static(file.contents())),
        path,
        mime.as_ref(),
    ))
}

async fn serve(base: &Path, path: &str) -> Option<Response> {
    let file_path = resolve(base, path)?;
    let bytes = tokio::fs::read(&file_path).await.ok()?;
    let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
    Some(bundle_response(Body::from(bytes), path, mime.as_ref()))
}

fn bundle_response(body: Body, path: &str, mime: &str) -> Response {
    let mut res = Response::new(body);
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );

    // Vite puts a content hash in every asset name, so those are immutable and
    // worth caching hard. index.html is the one file whose contents change
    // under a fixed name, and caching it is how a browser keeps showing the
    // previous release after an upgrade.
    let cache = if path == "index.html" {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));

    res
}

/// The bundle is read from disk, so a miss here means it was never built —
/// say that, instead of a bare 404.
fn missing_bundle(base: &Path) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!(
            "The interface was not found in {}. Run `npm run build` in apps/frontend (or point POM_UI_DIST to the bundle).",
            base.display()
        ),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, routing::post};
    use tower::ServiceExt;

    /// `..` must never walk the resolved path outside the bundle directory,
    /// no matter how it is spelled in the request.
    #[test]
    fn parent_dir_segments_are_rejected() {
        let base = Path::new("/srv/dist");
        assert_eq!(resolve(base, "../../etc/passwd"), None);
        assert_eq!(resolve(base, "assets/../../secret"), None);
        assert_eq!(resolve(base, "./index.html"), Some(base.join("index.html")));
        assert_eq!(
            resolve(base, "assets/app.js"),
            Some(base.join("assets/app.js"))
        );
    }

    /// A rooted or absolute-looking segment must not escape `base` either.
    #[test]
    fn absolute_looking_paths_are_rejected() {
        let base = Path::new("/srv/dist");
        assert_eq!(resolve(base, "/etc/passwd"), None);
    }

    /// Serving from a real temp directory exercises the actual filesystem
    /// path, not just the pure path-joining logic above.
    #[tokio::test]
    async fn serves_files_from_the_bundle_directory() {
        let dir = std::env::temp_dir().join(format!("pom-webapp-test-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(dir.join("assets")).await.unwrap();
        tokio::fs::write(dir.join("index.html"), "<html>CODAI-SERVER</html>")
            .await
            .unwrap();
        tokio::fs::write(dir.join("assets/app.js"), "console.log(1)")
            .await
            .unwrap();

        assert!(serve(&dir, "index.html").await.is_some());
        assert!(serve(&dir, "assets/app.js").await.is_some());
        assert!(serve(&dir, "assets/missing.js").await.is_none());
        assert!(serve(&dir, "../outside").await.is_none());

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    /// Both halves of the routing rule, which are easy to get backwards.
    #[test]
    fn api_prefixes_never_fall_back_to_the_app() {
        for p in ["api/ui/node", "v1/models", "admin/nodes", "health"] {
            assert!(
                API_PREFIXES.iter().any(|pre| p.starts_with(pre)),
                "{p} should be treated as an API route"
            );
        }
        for p in [
            "config/keys/api",
            "fatias/andamento",
            "chat",
            "admin-ui/chat",
            "base/plugin-de-base",
        ] {
            assert!(
                !API_PREFIXES.iter().any(|pre| p.starts_with(pre)),
                "{p} is a screen route and should fall to index.html"
            );
        }
    }

    #[tokio::test]
    async fn root_redirects_to_the_admin_ui_mount() {
        let response = root("/".parse().unwrap()).await.into_response();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/admin-ui/"
        );
    }

    #[tokio::test]
    async fn the_root_redirect_keeps_the_query_string() {
        let response = root("/?demo_cooldown=120".parse().unwrap())
            .await
            .into_response();
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/admin-ui/?demo_cooldown=120"
        );
    }

    #[tokio::test]
    async fn a_specific_admin_post_beats_the_get_only_spa_fallback() {
        let app = Router::new()
            .route(
                "/admin/nodes/:id/promote-admin",
                post(|| async { StatusCode::NO_CONTENT }),
            )
            .merge(routes::<()>());

        let response = app
            .oneshot(
                Request::post("/admin/nodes/peer/promote-admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn the_registered_admin_promotion_route_reaches_its_handler() {
        let state = crate::gateway::testkit::state_with("node", vec![]).await;
        {
            let mut auth = state.auth.write().await;
            auth.connection_key = "connection-secret".into();
        }
        let app = Router::new()
            .merge(crate::admin::admin_routes())
            .merge(routes())
            .with_state(state);

        let response = app
            .oneshot(
                Request::post("/admin/nodes/peer/promote-admin")
                    .header("authorization", "Bearer connection-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_unknown_mutation_is_not_found_not_method_not_allowed() {
        for request in [
            Request::post("/admin/nodes/unknown/promote-admin")
                .body(Body::empty())
                .unwrap(),
            Request::delete("/admin/nodes/unknown")
                .body(Body::empty())
                .unwrap(),
        ] {
            let response = routes::<()>().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }
}
