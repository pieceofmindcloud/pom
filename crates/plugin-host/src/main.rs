use enterprise_protocol::{
    abi::{ByteSlice, PluginApiV1, PluginEntryV1},
    IPC_PROTOCOL, MAX_IPC_FRAME_BYTES, PLUGIN_ABI_VERSION,
};
use libloading::Library;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub const MAX_FRAME_BYTES: usize = MAX_IPC_FRAME_BYTES;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcRequest {
    pub id: u64,
    pub method: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcResponse {
    pub id: u64,
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix)?;
    let size = u32::from_be_bytes(prefix) as usize;
    if size == 0 || size > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame size exceeds the limit of {MAX_FRAME_BYTES} bytes"),
        ));
    }
    let mut body = vec![0_u8; size];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    let body = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if body.is_empty() || body.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame size exceeds the limit of {MAX_FRAME_BYTES} bytes"),
        ));
    }
    let size = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    writer.write_all(&size.to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()
}

pub fn validate_plugin_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the plugin path must be absolute",
        ));
    }
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the plugin file does not exist",
        ));
    }
    Ok(())
}

/// Resolve the ABI entrypoint from the manifest's plugin code.
///
/// The host is shared by every plugin, so the symbol must follow the generic
/// contract instead of naming one product. Validate the code before putting it
/// into a symbol name so a malformed manifest cannot escape the contract.
pub fn plugin_entry_symbol(plugin_code: &str) -> io::Result<Vec<u8>> {
    let bytes = plugin_code.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 128
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid plugin_code for the plugin entrypoint",
        ));
    }
    let mut symbol = format!("pom_{plugin_code}_plugin_v1").into_bytes();
    symbol.push(0);
    Ok(symbol)
}

pub fn validate_artifact(
    path: &Path,
    manifest: &enterprise_protocol::PluginManifest,
) -> io::Result<()> {
    validate_plugin_path(path)?;
    if manifest.plugin_abi != PLUGIN_ABI_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest ABI incompatible",
        ));
    }
    if manifest.signature.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest without a signature",
        ));
    }
    let metadata = std::fs::metadata(path)?;
    if metadata.len() != manifest.size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plugin size mismatch",
        ));
    }
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    let actual = hex::encode(hasher.finalize());
    let expected = manifest
        .sha256
        .strip_prefix("sha256:")
        .unwrap_or(&manifest.sha256);
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plugin hash mismatch",
        ));
    }
    Ok(())
}

pub fn validate_api(api: &PluginApiV1) -> io::Result<()> {
    if api.abi_version != PLUGIN_ABI_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plugin ABI incompatible",
        ));
    }
    if api.plugin_version.is_null()
        || api.create.is_none()
        || api.ingest.is_none()
        || api.query.is_none()
        || api.free_buffer.is_none()
        || api.shutdown.is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plugin ABI does not provide all the required functions",
        ));
    }
    Ok(())
}

pub struct LoadedPlugin {
    _library: Library,
    api: &'static PluginApiV1,
    handle: enterprise_protocol::abi::PluginHandle,
}

