//! `pom-plugin-host --apply-update <plan.json>`: swaps a downloaded POM
//! release in place of the running one and starts it again.
//!
//! A running program cannot always replace itself (Windows locks a running
//! `.exe`, a macOS bundle or an AppImage is one file the process is using), so
//! the node stages the new files, writes a plan and starts this host from a
//! copy outside the install, detached, then exits. The host waits for the node
//! to be gone, moves each target aside into the backup directory, moves the
//! staged entry into its place and launches the program of the plan. When a
//! move fails, every entry already swapped is put back and the old program is
//! launched instead, so a failed update never leaves the machine without POM.
use serde::Deserialize;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long the old process gets to exit before the update gives up.
const EXIT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Deserialize)]
pub struct UpdatePlan {
    /// The node process to wait for.
    pub wait_pid: u32,
    /// Each staged entry and the path it replaces (a file or a directory).
    pub replacements: Vec<Replacement>,
    /// Where the replaced entries are kept until the swap succeeds.
    pub backup_dir: PathBuf,
    pub relaunch: Relaunch,
    /// The node's log, which this host appends its steps to.
    pub log: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct Replacement {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct Relaunch {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

pub fn run(plan_path: &Path) -> io::Result<()> {
    let plan: UpdatePlan = serde_json::from_slice(&fs::read(plan_path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&plan.log)?;
    let mut note = |message: &str| {
        let _ = writeln!(log, "pom-plugin-host update: {message}");
    };
    note(&format!("waiting for process {} to exit", plan.wait_pid));
    if !wait_for_exit(plan.wait_pid, EXIT_TIMEOUT) {
        note("the old process did not exit; nothing was replaced");
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the old process did not exit",
        ));
    }
    match swap(&plan) {
        Ok(()) => {
            note("new release in place; starting it");
            let _ = fs::remove_dir_all(&plan.backup_dir);
        }
        Err(error) => note(&format!(
            "update failed, the previous release was restored: {error}"
        )),
    }
    launch(&plan.relaunch).inspect_err(|error| note(&format!("could not start POM: {error}")))
}

/// Replaces every target, or none: on the first failure the entries already
/// swapped go back where they were.
fn swap(plan: &UpdatePlan) -> io::Result<()> {
    fs::create_dir_all(&plan.backup_dir)?;
    let mut done: Vec<(&Replacement, Option<PathBuf>)> = Vec::new();
    for (index, replacement) in plan.replacements.iter().enumerate() {
        let backup = plan.backup_dir.join(index.to_string());
        let result = (|| {
            let moved_aside = if fs::symlink_metadata(&replacement.to).is_ok() {
                move_entry(&replacement.to, &backup)?;
                Some(backup.clone())
            } else {
                None
            };
            if let Err(error) = move_entry(&replacement.from, &replacement.to) {
                if let Some(backup) = &moved_aside {
                    let _ = move_entry(backup, &replacement.to);
                }
                return Err(error);
            }
            Ok(moved_aside)
        })();
        match result {
            Ok(moved_aside) => done.push((replacement, moved_aside)),
            Err(error) => {
                for (replacement, moved_aside) in done.into_iter().rev() {
                    let _ = remove_entry(&replacement.to);
                    if let Some(backup) = moved_aside {
                        let _ = move_entry(&backup, &replacement.to);
                    }
                }
                return Err(error);
            }
        }
    }
    Ok(())
}

/// A rename, or a copy and delete when the two paths are on different disks.
fn move_entry(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_entry(from, to)?;
    remove_entry(from)
}

fn copy_entry(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    if metadata.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_entry(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(());
    }
    #[cfg(unix)]
    if metadata.file_type().is_symlink() {
        return std::os::unix::fs::symlink(fs::read_link(from)?, to);
    }
    fs::copy(from, to).map(|_| ())
}

fn remove_entry(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let started = Instant::now();
    while process_alive(pid) {
        if started.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Windows releases the executable's lock a moment after the process ends.
    std::thread::sleep(Duration::from_millis(500));
    true
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("tasklist")
        .args(["/NH", "/FI", &format!("PID eq {pid}")])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

/// Starts POM detached from this host, which exits right after.
fn launch(relaunch: &Relaunch) -> io::Result<()> {
    let mut command = Command::new(&relaunch.program);
    command
        .args(&relaunch.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cwd) = &relaunch.cwd {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(root: &Path, replacements: Vec<Replacement>) -> UpdatePlan {
        UpdatePlan {
            wait_pid: 0,
            replacements,
            backup_dir: root.join("backup"),
            relaunch: Relaunch {
                program: root.join("pom"),
                args: vec![],
                cwd: None,
            },
            log: root.join("pom.log"),
        }
    }

    #[test]
    fn swaps_files_and_directories_and_keeps_what_the_package_does_not_carry() {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("install");
        let staged = root.path().join("staged");
        fs::create_dir_all(install.join("apps/frontend/dist")).unwrap();
        fs::write(install.join("pom"), "old").unwrap();
        fs::write(install.join("apps/frontend/dist/index.html"), "old ui").unwrap();
        fs::write(install.join("pom.json"), "config").unwrap();
        fs::create_dir_all(staged.join("apps/frontend/dist")).unwrap();
        fs::write(staged.join("pom"), "new").unwrap();
        fs::write(staged.join("apps/frontend/dist/app.js"), "new ui").unwrap();
        let plan = plan(
            root.path(),
            vec![
                Replacement {
                    from: staged.join("pom"),
                    to: install.join("pom"),
                },
                Replacement {
                    from: staged.join("apps"),
                    to: install.join("apps"),
                },
            ],
        );
        swap(&plan).unwrap();
        assert_eq!(fs::read_to_string(install.join("pom")).unwrap(), "new");
        assert!(install.join("apps/frontend/dist/app.js").is_file());
        assert!(!install.join("apps/frontend/dist/index.html").exists());
        assert_eq!(
            fs::read_to_string(install.join("pom.json")).unwrap(),
            "config"
        );
    }

    #[test]
    fn a_failed_move_puts_every_swapped_entry_back() {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("install");
        let staged = root.path().join("staged");
        fs::create_dir_all(&install).unwrap();
        fs::create_dir_all(&staged).unwrap();
        fs::write(install.join("pom"), "old").unwrap();
        fs::write(staged.join("pom"), "new").unwrap();
        let plan = plan(
            root.path(),
            vec![
                Replacement {
                    from: staged.join("pom"),
                    to: install.join("pom"),
                },
                Replacement {
                    from: staged.join("missing"),
                    to: install.join("pom-plugin-host"),
                },
            ],
        );
        assert!(swap(&plan).is_err());
        assert_eq!(fs::read_to_string(install.join("pom")).unwrap(), "old");
    }

    #[test]
    fn an_exited_process_is_not_waited_for() {
        let mut child = Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) {
                vec!["/C", "exit"]
            } else {
                vec![]
            })
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(wait_for_exit(pid, Duration::from_secs(5)));
    }
}
