//! Puts the web interface where the binary can embed it.
//!
//! Two paths, and the hierarchy between them is the whole point:
//!
//! * **Development (default).** `cargo build` rebuilds the interface from
//!   source with Vite. Node and npm are required on the development machines,
//!   which is the price for the property that matters: the binary physically
//!   cannot serve an interface that disagrees with `web-ui/src`.
//!
//! * **Release (`--features release-bundle`).** Nothing is rebuilt; the bundle
//!   committed at `web-ui/dist-release` is embedded as-is, so cutting a release
//!   needs no JavaScript toolchain at all. That convenience is exactly where a
//!   stale bundle could slip through, so this path refuses to build unless the
//!   committed bundle's fingerprint matches the current source.
//!
//! Either way the chosen directory is handed to the crate through
//! `CODAI_UI_DIST`, and `rust-embed` compiles it in from there.

use std::path::{Path, PathBuf};
use std::process::Command;

include!("src/ui_manifest.rs");

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let web_ui = root.join("web-ui");

    // Rebuild when the interface changes — and only then.
    for input in UI_SOURCE_INPUTS {
        println!("cargo:rerun-if-changed=web-ui/{input}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/ui_manifest.rs");

    let dist = if cfg!(feature = "release-bundle") {
        release_bundle(&web_ui)
    } else {
        build_from_source(&web_ui)
    };

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

/// Embed the committed bundle, but only if it still matches the source.
fn release_bundle(web_ui: &Path) -> PathBuf {
    let dist = web_ui.join("dist-release");
    if !dist.join("index.html").exists() {
        panic!(
            "--features release-bundle, mas nao ha pacote em {}.\n\
             Gere com: ./scripts/cut-ui-bundle.sh",
            dist.display()
        );
    }

    let recorded = std::fs::read_to_string(dist.join(MANIFEST_FILE))
        .unwrap_or_else(|_| {
            panic!(
                "o pacote em {} nao tem {MANIFEST_FILE}, entao nao da para provar \
                 que ele corresponde ao codigo. Regere com ./scripts/cut-ui-bundle.sh",
                dist.display()
            )
        })
        .trim()
        .to_string();

    let actual = ui_source_fingerprint(web_ui).expect("ler o fonte da interface");

    // The failure this exists for: someone edits the UI, cuts a release, and the
    // binary ships the old screens without a word. Loud, with the way out.
    assert_eq!(
        recorded, actual,
        "\n\nO pacote versionado da interface esta velho.\n\
         web-ui/dist-release foi gerado de um fonte diferente do que esta aqui \
         (gravado {recorded}, atual {actual}).\n\
         Regere e commite antes de cortar o release:\n\
         \n    ./scripts/cut-ui-bundle.sh\n\n"
    );

    dist
}

/// Run Vite and embed what it produces.
fn build_from_source(web_ui: &Path) -> PathBuf {
    if !web_ui.join("package.json").exists() {
        panic!("nao achei {}/package.json", web_ui.display());
    }

    if !web_ui.join("node_modules").is_dir() {
        println!("cargo:warning=web-ui/node_modules ausente — rodando npm install (demora na primeira vez)");
        run(web_ui, "npm", &["install", "--no-audit", "--no-fund"]);
    }
    run(web_ui, "npm", &["run", "build"]);

    let dist = web_ui.join("dist");
    if !dist.join("index.html").exists() {
        panic!("o build do Vite nao produziu {}", dist.join("index.html").display());
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
                 O build padrao reconstroi a interface e precisa de Node e npm. \
                 Para compilar sem eles, use o pacote versionado:\n\
                 \n    cargo build --release --features release-bundle\n",
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
