use enterprise_protocol::abi::{ByteBuffer, ByteSlice, HostCallbacks, PluginApiV1, PluginHandle};
use std::ffi::c_void;

static VERSION: &[u8] = b"enterprise-fixture-0.1.0\0";

unsafe extern "C" fn create(_: HostCallbacks, _: ByteSlice) -> PluginHandle {
    Box::into_raw(Box::new(0_u8)).cast::<c_void>()
}

unsafe extern "C" fn ingest(_: PluginHandle, _: ByteSlice) -> i32 {
    0
}

unsafe extern "C" fn query(_: PluginHandle, _: ByteSlice) -> ByteBuffer {
    ByteBuffer {
        ptr: std::ptr::null_mut(),
        len: 0,
    }
}

unsafe extern "C" fn free_buffer(_: ByteBuffer) {}

unsafe extern "C" fn shutdown(handle: PluginHandle) {
    if !handle.is_null() {
        drop(Box::from_raw(handle.cast::<u8>()));
    }
}

static API: PluginApiV1 = PluginApiV1 {
    abi_version: enterprise_protocol::PLUGIN_ABI_VERSION,
    plugin_version: VERSION.as_ptr().cast(),
    capabilities: 0,
    create: Some(create),
    ingest: Some(ingest),
    query: Some(query),
    free_buffer: Some(free_buffer),
    shutdown: Some(shutdown),
};

#[no_mangle]
pub unsafe extern "C" fn pom_logs_plugin_v1() -> *const PluginApiV1 {
    &API
}

#[no_mangle]
pub unsafe extern "C" fn pom_base_plugin_v1() -> *const PluginApiV1 {
    &API
}

/// `pom_probe_plugin_v1`: the same plugin with a `query` that tries what the
/// request names and says what worked, so a test can check from inside a
/// sandboxed host what the sandbox allows (`crates/plugin-host/tests/sandbox.rs`).
/// Request: `{"read": [paths], "write": [paths], "pid": n}`.
unsafe extern "C" fn probe_query(_: PluginHandle, request: ByteSlice) -> ByteBuffer {
    let bytes = if request.ptr.is_null() {
        &[][..]
    } else {
        std::slice::from_raw_parts(request.ptr, request.len)
    };
    let request: serde_json::Value = serde_json::from_slice(bytes).unwrap_or_default();
    let outcome = |result: std::io::Result<()>| match result {
        Ok(()) => "ok".to_owned(),
        Err(error) => format!("refused: {error}"),
    };
    let mut answer = serde_json::Map::new();
    let mut reads = serde_json::Map::new();
    for path in request["read"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
    {
        reads.insert(path.into(), outcome(std::fs::read(path).map(|_| ())).into());
    }
    answer.insert("read".into(), reads.into());
    let mut writes = serde_json::Map::new();
    for path in request["write"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
    {
        writes.insert(path.into(), outcome(std::fs::write(path, b"probe")).into());
    }
    answer.insert("write".into(), writes.into());
    #[cfg(unix)]
    {
        if let Some(pid) = request["pid"].as_i64() {
            let signal = libc::kill(pid as i32, 0);
            answer.insert(
                "signal".into(),
                outcome(if signal == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                })
                .into(),
            );
            let traced = libc::ptrace(libc::PTRACE_ATTACH, pid as i32, 0, 0);
            answer.insert(
                "ptrace".into(),
                outcome(if traced == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                })
                .into(),
            );
        }
        answer.insert("pid".into(), libc::getpid().into());
    }
    let visible = std::fs::read_dir("/proc")
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().parse::<u32>().is_ok())
                .count()
        })
        .unwrap_or(0);
    answer.insert("visible_processes".into(), visible.into());
    let body = serde_json::Value::Object(answer)
        .to_string()
        .into_bytes()
        .into_boxed_slice();
    let len = body.len();
    ByteBuffer {
        ptr: Box::into_raw(body).cast::<u8>(),
        len,
    }
}

unsafe extern "C" fn probe_free(buffer: ByteBuffer) {
    if !buffer.ptr.is_null() {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            buffer.ptr, buffer.len,
        )));
    }
}

static PROBE_API: PluginApiV1 = PluginApiV1 {
    abi_version: enterprise_protocol::PLUGIN_ABI_VERSION,
    plugin_version: VERSION.as_ptr().cast(),
    capabilities: 0,
    create: Some(create),
    ingest: Some(ingest),
    query: Some(probe_query),
    free_buffer: Some(probe_free),
    shutdown: Some(shutdown),
};

#[no_mangle]
pub unsafe extern "C" fn pom_probe_plugin_v1() -> *const PluginApiV1 {
    &PROBE_API
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_exports_the_v1_abi_and_all_required_callbacks() {
        let api = unsafe { &*pom_logs_plugin_v1() };

        assert_eq!(api.abi_version, enterprise_protocol::PLUGIN_ABI_VERSION);
        assert!(!api.plugin_version.is_null());
        assert!(api.create.is_some());
        assert!(api.ingest.is_some());
        assert!(api.query.is_some());
        assert!(api.free_buffer.is_some());
        assert!(api.shutdown.is_some());
    }

    #[test]
    fn fixture_exports_the_generic_base_and_enterprise_entrypoints() {
        assert_eq!(unsafe { pom_base_plugin_v1() }, &API);
        assert_eq!(unsafe { pom_logs_plugin_v1() }, &API);
    }
}
