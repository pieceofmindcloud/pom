//! Limite e retenção do armazenamento de modelos fatiados.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    if !root.is_dir() {
        return Ok(0);
    }

    let mut total = 0u64;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            total = total.saturating_add(directory_bytes(&entry.path())?);
        } else if ty.is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

pub fn enforce_limit(root: &Path, max_bytes: u64, protected: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut used = directory_bytes(root)
        .with_context(|| format!("não deu para calcular o uso de {}", root.display()))?;
    if used <= max_bytes {
        return Ok(Vec::new());
    }

    let canonical_protected: Vec<PathBuf> = protected
        .iter()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect();
    let mut candidates = Vec::new();
    if root.is_dir() {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_name() == "recebidos" {
                continue;
            }
            let path = entry.path();
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if canonical_protected
                .iter()
                .any(|protected| protected.starts_with(&canonical))
            {
                continue;
            }
            if directory_bytes(&path)? == 0 {
                continue;
            }
            let created = entry
                .metadata()
                .ok()
                .and_then(|m| m.created().ok())
                .or_else(|| entry.metadata().ok().and_then(|m| m.modified().ok()))
                .unwrap_or(SystemTime::UNIX_EPOCH);
            candidates.push((created, path));
        }
    }

    candidates.sort_by(|(a_time, a_path), (b_time, b_path)| {
        a_time.cmp(b_time).then_with(|| a_path.cmp(b_path))
    });

    let mut removed = Vec::new();
    for (_, path) in candidates {
        if used <= max_bytes {
            break;
        }
        let bytes = directory_bytes(&path)?;
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
