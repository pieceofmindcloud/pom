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
    build_macos_status();
    embed_windows_icon(&root);
}

/// Compiles the Objective-C bridge of the status bar item, only on the macOS target.
///
/// AppKit and WebKit come in through the system frameworks, not through a Rust
/// dependency nor a bundled browser runtime: the bundle stays thin and
/// Linux and Windows gain nothing from this.
///
/// The outer `cfg(macos)` refers to the **host** and brings `cc` in as a build-dependency,
/// for exactly the reason described in `embed_windows_icon`. The test against
/// `CARGO_CFG_TARGET_OS` refers to the **target**, which decides whether the bridge has to
/// exist in the binary. Both must hold: building a macOS target is something
/// that only happens on a macOS host.
#[cfg(target_os = "macos")]
fn build_macos_status() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    println!("cargo:rerun-if-changed=src/macos_status.m");
    cc::Build::new()
        .file("src/macos_status.m")
        .flag("-fobjc-arc")
        .compile("pom_macos_status");
    println!("cargo:rustc-link-lib=framework=AppKit");
    println!("cargo:rustc-link-lib=framework=WebKit");
}

#[cfg(not(target_os = "macos"))]
fn build_macos_status() {}

/// Embeds the application icon inside the `.exe` (Windows only).
///
/// A Unix executable carries no icon: on macOS the bundle `.app`
/// (`scripts/make-macos-app.sh`) shows it and on Linux it is the `.desktop` with the
/// `hicolor` tree (both come from `assets/brand/`). On Windows the icon is a resource
/// inside the file itself, and this is where it goes in - without this Explorer
/// shows the generic console icon.
///
/// The `cfg(windows)` refers to the **host**, not the target, and is the same criterion that brings
/// `winresource` in as a build-dependency (`[target.'cfg(windows)'.build-dependencies]`
/// is also the host). The two match: the release builds the Windows target on a
/// Windows runner. Cross-compiling from Linux to Windows comes out without an icon, silently, instead of
/// failing the build for someone who only wants the node.
#[cfg(windows)]
fn embed_windows_icon(root: &Path) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = root.join("assets/brand/pom.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    if !icon.exists() {
        println!(
            "cargo:warning=Windows icon not found in {}",
            icon.display()
        );
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon(&icon.to_string_lossy());
    if let Err(e) = res.compile() {
        // A missing icon must not stop anyone from building the node.
        println!("cargo:warning=could not embed the icon in the executable: {e}");
    }
}

#[cfg(not(windows))]
fn embed_windows_icon(_root: &Path) {}

/// Stamp the binary with the commit it was built from.
///
/// A cluster is many machines, and "which build is that peer running?" was not
/// answerable without a shell on it. A peer running yesterday's binary looks
/// exactly like a peer with a fresh bug - the same layer range, the same
/// healthy `/health`, and an error from a code path that was already fixed.
/// Diagnosing one such peer cost an afternoon, so the answer now travels with
/// the node, in `/api/ui/node`.
fn emit_build_id(root: &Path) {
    // Docker images do not include `.git/`, so their compiled fallback is
    // `desconhecido`. The host commit is runtime metadata in the final image;
    // keeping it out of this build script prevents a commit-only change from
    // invalidating Cargo's expensive build layer.
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());

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
        if dirty { "-dirty" } else { "" }
    );
    // Without this the stamp freezes at whatever the first build saw. The paths
    // are resolved against the package directory, not the workspace, so they
    // have to be spelled out from the root or they name files that never exist.
    let git = root.join(".git");
    println!("cargo:rerun-if-changed={}", git.join("HEAD").display());
    println!("cargo:rerun-if-changed={}", git.join("index").display());
}
