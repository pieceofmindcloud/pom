//! The inference engine's own words about a load that failed.
//!
//! llama.cpp explains a failed load in its log ("error loading model: ...",
//! "failed to allocate CUDA buffer ...") and a native abort in the message it
//! hands to `ggml_abort`. Without a callback both go only to the process's
//! stderr, which scrolls away on Linux and closes with the window on Windows,
//! so the operator saw "failed" and nothing else. This module keeps them:
//!
//! - an in-memory ring of the last lines, read when the engine returns an error;
//! - a file at the project root, written line by line, read by the next start
//!   when the engine took the whole process down (a native crash leaves no
//!   other trace of the reason).
//!
//! Every line is still written to stderr, as before the callback existed.
//! An engine that runs out of this process (a plugin engine) is not captured
//! here: its failure carries the error it returned.

use std::collections::VecDeque;
use std::ffi::{c_char, c_uint, c_void, CStr};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, Once};

/// Lines kept in memory and offered by [`tail`].
const RING_LINES: usize = 200;
/// Lines a failure carries to the screen.
pub const TAIL_LINES: usize = 60;
/// Beyond this the file stops growing: what explains a load is at its start
/// and at its end, and the end is in the ring.
const FILE_LIMIT: u64 = 2 * 1024 * 1024;

const GGML_LOG_LEVEL_DEBUG: c_uint = 1;
const GGML_LOG_LEVEL_CONT: c_uint = 5;

struct Sink {
    ring: VecDeque<String>,
    partial: String,
    last_level: c_uint,
    file: Option<std::fs::File>,
    written: u64,
}

static SINK: Mutex<Sink> = Mutex::new(Sink {
    ring: VecDeque::new(),
    partial: String::new(),
    last_level: 0,
    file: None,
    written: 0,
});

/// The file the current load writes and the next start reads after a crash.
pub fn path() -> PathBuf {
    crate::config::project_root().join("engine-last-load.log")
}

fn sink() -> std::sync::MutexGuard<'static, Sink> {
    SINK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Registers the callbacks with llama.cpp/ggml. Idempotent.
pub fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        llama_engine::llama_cpp_sys_2::llama_log_set(Some(on_log), std::ptr::null_mut());
        llama_engine::llama_cpp_sys_2::ggml_log_set(Some(on_log), std::ptr::null_mut());
        llama_engine::llama_cpp_sys_2::ggml_set_abort_callback(Some(on_abort));
    });
}

/// A new load attempt starts: forget the previous attempt's lines.
pub fn begin_attempt() {
    let mut s = sink();
    s.ring.clear();
    s.partial.clear();
    s.written = 0;
    s.file = std::fs::File::create(path()).ok();
}

/// The last lines of the current attempt, oldest first.
pub fn tail(lines: usize) -> Vec<String> {
    let s = sink();
    let skip = s.ring.len().saturating_sub(lines);
    s.ring.iter().skip(skip).cloned().collect()
}

/// The last lines a previous process left on disk, for the start that
/// follows a crash.
pub fn disk_tail(lines: usize) -> Vec<String> {
    let Ok(raw) = std::fs::read(path()) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&raw);
    let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let skip = all.len().saturating_sub(lines);
    all[skip..].iter().map(|l| l.to_string()).collect()
}

fn record(s: &mut Sink, line: String) {
    if let Some(file) = s.file.as_mut() {
        if s.written < FILE_LIMIT {
            // One write per line: a native abort right after still leaves it in the file.
            if file.write_all(format!("{line}\n").as_bytes()).is_ok() {
                s.written += line.len() as u64 + 1;
            }
        }
    }
    if s.ring.len() == RING_LINES {
        s.ring.pop_front();
    }
    s.ring.push_back(line);
}

const GGML_LOG_LEVEL_WARN: c_uint = 3;

fn push_text(level: c_uint, text: &str) {
    // Warnings and errors also go to the node's log, so the cluster console
    // (when collected) carries the engine's reason beside the network's lines.
    // Emitted after the lock is released: a tracing layer may log in turn.
    let mut forward = Vec::new();
    {
        let mut s = sink();
    let level = if level == GGML_LOG_LEVEL_CONT {
        s.last_level
    } else {
        level
    };
    s.last_level = level;
    // Debug lines (the tensor listing) would push the reason out of the ring.
    if level == GGML_LOG_LEVEL_DEBUG {
        return;
    }
    s.partial.push_str(text);
    while let Some(end) = s.partial.find('\n') {
        let line: String = s.partial.drain(..=end).collect();
        let line = line.trim_end().to_string();
        if !line.is_empty() {
            if level >= GGML_LOG_LEVEL_WARN {
                forward.push((level, line.clone()));
            }
            record(&mut s, line);
        }
    }
    }
    for (level, line) in forward {
        if level > GGML_LOG_LEVEL_WARN {
            tracing::error!(target: "engine", "{line}");
        } else {
            tracing::warn!(target: "engine", "{line}");
        }
    }
}

unsafe extern "C" fn on_log(level: c_uint, text: *const c_char, _user: *mut c_void) {
    if text.is_null() {
        return;
    }
    let text = unsafe { CStr::from_ptr(text) }.to_string_lossy();
    eprint!("{text}");
    push_text(level, &text);
}

unsafe extern "C" fn on_abort(message: *const c_char) {
    let message = if message.is_null() {
        "unknown".into()
    } else {
        unsafe { CStr::from_ptr(message) }.to_string_lossy()
    };
    eprintln!("{message}");
    let mut s = sink();
    let pending = std::mem::take(&mut s.partial);
    if !pending.trim().is_empty() {
        record(&mut s, pending.trim_end().to_string());
    }
    record(&mut s, format!("engine aborted: {message}"));
    if let Some(file) = s.file.as_mut() {
        let _ = file.sync_all();
    }
}

#[cfg(test)]
pub(crate) fn feed_for_test(level: c_uint, text: &str) {
    push_text(level, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_assembled_and_debug_is_dropped() {
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("pom-engine-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("POM_HOME", &dir);
        begin_attempt();
        feed_for_test(2, "llama_model_load: loading ");
        feed_for_test(GGML_LOG_LEVEL_CONT, "model\n");
        feed_for_test(GGML_LOG_LEVEL_DEBUG, "tensor 1\n");
        feed_for_test(4, "error loading model: out of memory\n");
        assert_eq!(
            tail(10),
            vec!["llama_model_load: loading model", "error loading model: out of memory"]
        );
        assert_eq!(disk_tail(1), vec!["error loading model: out of memory"]);
        begin_attempt();
        assert!(tail(10).is_empty());
        std::env::remove_var("POM_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
