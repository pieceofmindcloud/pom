//! Native crash report: the fatal signal, the faulting address and the stack,
//! written to stderr before the process dies.
//!
//! The node runs llama.cpp in-process, so a bad pointer in native code kills
//! the whole node with no line in its own log - only the host kernel's
//! `segfault at ... in pom[...]`, which a container or an unprivileged
//! operator often cannot read. With this handler the report lands in the
//! container log: each frame is `binary(+offset)`, resolvable offline with
//! `addr2line -f -C -e <binary> <offset>` against the same build.
//!
//! The handler only uses async-signal-safe calls (`write`, glibc `backtrace`
//! after it was warmed up, `backtrace_symbols_fd`), then restores the default
//! action and re-raises, so the exit status (139 for SIGSEGV) and any core
//! dump are unchanged.

#[cfg(target_os = "linux")]
mod imp {
    use std::os::raw::{c_int, c_void};

    extern "C" {
        fn backtrace(buffer: *mut *mut c_void, size: c_int) -> c_int;
        fn backtrace_symbols_fd(buffer: *const *mut c_void, size: c_int, fd: c_int);
    }

    const SIGNALS: [(c_int, &str); 5] = [
        (libc::SIGSEGV, "SIGSEGV"),
        (libc::SIGBUS, "SIGBUS"),
        (libc::SIGILL, "SIGILL"),
        (libc::SIGFPE, "SIGFPE"),
        (libc::SIGABRT, "SIGABRT"),
    ];

    fn write_stderr(bytes: &[u8]) {
        unsafe { libc::write(2, bytes.as_ptr().cast(), bytes.len()) };
    }

    /// `value` in hex into `out`, without allocating.
    fn hex(value: usize, out: &mut [u8; 18]) -> &[u8] {
        out[0] = b'0';
        out[1] = b'x';
        let mut digits = [0u8; 16];
        let mut n = value;
        let mut len = 0;
        loop {
            digits[len] = b"0123456789abcdef"[n & 0xf];
            len += 1;
            n >>= 4;
            if n == 0 {
                break;
            }
        }
        for i in 0..len {
            out[2 + i] = digits[len - 1 - i];
        }
        &out[..2 + len]
    }

    extern "C" fn on_fatal(signal: c_int, info: *mut libc::siginfo_t, _context: *mut c_void) {
        let name = SIGNALS
            .iter()
            .find(|(number, _)| *number == signal)
            .map(|(_, name)| *name)
            .unwrap_or("fatal signal");
        let address = if info.is_null() { 0 } else { unsafe { (*info).si_addr() as usize } };
        let mut buffer = [0u8; 18];
        write_stderr(b"\n=== pom: native crash: ");
        write_stderr(name.as_bytes());
        write_stderr(b" at address ");
        write_stderr(hex(address, &mut buffer));
        let mut thread_name = [0u8; 32];
        unsafe {
            libc::pthread_getname_np(libc::pthread_self(), thread_name.as_mut_ptr().cast(), thread_name.len())
        };
        let thread_len = thread_name.iter().position(|byte| *byte == 0).unwrap_or(0);
        write_stderr(b" in thread ");
        write_stderr(&thread_name[..thread_len]);
        write_stderr(b"\n=== frames are binary(+offset): addr2line -f -C -e <binary> <offset>\n");
        let mut frames = [std::ptr::null_mut::<c_void>(); 64];
        unsafe {
            let count = backtrace(frames.as_mut_ptr(), frames.len() as c_int);
            backtrace_symbols_fd(frames.as_ptr(), count, 2);
        }
        write_stderr(b"=== end of native crash report\n");
        // Default action and re-raise: same exit status, same core dump.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    pub(super) fn install() {
        // glibc loads libgcc lazily on the first `backtrace` call, which is
        // not safe inside a signal handler: warm it up here.
        let mut warm = [std::ptr::null_mut::<c_void>(); 1];
        unsafe { backtrace(warm.as_mut_ptr(), 1) };
        for (signal, _) in SIGNALS {
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_fatal as usize;
                action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }
}

/// Install the native crash report for the life of the process. A no-op off
/// Linux.
pub fn install() {
    #[cfg(target_os = "linux")]
    imp::install();
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    /// The report reaches stderr and the process still dies of the signal.
    /// Runs the crash in a forked child so the test binary survives.
    #[test]
    fn a_segfault_is_reported_and_still_kills_with_the_same_signal() {
        let mut pipe = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                libc::dup2(pipe[1], 2);
                super::install();
                let null: *const u8 = std::ptr::null();
                std::ptr::read_volatile(null);
                libc::_exit(0);
            }
        }
        unsafe { libc::close(pipe[1]) };
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let mut output = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = unsafe { libc::read(pipe[0], chunk.as_mut_ptr().cast(), chunk.len()) };
            if read <= 0 {
                break;
            }
            output.extend_from_slice(&chunk[..read as usize]);
        }
        let text = String::from_utf8_lossy(&output);
        assert!(libc::WIFSIGNALED(status), "o filho devia morrer pelo sinal: {text}");
        assert_eq!(libc::WTERMSIG(status), libc::SIGSEGV);
        assert!(text.contains("native crash: SIGSEGV at address 0x0"), "{text}");
        assert!(text.contains("end of native crash report"), "{text}");
    }
}
