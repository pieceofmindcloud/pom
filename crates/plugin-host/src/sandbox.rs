//! The plugin sandbox, applied before the plugin library is loaded, so the
//! plugin and every process it starts inherit it. Design:
//! `docs/superpowers/specs/2026-11-27-plugin-sandbox-design.md`; measurements:
//! `docs/reports/2026-11-27-plugin-sandbox-linux-spike.md`.
//!
//! The node hands the policy in `POM_PLUGIN_SANDBOX` only for a plugin whose
//! release declares a sandbox; without it the host runs the plugin as before.
//!
//! Linux, in order:
//! 1. The extra layer, when asked and allowed: a forked probe checks that an
//!    unprivileged user namespace can be mapped (Ubuntu's AppArmor and
//!    Docker's seccomp refuse it by default), then the host enters user, PID,
//!    mount and IPC namespaces and forks; the child is PID 1 of the new
//!    namespace and goes on as the host, with a fresh `/proc` and its own
//!    `/tmp` and `/dev/shm` (granted to it; on the baseline `/tmp` is not). The parent only waits and exits with its status.
//! 2. `no_new_privs`, then Landlock: the files of the policy, the signal and
//!    abstract-socket scopes. ABI 6 at least, or the host refuses to load.
//! 3. A seccomp filter that answers EPERM to `ptrace`, cross-process memory
//!    calls and the calls a plugin never needs.
//!
//! The network is not filtered: Landlock rules are per port, never per
//! address, so they cannot tell loopback from the internet.
//!
//! macOS and Windows get their sandbox in later phases; until then the host
//! reports no level and the plugin runs as before.
use enterprise_protocol::{HostSandboxPolicy, SandboxLevel};
use std::io;

/// Folders every program reads: libraries, runtimes, configuration, the
/// process and device information CUDA and Python read.
#[cfg(target_os = "linux")]
const SYSTEM_READ_ONLY: &[&str] = &[
    "/usr",
    "/lib",
    "/lib64",
    "/lib32",
    "/bin",
    "/sbin",
    "/etc",
    "/opt",
    "/sys",
    "/proc",
    "/app",
    "/run/nvidia-persistenced",
];

/// Devices every program opens. `/dev/shm` is shared with every other
/// process on the baseline (vLLM's tensor parallelism needs it); `/tmp` is
/// not granted there, the node points `TMPDIR` into the plugin's home.
#[cfg(target_os = "linux")]
const DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
    "/dev/pts",
    "/dev/ptmx",
    "/dev/shm",
];

/// The landlock ABI the baseline needs: signal and abstract-socket scopes.
#[cfg(target_os = "linux")]
const MIN_LANDLOCK_ABI: i32 = 6;

/// Applies the policy. `Ok(None)` on a system without a sandbox yet (macOS,
/// Windows); an error means the plugin must not be loaded.
pub fn apply(policy: &HostSandboxPolicy) -> io::Result<Option<SandboxLevel>> {
    #[cfg(target_os = "linux")]
    {
        linux::apply(policy).map(Some)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = policy;
        Ok(None)
    }
}

