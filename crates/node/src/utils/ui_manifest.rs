// A fingerprint of the web UI's source, shared by the build script and a test.
//
// This file is compiled twice on purpose: `build.rs` pulls it in with
// `include!` and the crate has it as a module. It must therefore stay free of
// crate imports and dependencies — plain `std` only.
//
// Why it exists: development rebuilds the interface from source on every
// `cargo build`, so the binary can never serve a stale UI. The release bundle
// is the one path where it could, because it is a generated artefact committed
// to the repository — cut a release with a stale bundle and the binary ships
// the old screens without a word. Hashing the inputs that produce the bundle
// turns that silent drift into a failure.

/// Everything whose change should produce a different bundle.
///
/// Directories are walked recursively. `node_modules` and build output are
/// excluded: they are derived, and including them would make the hash depend on
/// the machine rather than on the source.
pub const UI_SOURCE_INPUTS: &[&str] = &[
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

/// Name of the fingerprint file written beside a cut bundle.
pub const MANIFEST_FILE: &str = ".source-manifest";

/// Hash every source input under `web_ui_dir`, in a stable order.
///
/// Paths go into the hash next to their contents, so renaming a file changes
/// the fingerprint even when the bytes are only moved around.
pub fn ui_source_fingerprint(web_ui_dir: &std::path::Path) -> std::io::Result<String> {
    let mut files = Vec::new();
    for input in UI_SOURCE_INPUTS {
        collect(&web_ui_dir.join(input), web_ui_dir, &mut files)?;
    }
    files.sort();

    // FNV-1a over the whole stream. Not a cryptographic choice: nobody is
    // attacking this, it only has to change when the input changes, and keeping
    // it dependency-free is what lets `build.rs` and the test share this file.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    };

    for (rel, path) in &files {
        eat(rel.as_bytes());
        eat(&std::fs::read(path)?);
    }
    Ok(format!("{hash:016x}"))
}

fn collect(
    path: &std::path::Path,
    root: &std::path::Path,
    out: &mut Vec<(String, std::path::PathBuf)>,
) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_file() {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push((rel, path.to_path_buf()));
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "node_modules" || name == "dist" || name.starts_with('.') {
            continue;
        }
        collect(&entry.path(), root, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A committed bundle that no longer matches the source must fail here, not
    /// on a user's machine as an interface from two releases ago.
    ///
    /// Only runs when the bundle exists: outside a release cut there is nothing
    /// generated in the tree, and that is the normal state.
    #[test]
    fn the_versioned_bundle_matches_the_source() {
        let web_ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web-ui");
        let manifest = web_ui.join("dist-release").join(MANIFEST_FILE);
        let Ok(recorded) = std::fs::read_to_string(&manifest) else {
            return; // No bundle cut yet — nothing to contradict.
        };

        let actual = ui_source_fingerprint(&web_ui).expect("ler o fonte da interface");
        assert_eq!(
            recorded.trim(),
            actual,
            "\nweb-ui/dist-release foi gerado de outro fonte.\n\
             Regere antes de cortar o release: ./scripts/cut-ui-bundle.sh\n"
        );
    }

    /// The fingerprint has to actually react to a change, or it proves nothing.
    #[test]
    fn a_changed_source_changes_the_fingerprint() {
        let dir = std::env::temp_dir().join(format!("codai-fp-{}", std::process::id()));
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.ts"), "export const a = 1;").unwrap();

        let before = ui_source_fingerprint(&dir).unwrap();
        std::fs::write(src.join("a.ts"), "export const a = 2;").unwrap();
        let after = ui_source_fingerprint(&dir).unwrap();
        assert_ne!(
            before, after,
            "editar o fonte tem de mudar a impressao digital"
        );

        // Renaming alone must count too: same bytes, different bundle.
        std::fs::rename(src.join("a.ts"), src.join("b.ts")).unwrap();
        let renamed = ui_source_fingerprint(&dir).unwrap();
        assert_ne!(after, renamed, "renomear tem de mudar a impressao digital");

        std::fs::remove_dir_all(&dir).ok();
    }
}
