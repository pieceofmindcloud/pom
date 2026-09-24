use std::path::Path;

pub fn prepare_prebuilt_libraries(source_dir: &Path, out_dir: &Path) -> Result<(), String> {
    const REQUIRED_LIBS: [&str; 4] = [
        "libllama.so",
        "libggml.so",
        "libggml-base.so",
        "libllama-common.so",
    ];
    let lib_dir = out_dir.join("lib");
    std::fs::create_dir_all(&lib_dir).map_err(|error| error.to_string())?;
    for name in REQUIRED_LIBS {
        let source = source_dir.join(name);
        if !source.is_file() {
            return Err(format!("required prebuilt library is missing: {}", source.display()));
        }
        println!("cargo:rerun-if-changed={}", source.display());
        std::fs::copy(&source, lib_dir.join(name)).map_err(|error| {
            format!("could not stage prebuilt library {}: {error}", source.display())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::prepare_prebuilt_libraries;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pom-llama-prebuilt-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn stages_the_required_shared_libraries() {
        let root = temp_dir("complete");
        let source = root.join("source");
        let out = root.join("out");
        fs::create_dir_all(&source).unwrap();
        for name in [
            "libllama.so",
            "libggml.so",
            "libggml-base.so",
            "libllama-common.so",
        ] {
            fs::write(source.join(name), name).unwrap();
        }

        prepare_prebuilt_libraries(&source, &out).unwrap();
        for name in [
            "libllama.so",
            "libggml.so",
            "libggml-base.so",
            "libllama-common.so",
        ] {
            assert_eq!(fs::read_to_string(out.join("lib").join(name)).unwrap(), name);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_a_package_missing_a_required_library() {
        let root = temp_dir("missing");
        fs::create_dir_all(&root).unwrap();
        let error = prepare_prebuilt_libraries(&root, &root.join("out")).unwrap_err();
        assert!(error.contains("libllama.so"));
        fs::remove_dir_all(root).unwrap();
    }
}
