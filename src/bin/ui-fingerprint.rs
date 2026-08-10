//! Prints the fingerprint of the web UI source.
//!
//! Exists so `scripts/cut-ui-bundle.sh` records exactly the value the build
//! script will later recompute, rather than a second implementation that could
//! drift from it.

include!("../ui_manifest.rs");

fn main() {
    let web_ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web-ui");
    match ui_source_fingerprint(&web_ui) {
        Ok(fp) => println!("{fp}"),
        Err(e) => {
            eprintln!("não deu para ler o fonte da interface em {}: {e}", web_ui.display());
            std::process::exit(1);
        }
    }
}
