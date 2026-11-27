//! The `sandbox` section of a plugin's release manifest: what a sandboxed
//! plugin asks for (network, listening, GPU, extra read-only paths). Design:
//! `docs/superpowers/specs/2026-11-27-plugin-sandbox-design.md`.
//!
//! A manifest without the section is a legacy plugin, run without a sandbox
//! as before (`Option<PluginSandbox>` is `None`). Every field has a default
//! and unknown fields and values are accepted, so a release written for a
//! newer node still installs on an older one, which grants only what it
//! knows: an unknown value is read as the narrowest one.
use serde::{Deserialize, Serialize};

/// At most this many extra read-only paths.
pub const MAX_SANDBOX_READ_PATHS: usize = 16;
/// At most this many bytes per path.
pub const MAX_SANDBOX_PATH_BYTES: usize = 1024;

/// Where the plugin's processes may connect to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", from = "String")]
pub enum SandboxNetwork {
    /// The node's gateway port and the plugin's own loopback servers.
    #[default]
    Loopback,
    /// Any address; on the Linux baseline this also reaches every loopback
    /// port of the machine (`docs/reports/2026-11-27-plugin-sandbox-linux-spike.md`).
    Internet,
}

/// Where the plugin's servers may listen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", from = "String")]
pub enum SandboxListen {
    #[default]
    Loopback,
    /// Every interface, so other machines reach it (Qdrant).
    Network,
}

/// Unknown values read as the narrowest permission.
impl From<String> for SandboxNetwork {
    fn from(value: String) -> Self {
        match value.as_str() {
            "internet" => Self::Internet,
            _ => Self::Loopback,
        }
    }
}

