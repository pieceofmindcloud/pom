//! The process tree a plugin host starts, tied to the host's lifetime.
//!
//! A plugin runs inside `pom-plugin-host`, and some plugins start processes of
//! their own (node, npm, code-server). On Linux and macOS the host itself ends
//! them when it leaves (`crates/plugin-host/src/tree.rs`: a child subreaper on
//! Linux, then SIGTERM and SIGKILL to every descendant); on Windows nothing
//! tied them together, so closing POM left the host and its children running,
//! each holding an inherited pipe that kept the host from ever seeing its
//! input close.
//!
//! On Windows each host goes into its own Job Object marked "kill on close":
//! everything the host starts afterwards joins the same job, and dropping the
//! `ProcessTree` (the host stopping) or POM exiting (the handle closing with
//! the process) ends the whole tree. POM itself is never put in a job, so its
//! own restart and the Windows supervisor are unaffected.

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct ProcessTree(HANDLE);

    // The handle is only closed once, in Drop; the job itself is a kernel
    // object usable from any thread.
    unsafe impl Send for ProcessTree {}
    unsafe impl Sync for ProcessTree {}

    impl ProcessTree {
        /// Puts the process behind `process` (a raw process handle) in a new
        /// kill-on-close job. `None` when Windows refuses; the host still runs,
        /// only without the guarantee.
        pub fn adopt(process: std::os::windows::io::RawHandle) -> Option<Self> {
            unsafe {
                let job = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?;
                let tree = Self(job);
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .ok()?;
                AssignProcessToJobObject(job, HANDLE(process)).ok()?;
                Some(tree)
            }
        }
    }

    impl Drop for ProcessTree {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
pub use imp::ProcessTree;

/// Outside Windows the host ends its own tree (`crates/plugin-host/src/tree.rs`); nothing to hold.
#[cfg(not(windows))]
pub struct ProcessTree;
