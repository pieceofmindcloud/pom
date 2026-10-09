//! Restart this process the way it started.
//!
//! The naive respawn - `Command::new(current_exe())` and nothing else - loses the
//! command-line arguments. A node started with `--advertise-addr` comes back
//! without it, and peers start keeping a temporary address that expires.
//!
//! The environment needs no care: `Command` inherits it by default, and that is
//! what preserves `POM_CONFIG` and `POM_NODE_KEY` of each node on a machine
//! that runs several.

use std::fs::OpenOptions;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

static ARGV: OnceLock<Vec<String>> = OnceLock::new();

/// Stores the startup `argv`. Called once, at the start of `main`.
pub fn remember_argv(args: Vec<String>) {
    let _ = ARGV.set(argv_rest(args));
}

/// The arguments without `argv[0]`. Separate from `remember_argv` because the
/// `OnceLock` only accepts one value per process and a test cannot undo it.
fn argv_rest(args: Vec<String>) -> Vec<String> {
    args.into_iter().skip(1).collect()
}

/// The startup arguments, without `argv[0]`.
pub fn original_argv() -> Vec<String> {
    ARGV.get().cloned().unwrap_or_default()
}

/// Starts a copy of this process and exits.
///
/// Only returns when the spawn fails - then the caller stays alive and reports,
/// instead of taking down a node that could not replace itself.
pub fn respawn_self() -> std::io::Error {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return e,
    };
    respawn_at(&exe)
}

/// Starts a copy through a path captured before an atomic swap.
///
/// On Unix, after a `rename` over the current executable, `current_exe()` may
/// point to the old inode with the `(deleted)` suffix. Whoever does the swap
/// must capture the path beforehand and pass it here.
pub(crate) fn respawn_at(exe: &std::path::Path) -> std::io::Error {
    let log_path = crate::config::project_root().join("pom.log");
    let mut log = match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(file) => file,
        Err(error) => return error,
    };
    if let Err(error) = writeln!(
        log,
        "\n--- respawn requested by process {} ---",
        std::process::id()
    ) {
        return error;
    }
    let stderr = match log.try_clone() {
        Ok(file) => file,
        Err(error) => return error,
    };

    let mut command = Command::new(exe);
    command
        .args(original_argv())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    // A controlled restart from the supervised Windows child must promote the
    // new process to supervisor. Otherwise the old parent sees exit code 0 and
    // leaves the replacement orphaned without crash monitoring.
    #[cfg(windows)]
    command.env_remove(crate::windows_status::SUPERVISED_CHILD_ENV);
    detach_from_parent_session(&mut command);

    match command.spawn() {
        Ok(_) => std::process::exit(0),
        Err(e) => e,
    }
}

/// The wizard is saved from a process that may be attached to a terminal or
/// supervisor. The child must survive the termination of that group, and the
/// descriptors must go to a file that allows diagnosing failures right at
/// startup.
#[cfg(unix)]
fn detach_from_parent_session(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach_from_parent_session(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The respawn must bring the process back the way it started. The old path
    /// passed along only --gateway-port, so a node with --advertise-addr came
    /// back without it and peers started keeping a temporary address.
    #[test]
    fn the_remembered_argv_comes_back_the_same_without_the_program() {
        remember_argv(vec![
            "./target/release/pom".into(),
            "--gateway-port".into(),
            "8090".into(),
            "--advertise-addr".into(),
            "[2804:d55::1]:42777".into(),
        ]);
        assert_eq!(
            original_argv(),
            vec![
                "--gateway-port".to_string(),
                "8090".to_string(),
                "--advertise-addr".to_string(),
                "[2804:d55::1]:42777".to_string(),
            ],
            "argv0 is the executable and comes back through current_exe, not through the list"
        );
    }

    /// A node started with no arguments at all comes back with no arguments at
    /// all - and the empty list must not become an empty argument on the command
    /// line.
    #[test]
    fn an_argv_with_only_the_program_yields_no_argument() {
        assert!(argv_rest(vec!["./target/release/pom".to_string()]).is_empty());
        assert_eq!(
            argv_rest(vec!["pom".into(), "--no-discovery".into()]),
            vec!["--no-discovery".to_string()]
        );
    }
}
