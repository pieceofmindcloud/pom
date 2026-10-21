//! Puts this machine's local CA (`web_ca.rs`) in the operating system's trust
//! store, so browsers, curl and the harnesses accept `https://` to this node.
//!
//! Every system asks the person first, and that is the point: a trusted CA can
//! vouch for sites, so no program may add one silently. What each one shows:
//!
//! - **macOS**: `security add-trusted-cert` into the user's login keychain. The
//!   system asks for the user's password to change the trust settings, so the
//!   app never installs it at start: only the Preferences button does.
//! - **Windows**: `certutil -user -addstore Root`, the current user's root
//!   store. Windows shows its own "install this certificate?" warning; no UAC.
//!   As on macOS, the app never installs it at start: only Preferences does.
//! - **Linux**: the NSS databases of Chrome (`~/.pki/nssdb`) and Firefox (each
//!   profile) belong to the user and take the CA with `certutil` without
//!   asking, so only this user's browsers trust it. The system store, which
//!   curl, Python and most tools read, is shared by every user of the machine:
//!   it is touched only with `--trust-ca-system-wide`, and then the copy and
//!   `update-ca-certificates` (or `update-ca-trust`) run under `pkexec`, which
//!   shows the desktop's password dialog. Without the flag nothing asks for
//!   root, and the desktop window accepts the node's own certificate in its
//!   view alone (`linux_window.rs`).
//!
//! A refusal is remembered (`.pom-ca.declined`, with the CA's fingerprint): the
//! desktop app asks once at start and never again for the same CA. The
//! interface keeps a button to try again and the `.crt` to install by hand.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::web_ca::LocalCa;

/// Whether this machine trusts the CA.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    Trusted,
    NotTrusted,
    /// No way to tell here (an unsupported system, or a missing tool).
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Installed,
    AlreadyTrusted,
    /// The person refused, or closed the dialog.
    Declined,
    /// This system, or this session, has no way to install it.
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub outcome: Outcome,
    /// What happened, for the operator; never a secret.
    pub detail: String,
}

impl InstallReport {
    fn new(outcome: Outcome, detail: impl Into<String>) -> Self {
        Self {
            outcome,
            detail: detail.into(),
        }
    }
}

/// `--trust-ca-system-wide`: whether the Linux system store, shared by every
/// user of the machine, is part of trusting the CA. Off by default, so another
/// user's tools never trust this user's node and nobody is asked for root.
static SYSTEM_WIDE: AtomicBool = AtomicBool::new(false);

pub fn set_system_wide(on: bool) {
    SYSTEM_WIDE.store(on, Ordering::Relaxed);
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn system_wide() -> bool {
    SYSTEM_WIDE.load(Ordering::Relaxed)
}

/// Whether this build can install the CA on this system at all.
pub fn supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux"))
}

fn declined_path() -> PathBuf {
    crate::web_ca::dir().join(format!(".{}-ca.declined", crate::brand::slug()))
}

/// Whether the person refused this very CA before.
pub fn declined(ca: &LocalCa) -> bool {
    std::fs::read_to_string(declined_path())
        .map(|text| text.trim() == ca.fingerprint_sha256())
        .unwrap_or(false)
}

