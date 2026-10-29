//! Everything a plugin starts ends with its host.
//!
//! Plugins start processes of their own (node, npm, code-server, herdr, a
//! dashboard), and those start more: agent panes, headless browsers, daemons
//! that call `setsid` and double-fork on purpose. Neither the host's process
//! group nor the plugin's own `shutdown` reaches all of them, so closing a
//! plugin used to leave them running, reparented to init.
//!
//! On Linux the host is a child subreaper (`adopt_orphans`): an orphan
//! anywhere below it is reparented to the host instead of init, so the host's
//! tree is complete while it runs. When the host leaves, after the plugin's
//! `shutdown`, `end_descendants` sends SIGTERM to every live descendant, gives
//! them `grace` to exit, SIGKILLs the survivors and reaps them. macOS has no
//! subreaper: the same walk ends whatever is still attached to the host.
//! Windows needs neither: POM puts each host in a kill-on-close Job Object
//! (`crates/node/src/process_tree.rs`).
//!
//! Only the plugin mode calls these; `--apply-update` must leave the POM it
//! starts running.

use std::time::Duration;

/// Makes the host the reaper of every orphan below it (Linux only).
#[cfg(target_os = "linux")]
pub fn adopt_orphans() {
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn adopt_orphans() {}

/// Ends every live process below the host: SIGTERM, `grace`, then SIGKILL.
/// Returns how many processes were asked to stop.
#[cfg(unix)]
pub fn end_descendants(grace: Duration) -> usize {
    use std::time::Instant;

    let me = std::process::id() as i32;
    let first = descendants(me);
    for pid in &first {
        unsafe { libc::kill(*pid, libc::SIGTERM) };
    }
    let deadline = Instant::now() + grace;
    loop {
        reap();
        let alive = descendants(me);
        if alive.is_empty() {
            // The last ones may have exited after the reap above.
            reap();
            return first.len();
        }
        if Instant::now() >= deadline {
            for pid in alive {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    while !descendants(me).is_empty() && Instant::now() < deadline {
        reap();
        std::thread::sleep(Duration::from_millis(20));
    }
    reap();
    first.len()
}

#[cfg(not(unix))]
pub fn end_descendants(_grace: Duration) -> usize {
    0
}

/// Collects the exit status of every child that already ended. Run only when
/// nothing else waits on a child: it would take their status.
#[cfg(unix)]
fn reap() {
    loop {
        let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if pid <= 0 {
            return;
        }
    }
}

/// Live (non-zombie) processes below `root`, parents before children.
#[cfg(unix)]
pub fn descendants(root: i32) -> Vec<i32> {
    let table = process_table();
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid, zombie) in &table {
            if *ppid == parent && *pid != root && !found.contains(pid) {
                frontier.push(*pid);
                if !zombie {
                    found.push(*pid);
                }
            }
        }
    }
    found
}

/// `(pid, parent pid, is a zombie)` of every process, from `/proc`.
#[cfg(target_os = "linux")]
fn process_table() -> Vec<(i32, i32, bool)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            parse_stat(pid, &stat)
        })
        .collect()
}

/// `/proc/<pid>/stat` is `pid (comm) state ppid ...`; `comm` may hold spaces
/// and parentheses, so the fields are read after its last `)`.
#[cfg(target_os = "linux")]
fn parse_stat(pid: i32, stat: &str) -> Option<(i32, i32, bool)> {
    let mut fields = stat.get(stat.rfind(')')? + 1..)?.split_whitespace();
    let state = fields.next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((pid, ppid, state == "Z"))
}

/// `(pid, parent pid, is a zombie)` of every process, from `ps`.
#[cfg(all(unix, not(target_os = "linux")))]
fn process_table() -> Vec<(i32, i32, bool)> {
    let Ok(child) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,stat="])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let own = child.id() as i32;
    let Ok(output) = child.wait_with_output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<i32>().ok()?;
            let ppid = fields.next()?.parse::<i32>().ok()?;
            let zombie = fields.next().is_some_and(|stat| stat.starts_with('Z'));
            (pid != own).then_some((pid, ppid, zombie))
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;

    #[cfg(target_os = "linux")]
    #[test]
    fn stat_fields_are_read_after_the_command_name() {
        assert_eq!(
            parse_stat(42, "42 (a) b (c)) S 7 42 42 0"),
            Some((42, 7, false))
        );
        assert_eq!(parse_stat(9, "9 (node) Z 1 9"), Some((9, 1, true)));
        assert_eq!(parse_stat(9, "garbage"), None);
    }

    /// A child that detaches a grandchild into its own session and exits: the
    /// grandchild is orphaned, and still ends with the host. Runs in a fresh
    /// process so the test's subreaper and SIGTERMs touch nothing else.
    #[test]
    fn a_detached_grandchild_ends_with_the_host() {
        if std::env::var_os("POM_TREE_TEST_CHILD").is_some() {
            adopt_orphans();
            let marker = std::env::var("POM_TREE_TEST_CHILD").unwrap();
            // `setsid sleep` in the background, then the shell exits.
            let status = Command::new("sh")
                .args([
                    "-c",
                    &format!("(setsid sh -c 'echo $$ > {marker}; exec sleep 60' &) ; exit 0"),
                ])
                .status()
                .unwrap();
            assert!(status.success());
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let grandchild = loop {
                if let Some(pid) = std::fs::read_to_string(&marker)
                    .ok()
                    .and_then(|text| text.trim().parse::<i32>().ok())
                {
                    break pid;
                }
                assert!(std::time::Instant::now() < deadline, "no grandchild");
                std::thread::sleep(Duration::from_millis(20));
            };
            if cfg!(target_os = "linux") {
                assert!(descendants(std::process::id() as i32).contains(&grandchild));
                assert!(end_descendants(Duration::from_secs(2)) >= 1);
                assert_eq!(
                    unsafe { libc::kill(grandchild, 0) },
                    -1,
                    "grandchild still alive"
                );
            } else {
                unsafe { libc::kill(grandchild, libc::SIGKILL) };
            }
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tree::tests::a_detached_grandchild_ends_with_the_host",
                "--nocapture",
            ])
            .env("POM_TREE_TEST_CHILD", dir.path().join("grandchild.pid"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
