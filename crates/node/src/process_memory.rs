//! Resident memory of a process and everything it started, for the
//! Background services screen (`memory_bytes` in `GET /api/ui/plugins/ui`).
//!
//! Linux reads `/proc`; other platforms answer `None` and the field is
//! omitted.
use std::path::Path;

/// Sum of `VmRSS` over `root` and its descendants under `proc_dir`
/// (`/proc` in production). `None` when `root` is not there.
pub(crate) fn tree_rss(root: u32, proc_dir: &Path) -> Option<u64> {
    let mut parents = Vec::new();
    for entry in std::fs::read_dir(proc_dir).ok()?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if let Some(ppid) = parent_of(&entry.path()) {
            parents.push((pid, ppid));
        }
    }
    if !parents.iter().any(|(pid, _)| *pid == root) {
        return None;
    }
    let mut tree = vec![root];
    let mut index = 0;
    while index < tree.len() {
        let parent = tree[index];
        tree.extend(parents.iter().filter(|(_, ppid)| *ppid == parent).map(|(pid, _)| *pid));
        index += 1;
    }
    Some(
        tree.iter()
            .filter_map(|pid| rss_kb(&proc_dir.join(pid.to_string())))
            .sum::<u64>()
            * 1024,
    )
}

/// The parent pid from `<pid>/stat`: the field after the `(comm)`, which may
/// itself hold spaces and parentheses.
fn parent_of(dir: &Path) -> Option<u32> {
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn rss_kb(dir: &Path) -> Option<u64> {
    let status = std::fs::read_to_string(dir.join("status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Resident memory of the process tree rooted at `pid`, where measurable.
pub(crate) fn resident_tree_bytes(pid: u32) -> Option<u64> {
    if cfg!(target_os = "linux") {
        tree_rss(pid, Path::new("/proc"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fake_proc(entries: &[(u32, u32, u64)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pom-proc-{}-{}",
            std::process::id(),
            entries.len()
        ));
        let _ = fs::remove_dir_all(&dir);
        for (pid, ppid, rss_kb) in entries {
            let p = dir.join(pid.to_string());
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("stat"), format!("{pid} (my (odd) name) S {ppid} 1 1 0")).unwrap();
            fs::write(p.join("status"), format!("Name:\tx\nVmRSS:\t  {rss_kb} kB\nThreads:\t1\n")).unwrap();
        }
        fs::create_dir_all(dir.join("self")).unwrap();
        dir
    }

    #[test]
    fn plugin_memory_sums_the_host_and_its_descendants() {
        // 10 is the host; 11 and 12 its children; 13 a grandchild; 20 unrelated.
        let dir = fake_proc(&[(10, 1, 100), (11, 10, 200), (12, 10, 300), (13, 12, 400), (20, 1, 9999)]);
        assert_eq!(tree_rss(10, &dir), Some((100 + 200 + 300 + 400) * 1024));
        assert_eq!(tree_rss(12, &dir), Some((300 + 400) * 1024));
        assert_eq!(tree_rss(99, &dir), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
