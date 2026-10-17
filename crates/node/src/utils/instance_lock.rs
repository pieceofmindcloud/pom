//! One running node per project root.
//!
//! Two processes on the same root share the identity key and `pom.json`, so the
//! second one is never a second node. The desktop builds move a saved port that
//! another program holds; without this lock a leftover POM would look like such
//! a program and a duplicate node would start beside it.
//!
//! The lock is an advisory `flock`/`LockFileEx` on a file in the project root,
//! released by the kernel when the process ends. A respawn starts the new
//! process just before the old one exits, so acquiring waits a few seconds
//! before deciding that another instance is running. The holder writes its
//! interface port into the file so a second launch can point at it.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a starting process waits for the previous holder to exit.
pub(crate) const RESPAWN_GRACE: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Held for the life of the process; dropping it releases the lock.
#[derive(Debug)]
pub(crate) struct InstanceLock {
    file: File,
}

#[derive(Debug)]
pub(crate) enum Acquired {
    Lock(InstanceLock),
    /// Another process holds the lock. The port is the interface port it
    /// recorded, when it got that far and the platform lets the file be read.
    HeldBy { gateway_port: Option<u16> },
}

pub(crate) fn lock_path() -> PathBuf {
    crate::config::project_root().join(format!(".{}-instance.lock", crate::brand::slug()))
}

pub(crate) fn acquire(wait: Duration) -> std::io::Result<Acquired> {
    acquire_at(&lock_path(), wait)
}

fn acquire_at(path: &Path, wait: Duration) -> std::io::Result<Acquired> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Acquired::Lock(InstanceLock { file })),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(TryLockError::WouldBlock) => {
                return Ok(Acquired::HeldBy {
                    gateway_port: recorded_port(&file),
                })
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
}

fn recorded_port(mut file: &File) -> Option<u16> {
    let mut content = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut content).ok()?;
    content.trim().parse().ok()
}

impl InstanceLock {
    /// Records the interface port for a second launch to find.
    pub(crate) fn record_gateway_port(&mut self, port: u16) -> std::io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        writeln!(self.file, "{port}")?;
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_sees_the_first_one_and_its_port() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.lock");
        let Acquired::Lock(mut first) = acquire_at(&path, Duration::ZERO).unwrap() else {
            panic!("the first acquire takes the lock");
        };
        first.record_gateway_port(8081).unwrap();

        match acquire_at(&path, Duration::ZERO).unwrap() {
            Acquired::HeldBy { gateway_port } => {
                #[cfg(unix)]
                assert_eq!(gateway_port, Some(8081));
                #[cfg(not(unix))]
                let _ = gateway_port;
            }
            Acquired::Lock(_) => panic!("the lock is already held"),
        }

        drop(first);
        assert!(matches!(
            acquire_at(&path, Duration::ZERO).unwrap(),
            Acquired::Lock(_)
        ));
    }

    #[test]
    fn acquiring_waits_for_a_holder_that_exits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.lock");
        let Acquired::Lock(first) = acquire_at(&path, Duration::ZERO).unwrap() else {
            panic!("the first acquire takes the lock");
        };
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(first);
        });
        assert!(matches!(
            acquire_at(&path, Duration::from_secs(5)).unwrap(),
            Acquired::Lock(_)
        ));
        releaser.join().unwrap();
    }
}
