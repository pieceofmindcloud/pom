//! Limite e retenção do armazenamento de modelos fatiados.

use anyhow::{bail, Context, Result};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::debug;

/// Espaço padrão reservado para fatias novas quando a configuração ainda não
/// tem o campo introduzido nesta versão.
pub const DEFAULT_SHARD_MAX_GB: f64 = 100.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageUsage {
    pub used_bytes: u64,
    pub max_bytes: u64,
}

impl StorageUsage {
    pub fn percent(&self) -> f64 {
        if self.max_bytes == 0 {
            return 0.0;
        }
        (self.used_bytes as f64 / self.max_bytes as f64 * 100.0).min(100.0)
    }
}

pub fn directory_bytes(root: &Path) -> std::io::Result<u64> {
    let metadata = fs::metadata(root)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} não é um diretório", root.display()),
        ));
    }

    directory_bytes_inner(root)
}

fn directory_bytes_inner(root: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(root)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                debug!(
                    path = %root.display(),
                    error = %error,
                    "ignorando entrada ilegível durante o cálculo de uso"
                );
                continue;
            }
        };
        let path = entry.path();
        let ty = match entry.file_type() {
            Ok(ty) => ty,
            Err(error) => {
                debug!(
                    path = %path.display(),
                    error = %error,
                    "ignorando entrada cujo tipo não pôde ser lido durante o cálculo de uso"
                );
                continue;
            }
        };
        if ty.is_dir() {
            match directory_bytes_inner(&path) {
                Ok(bytes) => total = total.saturating_add(bytes),
                Err(error) => {
                    debug!(
                        path = %path.display(),
                        error = %error,
                        "ignorando diretório ilegível durante o cálculo de uso"
                    );
                }
            }
        } else if ty.is_file() {
            match entry.metadata() {
                Ok(metadata) => total = total.saturating_add(metadata.len()),
                Err(error) => {
                    debug!(
                        path = %path.display(),
                        error = %error,
                        "ignorando arquivo cujo tamanho não pôde ser lido durante o cálculo de uso"
                    );
                }
            }
        }
    }
    Ok(total)
}