/// The policy from the environment, when the node sent one.
pub fn policy_from_env() -> io::Result<Option<HostSandboxPolicy>> {
    match std::env::var(enterprise_protocol::HOST_SANDBOX_ENV) {
        Ok(json) => serde_json::from_str(&json).map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("sandbox policy: {error}"),
            )
        }),
        Err(_) => Ok(None),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use landlock::{
        path_beneath_rules, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, Scope, ABI,
    };
    use std::collections::BTreeMap;
    use std::ffi::CString;
    use std::path::Path;

    fn os_error(context: &str) -> io::Error {
        let error = io::Error::last_os_error();
        io::Error::new(error.kind(), format!("{context}: {error}"))
    }

    pub fn apply(policy: &HostSandboxPolicy) -> io::Result<SandboxLevel> {
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<u8>(),
                0,
                1,
            )
        } as i32;
        if abi < MIN_LANDLOCK_ABI {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("sandbox_unavailable: Landlock ABI {abi} on this kernel, {MIN_LANDLOCK_ABI} needed (Linux 6.12)"),
            ));
        }
        let level = if policy.pid_namespace && user_namespace_allowed() {
            enter_namespaces()?;
            SandboxLevel::PidNamespace
        } else {
            SandboxLevel::Baseline
        };
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(os_error("no_new_privs"));
        }
        // Under the extra layer `/tmp` is a tmpfs of this plugin's own.
        landlock(policy, level == SandboxLevel::PidNamespace)?;
        seccomp()?;
        Ok(level)
    }

    /// A forked child tries to map a user namespace, so a refusal (which
    /// would leave this process unmapped) never happens here.
    fn user_namespace_allowed() -> bool {
        let child = unsafe { libc::fork() };
        if child == 0 {
            let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
            let code = if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
                2
            } else if map_ids(uid, gid).is_err() {
                3
            } else {
                0
            };
            unsafe { libc::_exit(code) };
        }
        if child < 0 {
            return false;
        }
        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
        let allowed = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
        if !allowed {
            let why = match libc::WEXITSTATUS(status) {
                2 => "unshare refused (seccomp or no user namespaces)",
                3 => "id maps refused (AppArmor userns restriction)",
                _ => "probe failed",
            };
            eprintln!("pom-plugin-host: sandbox extra layer unavailable, baseline only: {why}");
        }
        allowed
    }

    fn map_ids(uid: libc::uid_t, gid: libc::gid_t) -> io::Result<()> {
        std::fs::write("/proc/self/setgroups", "deny")?;
        std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1"))?;
        std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1"))
    }

    /// Enters the namespaces and forks; only the child returns. The parent
    /// waits for it and exits with its status, and the child dies with the
    /// parent (the node kills the parent it started).
    fn enter_namespaces() -> io::Result<()> {
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let flags =
            libc::CLONE_NEWUSER | libc::CLONE_NEWPID | libc::CLONE_NEWNS | libc::CLONE_NEWIPC;
        if unsafe { libc::unshare(flags) } != 0 {
            return Err(os_error("sandbox namespaces"));
        }
        map_ids(uid, gid)?;
        let child = unsafe { libc::fork() };
        if child < 0 {
            return Err(os_error("sandbox fork"));
        }
        if child > 0 {
            let mut status = 0;
            loop {
                let waited = unsafe { libc::waitpid(child, &mut status, 0) };
                if waited == child
                    || (waited < 0
                        && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR))
                {
                    break;
                }
            }
            let code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                128 + libc::WTERMSIG(status)
            };
            unsafe { libc::_exit(code) };
        }
        // From inside the new namespace the parent shows as PID 0, so its
        // death cannot be checked here; if it died before this line, the
        // child still holds the node's pipes and ends at their EOF.
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
        mount_private()
    }

    /// `/` private so nothing propagates out, then a `/proc` of this PID
    /// namespace and private `/tmp` and `/dev/shm`.
    fn mount_private() -> io::Result<()> {
        let none: *const libc::c_char = std::ptr::null();
        let root = CString::new("/").unwrap();
        if unsafe {
            libc::mount(
                none,
                root.as_ptr(),
                none,
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(os_error("make / private"));
        }
        let proc_ = CString::new("proc").unwrap();
        let proc_dir = CString::new("/proc").unwrap();
        let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        if unsafe {
            libc::mount(
                proc_.as_ptr(),
                proc_dir.as_ptr(),
                proc_.as_ptr(),
                flags,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(os_error("mount /proc"));
        }
        let tmpfs = CString::new("tmpfs").unwrap();
        for dir in ["/tmp", "/dev/shm"] {
            if !Path::new(dir).is_dir() {
                continue;
            }
            let target = CString::new(dir).unwrap();
            let options = CString::new("mode=1777").unwrap();
            if unsafe {
                libc::mount(
                    tmpfs.as_ptr(),
                    target.as_ptr(),
                    tmpfs.as_ptr(),
                    libc::MS_NOSUID | libc::MS_NODEV,
                    options.as_ptr().cast(),
                )
            } != 0
            {
                return Err(os_error(&format!("mount a private {dir}")));
            }
        }
        Ok(())
    }

    fn existing(paths: impl IntoIterator<Item = String>) -> Vec<String> {
        paths
            .into_iter()
            .filter(|path| Path::new(path).exists())
            .collect()
    }

    fn landlock(policy: &HostSandboxPolicy, private_tmp: bool) -> io::Result<()> {
        let abi = ABI::V6;
        let read_only =
            existing(
                SYSTEM_READ_ONLY
                    .iter()
                    .map(|path| (*path).to_owned())
                    .chain(policy.read_only.iter().cloned())
                    .chain(std::env::current_exe().ok().and_then(|exe| {
                        exe.parent().map(|dir| dir.to_string_lossy().into_owned())
                    })),
            );
        let mut read_write: Vec<String> = DEVICES.iter().map(|path| (*path).to_owned()).collect();
        read_write.extend(policy.read_write.iter().cloned());
        if private_tmp {
            read_write.push("/tmp".to_owned());
        }
        if policy.gpu {
            if let Ok(entries) = std::fs::read_dir("/dev") {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("nvidia") || name == "dri" || name == "kfd" {
                        read_write.push(format!("/dev/{name}"));
                    }
                }
            }
        }
        let read_write = existing(read_write);
        let to_io = |error: landlock::RulesetError| {
            io::Error::other(format!("sandbox_unavailable: landlock: {error}"))
        };
        let status = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .map_err(to_io)?
            .scope(Scope::Signal | Scope::AbstractUnixSocket)
            .map_err(to_io)?
            .create()
            .map_err(to_io)?
            .add_rules(path_beneath_rules(&read_only, AccessFs::from_read(abi)))
            .map_err(to_io)?
            .add_rules(path_beneath_rules(&read_write, AccessFs::from_all(abi)))
            .map_err(to_io)?
            .restrict_self()
            .map_err(to_io)?;
        if status.ruleset != RulesetStatus::FullyEnforced {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("sandbox_unavailable: landlock only {:?}", status.ruleset),
            ));
        }
        Ok(())
    }

    fn seccomp() -> io::Result<()> {
        use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule};
        let denied = [
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_kcmp,
            libc::SYS_pidfd_getfd,
            libc::SYS_bpf,
            libc::SYS_perf_event_open,
            libc::SYS_userfaultfd,
            libc::SYS_unshare,
            libc::SYS_setns,
            libc::SYS_keyctl,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_init_module,
            libc::SYS_finit_module,
            libc::SYS_delete_module,
            libc::SYS_kexec_load,
        ];
        let rules: BTreeMap<i64, Vec<SeccompRule>> =
            denied.iter().map(|call| (*call, Vec::new())).collect();
        let arch = std::env::consts::ARCH.try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "sandbox_unavailable: seccomp architecture",
            )
        })?;
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EPERM as u32),
            arch,
        )
        .map_err(|error| io::Error::other(format!("sandbox_unavailable: seccomp: {error}")))?;
        let program: BpfProgram = filter.try_into().map_err(|error| {
            io::Error::other(format!("sandbox_unavailable: seccomp: {error:?}"))
        })?;
        seccompiler::apply_filter(&program)
            .map_err(|error| io::Error::other(format!("sandbox_unavailable: seccomp: {error}")))
    }
}