fn remember(ca: &LocalCa, report: &InstallReport) {
    match report.outcome {
        Outcome::Declined => {
            let _ = std::fs::write(declined_path(), ca.fingerprint_sha256());
        }
        Outcome::Installed | Outcome::AlreadyTrusted => {
            let _ = std::fs::remove_file(declined_path());
        }
        Outcome::Unavailable => {}
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
/// The name the CA goes by in the stores: the product, plus the start of its
/// fingerprint so two machines' CAs never share one.
fn nickname(ca: &LocalCa) -> String {
    let short: String = ca.fingerprint_sha256().replace(':', "").chars().take(8).collect();
    format!("{} local CA {}", crate::brand::name(), short.to_ascii_lowercase())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn file_stem(ca: &LocalCa) -> String {
    let short: String = ca.fingerprint_sha256().replace(':', "").chars().take(8).collect();
    format!("{}-local-ca-{}", crate::brand::slug(), short.to_ascii_lowercase())
}

/// Runs a tool and says whether it succeeded, with its output for the report.
fn run(command: &mut Command) -> (bool, String) {
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window flashing over the app.
        command.creation_flags(0x0800_0000);
    }
    match command.output() {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if text.is_empty() {
                text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            }
            (output.status.success(), text)
        }
        Err(error) => (false, error.to_string()),
    }
}

/// Whether this machine trusts the CA now.
pub fn state(ca: &LocalCa) -> TrustState {
    platform::state(ca, &crate::web_ca::ca_cert_path())
}

/// Installs the CA, asking the person as the system requires. Blocks while the
/// system's dialog is open, so it runs off the async runtime.
pub fn install(ca: &LocalCa) -> InstallReport {
    if !supported() {
        return InstallReport::new(Outcome::Unavailable, "this system has no supported trust store");
    }
    let report = if state(ca) == TrustState::Trusted {
        InstallReport::new(Outcome::AlreadyTrusted, "this machine already trusts the CA")
    } else {
        platform::install(ca, &crate::web_ca::ca_cert_path())
    };
    remember(ca, &report);
    report
}

/// What the Linux desktop app does at start (macOS and Windows do not: their
/// stores always show a dialog): install the CA unless this machine
/// already trusts it or the person refused this CA before. Never on a server:
/// there is nobody at the screen to answer the system's dialog.
#[cfg_attr(not(all(target_os = "linux", feature = "desktop")), allow(dead_code))]
pub fn auto_install() {
    let ca = match LocalCa::load_or_create() {
        Ok(ca) => ca,
        Err(error) => {
            tracing::warn!("local CA unavailable, the interface stays on an untrusted certificate: {error:#}");
            return;
        }
    };
    if declined(&ca) || state(&ca) == TrustState::Trusted {
        return;
    }
    let report = install(&ca);
    match report.outcome {
        Outcome::Installed => tracing::info!("local CA installed in the system trust store"),
        Outcome::AlreadyTrusted => {}
        Outcome::Declined => tracing::info!(
            "the local CA was not installed ({}); the interface offers it again under Preferences",
            report.detail
        ),
        Outcome::Unavailable => tracing::info!("local CA not installed: {}", report.detail),
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    const SECURITY: &str = "/usr/bin/security";

    fn login_keychain() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Keychains/login.keychain-db"))
    }

    pub fn state(_ca: &LocalCa, cert: &Path) -> TrustState {
        // Evaluating the root itself succeeds only when it is a trusted anchor.
        let (ok, _) = run(Command::new(SECURITY).arg("verify-cert").arg("-c").arg(cert).arg("-L").arg("-q"));
        if ok {
            TrustState::Trusted
        } else {
            TrustState::NotTrusted
        }
    }

    pub fn install(_ca: &LocalCa, cert: &Path) -> InstallReport {
        let Some(keychain) = login_keychain() else {
            return InstallReport::new(Outcome::Unavailable, "no HOME to find the login keychain");
        };
        let (ok, detail) = run(
            Command::new(SECURITY)
                .args(["add-trusted-cert", "-r", "trustRoot", "-k"])
                .arg(&keychain)
                .arg(cert),
        );
        if ok {
            InstallReport::new(Outcome::Installed, "added to the login keychain as a trusted root")
        } else {
            InstallReport::new(Outcome::Declined, detail)
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;

    fn certutil() -> PathBuf {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        PathBuf::from(root).join("System32").join("certutil.exe")
    }

    /// `certutil` finds a certificate by its serial number.
    fn serial(ca: &LocalCa) -> Option<String> {
        let (_, cert) = x509_parser::parse_x509_certificate(ca.cert_der()).ok()?;
        Some(hex::encode(cert.raw_serial()))
    }

    pub fn state(ca: &LocalCa, _cert: &Path) -> TrustState {
        let Some(serial) = serial(ca) else {
            return TrustState::Unknown;
        };
        let (ok, _) = run(Command::new(certutil()).args(["-user", "-store", "Root"]).arg(serial));
        if ok {
            TrustState::Trusted
        } else {
            TrustState::NotTrusted
        }
    }

    pub fn install(_ca: &LocalCa, cert: &Path) -> InstallReport {
        // Windows itself asks "Do you want to install this certificate?".
        let (ok, detail) = run(Command::new(certutil()).args(["-user", "-addstore", "Root"]).arg(cert));
        if ok {
            InstallReport::new(Outcome::Installed, "added to the current user's root store")
        } else {
            InstallReport::new(Outcome::Declined, detail)
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    /// Where the system store takes a new anchor, and the command that
    /// rebuilds it: Debian and Ubuntu, then Fedora and Arch.
    fn system_store() -> Option<(PathBuf, &'static str, &'static str)> {
        [
            ("/usr/local/share/ca-certificates", "crt", "update-ca-certificates"),
            ("/etc/pki/ca-trust/source/anchors", "pem", "update-ca-trust"),
            ("/etc/ca-certificates/trust-source/anchors", "crt", "update-ca-trust"),
        ]
        .into_iter()
        .find(|(dir, _, tool)| Path::new(dir).is_dir() && find_tool(tool).is_some())
        .map(|(dir, ext, tool)| (PathBuf::from(dir), ext, tool))
    }

    fn find_tool(name: &str) -> Option<PathBuf> {
        ["/usr/sbin", "/usr/bin", "/sbin", "/bin", "/usr/local/bin"]
            .iter()
            .map(|dir| Path::new(dir).join(name))
            .find(|path| path.is_file())
    }

    fn system_file(ca: &LocalCa) -> Option<PathBuf> {
        system_store().map(|(dir, ext, _)| dir.join(format!("{}.{ext}", file_stem(ca))))
    }

    fn system_trusts(ca: &LocalCa) -> bool {
        system_file(ca)
            .and_then(|path| std::fs::read_to_string(path).ok())
            .is_some_and(|text| text.trim() == ca.cert_pem().trim())
    }

    /// Chrome's user database and every Firefox profile, the snap one included.
    fn nss_databases() -> Vec<PathBuf> {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return vec![];
        };
        let mut dbs = vec![home.join(".pki/nssdb")];
        for profiles in [
            home.join(".mozilla/firefox"),
            home.join("snap/firefox/common/.mozilla/firefox"),
        ] {
            if let Ok(entries) = std::fs::read_dir(&profiles) {
                for entry in entries.flatten() {
                    if entry.path().join("cert9.db").is_file() {
                        dbs.push(entry.path());
                    }
                }
            }
        }
        dbs
    }

    fn nss_trusts(certutil: &Path, db: &Path, nick: &str) -> bool {
        let (ok, _) = run(
            Command::new(certutil)
                .arg("-L")
                .arg("-d")
                .arg(format!("sql:{}", db.display()))
                .arg("-n")
                .arg(nick),
        );
        ok
    }

    pub fn state(ca: &LocalCa, _cert: &Path) -> TrustState {
        let system = system_store().filter(|_| system_wide()).map(|_| system_trusts(ca));
        let nick = nickname(ca);
        let browsers = find_tool("certutil").map(|certutil| {
            nss_databases()
                .iter()
                .filter(|db| db.exists())
                .all(|db| nss_trusts(&certutil, db, &nick))
        });
        match (system, browsers) {
            (None, None) => TrustState::Unknown,
            (Some(false), _) | (_, Some(false)) => TrustState::NotTrusted,
            _ => TrustState::Trusted,
        }
    }

    pub fn install(ca: &LocalCa, cert: &Path) -> InstallReport {
        let mut done = Vec::new();
        let nick = nickname(ca);
        if let Some(certutil) = find_tool("certutil") {
            for db in nss_databases() {
                if !db.exists() {
                    // Chrome's database may not exist before its first run.
                    if std::fs::create_dir_all(&db).is_err() {
                        continue;
                    }
                    let _ = run(
                        Command::new(&certutil)
                            .arg("-N")
                            .arg("-d")
                            .arg(format!("sql:{}", db.display()))
                            .arg("--empty-password"),
                    );
                }
                if nss_trusts(&certutil, &db, &nick) {
                    continue;
                }
                let (ok, _) = run(
                    Command::new(&certutil)
                        .arg("-A")
                        .arg("-d")
                        .arg(format!("sql:{}", db.display()))
                        .args(["-t", "C,,"])
                        .arg("-n")
                        .arg(&nick)
                        .arg("-i")
                        .arg(cert),
                );
                if ok {
                    done.push("browsers");
                }
            }
        }

        if let Some((_, _, tool)) = system_store().filter(|_| system_wide() && !system_trusts(ca)) {
            let (Some(pkexec), Some(target)) = (find_tool("pkexec"), system_file(ca)) else {
                return finish(done, "no pkexec to ask for the system store");
            };
            // The paths travel as arguments, never inside the script.
            let (ok, detail) = run(
                Command::new(pkexec)
                    .args(["/bin/sh", "-c", "install -m 0644 \"$1\" \"$2\" && \"$3\""])
                    .arg("sh")
                    .arg(cert)
                    .arg(&target)
                    .arg(find_tool(tool).unwrap_or_else(|| PathBuf::from(tool))),
            );
            if ok {
                done.push("system");
            } else {
                return if done.is_empty() {
                    InstallReport::new(Outcome::Declined, detail)
                } else {
                    InstallReport::new(
                        Outcome::Declined,
                        format!("browsers trust it, the system store was refused: {detail}"),
                    )
                };
            }
        }
        finish(done, "no trust store found on this system")
    }

    fn finish(done: Vec<&str>, nothing: &str) -> InstallReport {
        if done.is_empty() {
            InstallReport::new(Outcome::Unavailable, nothing)
        } else {
            InstallReport::new(Outcome::Installed, format!("added to the {} store", done.join(" and ")))
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod platform {
    use super::*;

    pub fn state(_ca: &LocalCa, _cert: &Path) -> TrustState {
        TrustState::Unknown
    }

    pub fn install(_ca: &LocalCa, _cert: &Path) -> InstallReport {
        InstallReport::new(Outcome::Unavailable, "this system has no supported trust store")
    }
}
