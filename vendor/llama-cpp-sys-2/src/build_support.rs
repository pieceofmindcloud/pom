use std::path::{Component, Path, PathBuf};

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

pub(crate) fn clone_destination(
    manifest_dir: &Path,
    source: &Path,
    symlink_target: Option<&Path>,
    is_windows: bool,
) -> PathBuf {
    if is_windows {
        return source.to_path_buf();
    }

    let Some(target) = symlink_target else {
        return source.to_path_buf();
    };
    let base = source.parent().unwrap_or(manifest_dir);
    normalize_path(&base.join(target))
}

pub(crate) fn destination_can_be_replaced(
    destination: &Path,
    source: &Path,
    is_windows: bool,
) -> bool {
    is_windows && destination == source
}

pub(crate) fn source_can_be_used_directly(
    has_ggml: bool,
    is_symlink: bool,
    is_windows: bool,
) -> bool {
    has_ggml && !(is_windows && is_symlink)
}

#[cfg(test)]
mod tests {
    use super::{clone_destination, destination_can_be_replaced, source_can_be_used_directly};
    use std::path::Path;

    #[test]
    fn windows_installs_clone_at_source_instead_of_following_symlink() {
        let manifest_dir = Path::new(r"D:\a\rdma-p2p\rdma-p2p\vendor\llama-cpp-sys-2");
        let source = manifest_dir.join("llama.cpp");

        assert_eq!(
            clone_destination(
                manifest_dir,
                &source,
                Some(Path::new(r"..\..\llama.cpp")),
                true,
            ),
            source
        );
    }

    #[test]
    fn unix_installs_clone_into_relative_symlink_target() {
        let manifest_dir = Path::new("/worktree/vendor/llama-cpp-sys-2");
        let source = manifest_dir.join("llama.cpp");

        assert_eq!(
            clone_destination(
                manifest_dir,
                &source,
                Some(Path::new("../../llama.cpp")),
                false,
            ),
            Path::new("/worktree/llama.cpp")
        );
    }

    #[test]
    fn windows_allows_replacing_placeholder_at_source() {
        let manifest_dir = Path::new(r"D:\a\rdma-p2p\rdma-p2p\vendor\llama-cpp-sys-2");
        let source = manifest_dir.join("llama.cpp");

        assert!(destination_can_be_replaced(&source, &source, true));
        assert!(!destination_can_be_replaced(
            &manifest_dir.join(r"..\..\llama.cpp"),
            &source,
            true
        ));
        assert!(!destination_can_be_replaced(&source, &source, false));
    }

    #[test]
    fn windows_does_not_reuse_a_valid_symlink_as_the_build_root() {
        assert!(!source_can_be_used_directly(true, true, true));
        assert!(source_can_be_used_directly(true, false, true));
        assert!(source_can_be_used_directly(true, true, false));
    }
}