impl From<String> for SandboxListen {
    fn from(value: String) -> Self {
        match value.as_str() {
            "network" => Self::Network,
            _ => Self::Loopback,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSandbox {
    #[serde(default)]
    pub network: SandboxNetwork,
    #[serde(default)]
    pub listen: SandboxListen,
    /// GPU device access (NVIDIA, AMD, Intel, Metal).
    #[serde(default)]
    pub gpu: bool,
    /// Extra read-only paths: absolute, or under the user's home as `~/...`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_paths: Vec<String>,
}

impl PluginSandbox {
    /// The limits a node enforces before it accepts a manifest.
    pub fn validate(&self) -> Result<(), String> {
        if self.read_paths.len() > MAX_SANDBOX_READ_PATHS {
            return Err(format!(
                "sandbox.read_paths lists more than {MAX_SANDBOX_READ_PATHS} paths"
            ));
        }
        for path in &self.read_paths {
            validate_read_path(path)?;
        }
        Ok(())
    }

    /// Whether `self` asks for something `granted` does not give.
    fn asks_more_than(&self, granted: &PluginSandbox) -> bool {
        self.network > granted.network
            || self.listen > granted.listen
            || (self.gpu && !granted.gpu)
            || self
                .read_paths
                .iter()
                .any(|path| !granted.read_paths.contains(path))
    }
}

fn validate_read_path(path: &str) -> Result<(), String> {
    let invalid = |why: &str| Err(format!("sandbox.read_paths entry {path:?} {why}"));
    if path.is_empty() || path.len() > MAX_SANDBOX_PATH_BYTES || path.contains('\0') {
        return invalid("is empty, too long or has a NUL byte");
    }
    let windows_drive = path.len() >= 3
        && path.as_bytes()[0].is_ascii_alphabetic()
        && path.as_bytes()[1] == b':'
        && matches!(path.as_bytes()[2], b'\\' | b'/');
    if !(path.starts_with('/') || path.starts_with("~/") || windows_drive) {
        return invalid("must be absolute or start with ~/");
    }
    if path.split(['/', '\\']).any(|part| part == "..") {
        return invalid("must not contain ..");
    }
    Ok(())
}

/// The environment variable the node hands `pom-plugin-host` the policy in, as
/// JSON. Absent for a legacy plugin: the host then runs it as before.
pub const HOST_SANDBOX_ENV: &str = "POM_PLUGIN_SANDBOX";

/// What the node resolved a plugin's [`PluginSandbox`] into on this machine:
/// absolute paths and the device grant. The host applies it before it loads
/// the plugin, so every process the plugin starts inherits it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSandboxPolicy {
    /// Read and write: the plugin's home, the workspace, the model folders.
    #[serde(default)]
    pub read_write: Vec<String>,
    /// Read only, beyond the system folders the host always allows: the
    /// plugin's install folder and the manifest's `read_paths`.
    #[serde(default)]
    pub read_only: Vec<String>,
    #[serde(default)]
    pub gpu: bool,
    /// Try the extra layer (own PID, mount and IPC namespaces) and fall back
    /// to the baseline where the system refuses it.
    #[serde(default)]
    pub pid_namespace: bool,
}

/// The isolation the host established, in its `ready` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxLevel {
    /// Landlock (files, signals, abstract sockets) and seccomp: no access to
    /// other processes or files outside the policy; other processes stay
    /// visible in `/proc`.
    Baseline,
    /// The baseline plus its own PID, mount and IPC namespaces: no other
    /// process is visible, and `/tmp` and `/dev/shm` are its own.
    PidNamespace,
}

/// Whether moving from the installed release's sandbox to a new one asks for
/// more, so the operator must approve the update again. A legacy release
/// (`None`) runs unconfined: leaving the sandbox widens, entering it does not.
pub fn sandbox_widens(installed: Option<&PluginSandbox>, next: Option<&PluginSandbox>) -> bool {
    match (installed, next) {
        (_, None) => installed.is_some(),
        (None, Some(_)) => false,
        (Some(installed), Some(next)) => next.asks_more_than(installed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> PluginSandbox {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn an_empty_section_is_the_narrowest_sandbox() {
        assert_eq!(parse("{}"), PluginSandbox::default());
        let narrow = PluginSandbox::default();
        assert_eq!(narrow.network, SandboxNetwork::Loopback);
        assert_eq!(narrow.listen, SandboxListen::Loopback);
        assert!(!narrow.gpu && narrow.read_paths.is_empty());
    }

    #[test]
    fn a_newer_manifest_still_reads_and_unknown_values_grant_the_least() {
        let sandbox = parse(r#"{"network":"lan","listen":"mesh","gpu":true,"future":{"x":1}}"#);
        assert_eq!(sandbox.network, SandboxNetwork::Loopback);
        assert_eq!(sandbox.listen, SandboxListen::Loopback);
        assert!(sandbox.gpu);
    }

    #[test]
    fn it_serializes_without_the_empty_path_list() {
        let json = serde_json::to_value(parse(r#"{"network":"internet","gpu":true}"#)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"network":"internet","listen":"loopback","gpu":true})
        );
    }

    #[test]
    fn read_paths_must_be_absolute_bounded_and_without_parent_steps() {
        for good in ["/opt/cuda", "~/models", "C:\\models", "D:/data"] {
            let sandbox = PluginSandbox {
                read_paths: vec![good.into()],
                ..Default::default()
            };
            assert!(sandbox.validate().is_ok(), "{good}");
        }
        for bad in [
            "",
            "models",
            "~models",
            "/opt/../etc",
            "C:\\a\\..\\b",
            "/a\0b",
        ] {
            let sandbox = PluginSandbox {
                read_paths: vec![bad.into()],
                ..Default::default()
            };
            assert!(sandbox.validate().is_err(), "{bad:?}");
        }
        let long = PluginSandbox {
            read_paths: vec![format!("/{}", "a".repeat(MAX_SANDBOX_PATH_BYTES))],
            ..Default::default()
        };
        assert!(long.validate().is_err());
        let many = PluginSandbox {
            read_paths: (0..=MAX_SANDBOX_READ_PATHS)
                .map(|i| format!("/p{i}"))
                .collect(),
            ..Default::default()
        };
        assert!(many.validate().is_err());
    }

    #[test]
    fn an_update_widens_when_it_asks_for_more_or_leaves_the_sandbox() {
        let narrow = PluginSandbox::default();
        let internet = PluginSandbox {
            network: SandboxNetwork::Internet,
            ..Default::default()
        };
        let listen = PluginSandbox {
            listen: SandboxListen::Network,
            ..Default::default()
        };
        let gpu = PluginSandbox {
            gpu: true,
            ..Default::default()
        };
        let path = PluginSandbox {
            read_paths: vec!["/opt/x".into()],
            ..Default::default()
        };

        assert!(!sandbox_widens(None, None));
        assert!(
            !sandbox_widens(None, Some(&internet)),
            "entering the sandbox narrows"
        );
        assert!(
            sandbox_widens(Some(&narrow), None),
            "leaving the sandbox widens"
        );
        for wider in [&internet, &listen, &gpu, &path] {
            assert!(sandbox_widens(Some(&narrow), Some(wider)));
            assert!(!sandbox_widens(Some(wider), Some(&narrow)));
            assert!(!sandbox_widens(Some(wider), Some(wider)));
        }
    }
}