impl LoadedPlugin {
    /// Loads the library once and keeps it resident until the host exits.
    pub unsafe fn load(path: &Path, plugin_code: &str, config: &[u8]) -> io::Result<Self> {
        validate_plugin_path(path)?;
        let library = Library::new(path).map_err(|error| io::Error::other(error.to_string()))?;
        let entry_symbol = plugin_entry_symbol(plugin_code)?;
        let entry = library
            .get::<PluginEntryV1>(&entry_symbol)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let api_ptr = entry();
        if api_ptr.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "plugin returned a null ABI",
            ));
        }
        validate_api(&*api_ptr)?;
        let api: &'static PluginApiV1 = &*api_ptr;
        let create = api.create.expect("validate_api guaranteed create");
        let handle = create(
            enterprise_protocol::abi::HostCallbacks { log: None },
            ByteSlice {
                ptr: config.as_ptr(),
                len: config.len(),
            },
        );
        if handle.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "plugin refused the configuration",
            ));
        }
        Ok(Self {
            _library: library,
            api,
            handle,
        })
    }

    pub fn version(&self) -> io::Result<String> {
        unsafe {
            CStr::from_ptr(self.api.plugin_version)
                .to_str()
                .map(str::to_owned)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
    }

    pub fn ingest(&self, event: &[u8]) -> io::Result<()> {
        let ingest = self.api.ingest.expect("validate_api guaranteed ingest");
        let status = unsafe {
            ingest(
                self.handle,
                ByteSlice {
                    ptr: event.as_ptr(),
                    len: event.len(),
                },
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::other(format!("plugin returned status {status}")))
        }
    }

    pub fn query(&self, request: &[u8]) -> io::Result<Vec<u8>> {
        let query = self.api.query.expect("validate_api guaranteed query");
        let buffer = unsafe {
            query(
                self.handle,
                ByteSlice {
                    ptr: request.as_ptr(),
                    len: request.len(),
                },
            )
        };
        if buffer.len > MAX_FRAME_BYTES {
            if let Some(free_buffer) = self.api.free_buffer {
                unsafe { free_buffer(buffer) };
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "plugin response exceeds the IPC limit",
            ));
        }
        let result = if buffer.len == 0 {
            Vec::new()
        } else if buffer.ptr.is_null() {
            if let Some(free_buffer) = self.api.free_buffer {
                unsafe { free_buffer(buffer) };
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "plugin returned a null buffer with a positive size",
            ));
        } else {
            unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len).to_vec() }
        };
        if let Some(free_buffer) = self.api.free_buffer {
            unsafe { free_buffer(buffer) };
        }
        Ok(result)
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        if let Some(shutdown) = self.api.shutdown {
            unsafe { shutdown(self.handle) };
        }
    }
}

#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    threshold: u32,
    failures: u32,
}

impl CircuitBreaker {
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold: threshold.max(1),
            failures: 0,
        }
    }

    pub fn record_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    pub fn record_success(&mut self) {
        self.failures = 0;
    }

    pub fn is_open(&self) -> bool {
        self.failures >= self.threshold
    }
}

fn response(request: &IpcRequest, result: Value) -> IpcResponse {
    IpcResponse {
        id: request.id,
        ok: true,
        result: Some(result),
        error: None,
    }
}

fn error_response(request: &IpcRequest, error: impl Into<String>) -> IpcResponse {
    IpcResponse {
        id: request.id,
        ok: false,
        result: None,
        error: Some(error.into()),
    }
}

fn handle_request(request: &IpcRequest, plugin: Option<&LoadedPlugin>) -> IpcResponse {
    match request.method.as_str() {
        "ping" => response(
            request,
            serde_json::json!({ "protocol": IPC_PROTOCOL, "status": "pong" }),
        ),
        "ready" => {
            let Some(plugin) = plugin else {
                return error_response(request, "no plugin loaded");
            };
            match plugin.version() {
                Ok(version) => response(
                    request,
                    serde_json::json!({ "status": "ready", "plugin_version": version }),
                ),
                Err(error) => error_response(request, error.to_string()),
            }
        }
        "shutdown" => response(request, serde_json::json!({ "status": "shutting_down" })),
        "ingest" => {
            let Some(plugin) = plugin else {
                return error_response(request, "no plugin loaded");
            };
            let event = request.payload.to_string();
            match plugin.ingest(event.as_bytes()) {
                Ok(()) => response(request, serde_json::json!({ "status": "accepted" })),
                Err(error) => error_response(request, error.to_string()),
            }
        }
        "query" => {
            let Some(plugin) = plugin else {
                return error_response(request, "no plugin loaded");
            };
            let query = request.payload.to_string();
            match plugin.query(query.as_bytes()) {
                Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                    Ok(result) => response(request, result),
                    Err(error) => error_response(request, error.to_string()),
                },
                Err(error) => error_response(request, error.to_string()),
            }
        }
        _ => error_response(request, "unknown IPC method"),
    }
}