#[allow(dead_code)]
pub fn enforce_limit(root: &Path, max_bytes: u64, protected: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut used = directory_bytes(root)
        .with_context(|| {
            format!(
                "não foi possível calcular o uso de {}. Verifique se a pasta existe, é um diretório e se o nó tem permissão de leitura",
                root.display()
            )
        })?;
    if used <= max_bytes {
        return Ok(Vec::new());
    }

    let canonical_protected: Vec<PathBuf> = protected
        .iter()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect();
    let mut candidates = Vec::new();
    for entry in fs::read_dir(root).with_context(|| {
        format!(
            "não foi possível calcular o uso de {}. Verifique se a pasta existe e se o nó tem permissão de leitura",
            root.display()
        )
    })? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    debug!(
                        path = %root.display(),
                        error = %error,
                        "ignorando entrada ilegível durante a busca de fatias antigas"
                    );
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    debug!(
                        path = %path.display(),
                        error = %error,
                        "ignorando entrada cujo tipo não pôde ser lido durante a busca de fatias antigas"
                    );
                    continue;
                }
            };
            if !file_type.is_dir() || entry.file_name() == "recebidos" {
                continue;
            }
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if canonical_protected
                .iter()
                .any(|protected| protected.starts_with(&canonical))
            {
                continue;
            }
            match directory_bytes(&path) {
                Ok(0) => continue,
                Ok(_) => {}
                Err(error) => {
                    debug!(
                        path = %path.display(),
                        error = %error,
                        "ignorando fatia antiga ilegível durante a retenção"
                    );
                    continue;
                }
            }
            let created = entry
                .metadata()
                .ok()
                .and_then(|m| m.created().ok())
                .or_else(|| entry.metadata().ok().and_then(|m| m.modified().ok()))
                .unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((created, path));
    }

    candidates.sort_by(|(a_time, a_path), (b_time, b_path)| {
        a_time.cmp(b_time).then_with(|| a_path.cmp(b_path))
    });

    let mut removed = Vec::new();
    for (_, path) in candidates {
        if used <= max_bytes {
            break;
        }
        let bytes = match directory_bytes(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                debug!(
                    path = %path.display(),
                    error = %error,
                    "ignorando fatia antiga que deixou de ser legível durante a retenção"
                );
                continue;
            }
        };
        fs::remove_dir_all(&path)
            .with_context(|| format!("não deu para remover fatia antiga {}", path.display()))?;
        used = used.saturating_sub(bytes);
        removed.push(path);
    }

    if used > max_bytes {
        bail!(
            "o limite de armazenamento das fatias foi excedido: {} bytes usados de {} e não há fatias antigas elegíveis para remover",
            used,
            max_bytes
        );
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pom-storage-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn sums_nested_files_without_following_directories_as_files() {
        let root = temp_root("usage");
        fs::create_dir_all(root.join("qwen")).unwrap();
        fs::write(root.join("qwen/part.gguf"), vec![0u8; 7]).unwrap();
        fs::write(root.join("notes"), vec![0u8; 3]).unwrap();

        assert_eq!(directory_bytes(&root).unwrap(), 10);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn sums_readable_files_when_nested_directory_is_inaccessible() {
        let uid = std::process::Command::new("id").arg("-u").output().unwrap();
        if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
            eprintln!("skipped: permission test cannot exercise EACCES as root");
            return;
        }

        let root = temp_root("inaccessible");
        fs::write(root.join("readable.gguf"), vec![0u8; 7]).unwrap();
        let inaccessible = root.join("inaccessible");
        fs::create_dir(&inaccessible).unwrap();
        fs::write(inaccessible.join("hidden.gguf"), vec![0u8; 11]).unwrap();

        let mut permissions = fs::metadata(&inaccessible).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o000);
        fs::set_permissions(&inaccessible, permissions).unwrap();

        let measured = directory_bytes(&root);

        let mut permissions = fs::metadata(&inaccessible).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&inaccessible, permissions).unwrap();

        assert_eq!(measured.unwrap(), 7);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn enforce_limit_ignores_unreadable_child() {
        let uid = std::process::Command::new("id").arg("-u").output().unwrap();
        if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
            eprintln!("skipped: permission test cannot exercise EACCES as root");
            return;
        }

        let root = temp_root("inaccessible-limit");
        fs::write(root.join("readable.gguf"), vec![0u8; 7]).unwrap();
        let inaccessible = root.join("inaccessible");
        fs::create_dir(&inaccessible).unwrap();
        fs::write(inaccessible.join("hidden.gguf"), vec![0u8; 11]).unwrap();

        let mut permissions = fs::metadata(&inaccessible).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o000);
        fs::set_permissions(&inaccessible, permissions).unwrap();

        let result = enforce_limit(&root, 7, &[]);

        let mut permissions = fs::metadata(&inaccessible).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&inaccessible, permissions).unwrap();

        assert!(
            result.is_ok(),
            "unreadable child must not abort apply: {result:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_root_reports_path_cause_and_recovery_action() {
        let root = std::env::temp_dir().join(format!(
            "pom-storage-missing-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);

        let error = enforce_limit(&root, 1, &[]).unwrap_err().to_string();

        assert!(error.contains(root.to_string_lossy().as_ref()));
        assert!(error.contains("existe"));
        assert!(error.contains("permissão"));
    }

    #[test]
    fn removes_oldest_slice_directory_until_the_limit_is_met() {
        let root = temp_root("retention");
        let old = root.join("a-old-model");
        let new = root.join("z-new-model");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("old.gguf"), vec![0u8; 8]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::create_dir_all(&new).unwrap();
        fs::write(new.join("new.gguf"), vec![0u8; 8]).unwrap();

        let removed = enforce_limit(&root, 8, &[]).unwrap();
        assert_eq!(removed, vec![old.clone()]);
        assert!(!old.exists());
        assert!(root.join("z-new-model/new.gguf").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn protected_slice_is_never_removed_and_reports_when_limit_is_impossible() {
        let root = temp_root("protected");
        let active = root.join("active");
        fs::create_dir_all(&active).unwrap();
        fs::write(active.join("active.gguf"), vec![0u8; 8]).unwrap();

        let error = enforce_limit(&root, 1, &[active.clone()]).unwrap_err();
        assert!(error.to_string().contains("limite"));
        assert!(active.join("active.gguf").exists());
        let _ = fs::remove_dir_all(root);
    }
}
