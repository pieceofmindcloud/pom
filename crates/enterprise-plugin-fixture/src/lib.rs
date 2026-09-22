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
pub unsafe extern "C" fn pom_enterprise_plugin_v1() -> *const PluginApiV1 {
    &API
}

#[no_mangle]
pub unsafe extern "C" fn pom_base_plugin_v1() -> *const PluginApiV1 {
    &API
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_exports_the_v1_abi_and_all_required_callbacks() {
        let api = unsafe { &*pom_enterprise_plugin_v1() };

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
        assert_eq!(unsafe { pom_enterprise_plugin_v1() }, &API);
    }
}
