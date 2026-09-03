//! Stamps the binary with the commit it was built from.
//!
//! The web interface (`apps/frontend`) is *not* built or embedded here anymore:
//! the node reads it off disk at runtime (see `src/backend/webapp.rs`), so
//! editing the interface no longer touches the Rust build at all. Build it
//! separately with `npm run build` in `apps/frontend` when you need fresh
//! assets on disk.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");

    emit_build_id(&root);
    embed_windows_icon(&root);
}

/// Grava o ícone do aplicativo dentro do `.exe` (só no Windows).
///
/// Um executável Unix não carrega ícone: no macOS quem o mostra é o bundle
/// `.app` (`scripts/make-macos-app.sh`) e no Linux é o `.desktop` com a árvore
/// `hicolor` (os dois saem de `assets/brand/`). No Windows o ícone é um recurso
/// dentro do próprio arquivo, e é aqui que ele entra - sem isto o Explorer
/// mostra o ícone genérico de console.
///
/// O `cfg(windows)` é do **host**, não do alvo, e é o mesmo critério que traz o
/// `winresource` como build-dependency (`[target.'cfg(windows)'.build-dependencies]`
/// também é host). Os dois casam: o release compila o alvo Windows num runner
/// Windows. Cruzar de Linux para Windows sai sem ícone, em silêncio, em vez de
/// falhar o build de quem só quer o nó.
#[cfg(windows)]
fn embed_windows_icon(root: &Path) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = root.join("assets/brand/pom.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    if !icon.exists() {
        println!(
            "cargo:warning=ícone do Windows não encontrado em {}",
            icon.display()
        );
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon(&icon.to_string_lossy());
    if let Err(e) = res.compile() {
        // Um ícone ausente não pode impedir alguém de compilar o nó.
        println!("cargo:warning=não deu para embutir o ícone no executável: {e}");
    }
}

#[cfg(not(windows))]
fn embed_windows_icon(_root: &Path) {}

/// Stamp the binary with the commit it was built from.
///
/// A cluster is many machines, and "which build is that peer running?" was not
/// answerable without a shell on it. A peer running yesterday's binary looks
/// exactly like a peer with a fresh bug — the same layer range, the same
/// healthy `/health`, and an error from a code path that was already fixed.
/// Diagnosing one such peer cost an afternoon, so the answer now travels with
/// the node, in `/api/ui/node`.
fn emit_build_id(root: &Path) {
    // A build that cannot run `git` has to be *told* the commit, and the image
    // build is exactly that: `.dockerignore` keeps `.git/` out of the context
    // and the builder image has no `git` binary, so both halves below fail and
    // every container reported "desconhecido" — the one build whose version you
    // most need, since a container is the hardest place to get a shell into.
    // `scripts/build-docker.sh` reads the commit on the host and passes it in
    // through this variable.
    println!("cargo:rerun-if-env-changed=POM_BUILD_COMMIT");
    if let Ok(stamped) = std::env::var("POM_BUILD_COMMIT") {
        let stamped = stamped.trim();
        if !stamped.is_empty() {
            println!("cargo:rustc-env=POM_BUILD_COMMIT={stamped}");
            return;
        }
    }

    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "desconhecido".into());

    // A dirty tree is a build that matches no commit; saying so is the point.
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    println!(
        "cargo:rustc-env=POM_BUILD_COMMIT={}{}",
        commit,
        if dirty { "-sujo" } else { "" }
    );
    // Without this the stamp freezes at whatever the first build saw. The paths
    // are resolved against the package directory, not the workspace, so they
    // have to be spelled out from the root or they name files that never exist.
    let git = root.join(".git");
    println!("cargo:rerun-if-changed={}", git.join("HEAD").display());
    println!("cargo:rerun-if-changed={}", git.join("index").display());
}
