//! Serving the CODAI-SERVER interface out of the binary.
//!
//! The bundle is compiled in from wherever `build.rs` pointed `CODAI_UI_DIST`,
//! so a node needs nothing on disk beside it to serve the panel — the same
//! property the old single-file `include_str!` had, extended to a real bundle
//! with hashed asset names.
//!
//! The routing rule is the part worth reading. A single-page app owns paths
//! that do not exist as files: reloading the browser on `/config/keys/api` has
//! to answer with `index.html` so the client router can take it from there.
//! Answering *everything* with `index.html` would be wrong in the other
//! direction — a mistyped `/api/ui/nodes` would return a web page with status
//! 200, and the screen would report "Unexpected token '<'" instead of a 404.
//! So the fallback is deliberately narrow: real files win, the API prefixes are
//! never touched, and anything left over that looks like a page gets the app.

use axum::{
    body::Body,
    extract::Path as AxumPath,
    http::{header, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$CODAI_UI_DIST"]
struct Bundle;

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
        .route("/", get(index))
        .route("/*path", get(asset))
}

async fn index() -> Response {
    serve("index.html").unwrap_or_else(missing_bundle)
}

async fn asset(AxumPath(path): AxumPath<String>, uri: Uri) -> Response {
    let trimmed = path.trim_start_matches('/');

    if let Some(hit) = serve(trimmed) {
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

    serve("index.html").unwrap_or_else(missing_bundle)
}

fn serve(path: &str) -> Option<Response> {
    let file = Bundle::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();

    let mut res = Response::new(Body::from(file.data.into_owned()));
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

/// The bundle is compiled in, so an empty one means the build went wrong rather
/// than anything the operator can fix at runtime — say that, instead of 404.
fn missing_bundle() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "A interface não foi embutida neste binário. Recompile com `cargo build --release`.",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundle has to actually be in the binary. Without this, a broken
    /// build.rs would only show up as a blank page on a deployed node.
    #[test]
    fn the_interface_is_embedded() {
        let index = Bundle::get("index.html").expect("index.html embutido");
        let html = String::from_utf8_lossy(&index.data);
        assert!(
            html.contains("CODAI-SERVER"),
            "o index.html embutido nao parece ser o do CODAI-SERVER: {html:.200}"
        );
        assert!(
            Bundle::iter().any(|f| f.ends_with(".js")),
            "nenhum javascript no pacote embutido"
        );
        assert!(
            Bundle::iter().any(|f| f.ends_with(".css")),
            "nenhum css no pacote embutido — foi assim que a tela crua passou batida"
        );
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
}