fn run() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let loaded = match (args.next(), args.next(), args.next()) {
        (None, None, None) => None,
        (Some(path), Some(plugin_code), None) => {
            let plugin_code = plugin_code.into_string().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "plugin_code is not UTF-8")
            })?;
            Some(unsafe { LoadedPlugin::load(&PathBuf::from(path), &plugin_code, b"{}") }?)
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: pom-plugin-host [absolute path] [plugin_code]",
            ))
        }
    };
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    loop {
        let request = match read_frame::<IpcRequest>(&mut stdin) {
            Ok(request) => request,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        };
        let should_stop = request.method == "shutdown";
        write_frame(&mut stdout, &handle_request(&request, loaded.as_ref()))?;
        if should_stop {
            break;
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("pom-plugin-host: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn ipc_frame_round_trips_json_with_a_size_prefix() {
        let request = IpcRequest {
            id: 7,
            method: "ping".into(),
            payload: serde_json::json!({"protocol": "pom-enterprise-ipc/v1"}),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).expect("write frame");

        assert_eq!(&bytes[..4], &(bytes.len() as u32 - 4).to_be_bytes());
        let decoded: IpcRequest = read_frame(&mut Cursor::new(bytes)).expect("read frame");
        assert_eq!(decoded, request);
    }

    #[test]
    fn ipc_accepts_log_frames_larger_than_one_mib() {
        let payload = "x".repeat(2 * 1024 * 1024);
        let request = IpcRequest {
            id: 8,
            method: "query".into(),
            payload: Value::String(payload.clone()),
        };
        let mut bytes = Vec::new();

        write_frame(&mut bytes, &request).expect("2 MiB log query response fits the IPC frame");
        let decoded: IpcRequest = read_frame(&mut Cursor::new(bytes)).expect("read large frame");

        assert_eq!(decoded.payload.as_str(), Some(payload.as_str()));
    }

    #[test]
    fn ipc_rejects_frames_over_the_protocol_limit() {
        let mut bytes = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes().to_vec();
        bytes.extend(std::iter::repeat_n(b'x', MAX_FRAME_BYTES + 1));

        let error = read_frame::<IpcRequest>(&mut Cursor::new(bytes)).expect_err("oversized frame");

        assert!(error.to_string().contains("limit"));
    }

    #[test]
    fn loader_rejects_relative_plugin_paths_before_dynamic_loading() {
        let error =
            validate_plugin_path(std::path::Path::new("enterprise.so")).expect_err("relative path");

        assert!(error.to_string().contains("absolute"));
    }

    #[test]
    fn entrypoint_symbol_is_derived_for_base_and_enterprise_plugins() {
        assert_eq!(
            plugin_entry_symbol("base").expect("base symbol"),
            b"pom_base_plugin_v1\0"
        );
        assert_eq!(
            plugin_entry_symbol("logs").expect("enterprise symbol"),
            b"pom_logs_plugin_v1\0"
        );
    }

    #[test]
    fn entrypoint_symbol_rejects_path_injection() {
        let error = plugin_entry_symbol("../enterprise").expect_err("invalid code");

        assert!(error.to_string().contains("plugin_code"));
    }

    #[test]
    fn incompatible_abi_is_rejected_without_calling_plugin_code() {
        let api = enterprise_protocol::abi::PluginApiV1 {
            abi_version: 2,
            plugin_version: std::ptr::null(),
            capabilities: 0,
            create: None,
            ingest: None,
            query: None,
            free_buffer: None,
            shutdown: None,
        };

        let error = validate_api(&api).expect_err("wrong ABI");

        assert!(error.to_string().contains("ABI"));
    }

    #[test]
    fn circuit_breaker_opens_after_repeated_plugin_failures() {
        let mut breaker = CircuitBreaker::new(3);
        breaker.record_failure();
        breaker.record_failure();
        assert!(!breaker.is_open());
        breaker.record_failure();

        assert!(breaker.is_open());
    }
}
