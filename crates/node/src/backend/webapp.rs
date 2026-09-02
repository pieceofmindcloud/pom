//! Serving the CODAI-SERVER interface off disk.
//!
//! The bundle lives wherever `config::ui_dist_dir()` points - by default
//! `<project_root>/apps/frontend/dist`, built independently with
//! `npm run build` in `apps/frontend`. Nothing here is compiled into the
//! binary, so editing the interface no longer touches the Rust build: run
//! `npm run build` (or point `POM_UI_DIST` at a Vite watch build) and
//! reload the browser.
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
    routing::get,
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
        .route("/admin-ui/*path", get(asset))
        .route("/*path", get(asset))
}

async fn root() -> Redirect {
    Redirect::temporary("/admin-ui/")
}

async fn index() -> Response {
    let base = crate::config::ui_dist_dir();
    serve(&base, "index.html")
        .await
        .unwrap_or_else(|| missing_bundle(&base))
}

async fn asset(AxumPath(path): AxumPath<String>, uri: Uri) -> Response {
    let base = crate::config::ui_dist_dir();
    let trimmed = path
        .strip_prefix("admin-ui/")
        .unwrap_or(path.as_str())
        .trim_start_matches('/');

    if let Some(hit) = serve(&base, trimmed).await {
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
        return (StatusCode::NOT_FOUND, "asset não encontrado").into_response();
    }

    serve(&base, "index.html")
        .await
        .unwrap_or_else(|| missing_bundle(&base))
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

async fn serve(base: &Path, path: &str) -> Option<Response> {
    let file_path = resolve(base, path)?;
    let bytes = tokio::fs::read(&file_path).await.ok()?;
    let mime = mime_guess::from_path(&file_path).first_or_octet_stream();

    let mut res = Response::new(Body::from(bytes));
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
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

    Some(res)
}

/// The bundle is read from disk, so a miss here means it was never built —
/// say that, instead of a bare 404.
fn missing_bundle(base: &Path) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!(
            "A interface não foi encontrada em {}. Rode `npm run build` em apps/frontend \
             (ou aponte POM_UI_DIST para o bundle).",
            base.display()
        ),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

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
                "{p} devia ser tratado como rota de API"
            );
        }
        for p in ["config/keys/api", "fatias/andamento", "chat"] {
            assert!(
                !API_PREFIXES.iter().any(|pre| p.starts_with(pre)),
                "{p} e uma rota de tela e devia cair no index.html"
            );
        }
    }

    #[tokio::test]
    async fn root_redirects_to_the_admin_ui_mount() {
        let response = root().await.into_response();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/admin-ui/"
        );
    }
}
