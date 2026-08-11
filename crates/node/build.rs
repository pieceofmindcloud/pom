//! Puts the web interface where the binary can embed it.
//!
//! Every build recompiles the React interface with Vite, then hands its output
//! to the crate through `CODAI_UI_DIST`. Node and npm are therefore required
//! only on development and release-building machines. The resulting binary
//! embeds the assets with `rust-embed` and needs neither at runtime.

use std::path::{Path, PathBuf};
use std::process::Command;

const UI_SOURCE_INPUTS: &[&str] = &[
    "src",
    "index.html",
    "package.json",
    "package-lock.json",
    "vite.config.ts",
    "tailwind.config.js",
    "postcss.config.js",
    "tsconfig.json",
    "tsconfig.app.json",
    "tsconfig.node.json",
];

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let web_ui = root.join("apps/frontend");

    // Rebuild when the interface changes — and only then.
    for input in UI_SOURCE_INPUTS {
        println!("cargo:rerun-if-changed=apps/frontend/{input}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    let dist = build_from_source(&web_ui);

    println!("cargo:rustc-env=CODAI_UI_DIST={}", dist.display());
    emit_build_id(&root);
}

/// Stamp the binary with the commit it was built from.
///
/// A cluster is many machines, and "which build is that peer running?" was not
/// answerable without a shell on it. A peer running yesterday's binary looks
/// exactly like a peer with a fresh bug — the same layer range, the same
/// healthy `/health`, and an error from a code path that was already fixed.
/// Diagnosing one such peer cost an afternoon, so the answer now travels with
/// the node, in `/api/ui/node`.
fn emit_build_id(root: &Path) {
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
        "cargo:rustc-env=RDMA_P2P_BUILD_COMMIT={}{}",
        commit,
        if dirty { "-sujo" } else { "" }
    );
    // Without this the stamp freezes at whatever the first build saw.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}

/// Run Vite and embed what it produces.
fn build_from_source(web_ui: &Path) -> PathBuf {
    if !web_ui.join("package.json").exists() {
        panic!("nao achei {}/package.json", web_ui.display());
    }

    if !web_ui.join("node_modules").is_dir() {
        println!("cargo:warning=apps/frontend/node_modules ausente — rodando npm install (demora na primeira vez)");
        run(web_ui, "npm", &["install", "--no-audit", "--no-fund"]);
    }
    run(web_ui, "npm", &["run", "build"]);

    let dist = web_ui.join("dist");
    if !dist.join("index.html").exists() {
        panic!(
            "o build do Vite nao produziu {}",
            dist.join("index.html").display()
        );
    }
    dist
}

/// Spawn `program args…`, going through `cmd /C` on Windows.
///
/// npm there is `npm.cmd`, a batch file, and `CreateProcess` — what
/// `std::process::Command` calls — does not complete a bare name from `PATHEXT`
/// the way the shell does. `Command::new("npm")` therefore fails with "program
/// not found" on a machine where npm is installed and on the PATH, which is a
/// confusing thing to tell someone whose toolchain is fine.
fn command(program: &str, args: &[&str]) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(program).args(args);
        c
    } else {
        let mut c = Command::new(program);
        c.args(args);
        c
    }
}

fn run(cwd: &Path, program: &str, args: &[&str]) {
    let out = command(program, args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "nao deu para rodar `{program} {}` em {}: {e}\n\
                 O build recompila a interface e precisa de Node e npm na maquina de desenvolvimento.\n",
                args.join(" "),
                cwd.display()
            )
        });

    if !out.status.success() {
        panic!(
            "`{program} {}` falhou em {}:\n{}\n{}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
