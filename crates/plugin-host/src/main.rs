use enterprise_protocol::{
    abi::{ByteSlice, PluginApiV1, PluginEntryV1},
    IPC_PROTOCOL, PLUGIN_ABI_VERSION,
};
use libloading::Library;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

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
            format!("tamanho do frame excede o limite de {MAX_FRAME_BYTES} bytes"),
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
            format!("tamanho do frame excede o limite de {MAX_FRAME_BYTES} bytes"),
        ));
    }
    let size = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame grande demais"))?;
    writer.write_all(&size.to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()
}

pub fn validate_plugin_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "o caminho do plugin deve ser absoluto",
        ));
    }
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "o arquivo do plugin não existe",
        ));
    }
    Ok(())
}

pub fn validate_artifact(
    path: &Path,
    manifest: &enterprise_protocol::PluginManifest,
) -> io::Result<()> {
    validate_plugin_path(path)?;
    if manifest.plugin_abi != PLUGIN_ABI_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ABI do manifest incompatível",
        ));
    }
    if manifest.signature.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest sem assinatura",
        ));
    }
    let metadata = std::fs::metadata(path)?;
    if metadata.len() != manifest.size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "tamanho do plugin divergente",
        ));
    }
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
    if actual != manifest.sha256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hash do plugin divergente",
        ));
    }
    Ok(())
}

pub fn validate_api(api: &PluginApiV1) -> io::Result<()> {
    if api.abi_version != PLUGIN_ABI_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ABI do plugin incompatível",
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
            "ABI do plugin não fornece todas as funções obrigatórias",
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
    pub unsafe fn load(path: &Path, config: &[u8]) -> io::Result<Self> {
        validate_plugin_path(path)?;
        let library = Library::new(path).map_err(|error| io::Error::other(error.to_string()))?;
        let entry = library
            .get::<PluginEntryV1>(b"pom_enterprise_plugin_v1\0")
            .map_err(|error| io::Error::other(error.to_string()))?;
        let api_ptr = entry();
        if api_ptr.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "plugin retornou ABI nula",
            ));
        }
        validate_api(&*api_ptr)?;
        let api: &'static PluginApiV1 = &*api_ptr;
        let create = api.create.expect("validate_api garantiu create");
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
                "plugin recusou configuração",
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
        let ingest = self.api.ingest.expect("validate_api garantiu ingest");
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
            Err(io::Error::other(format!("plugin retornou status {status}")))
        }
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
                return error_response(request, "plugin Enterprise não carregado");
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
                return error_response(request, "plugin Enterprise não carregado");
            };
            let event = request.payload.to_string();
            match plugin.ingest(event.as_bytes()) {
                Ok(()) => response(request, serde_json::json!({ "status": "accepted" })),
                Err(error) => error_response(request, error.to_string()),
            }
        }
        _ => error_response(request, "método IPC desconhecido"),
    }
}

fn run() -> io::Result<()> {
    let plugin_path = std::env::args_os().skip(1).collect::<Vec<_>>();
    let plugin = plugin_path.first().map(PathBuf::from);
    let loaded = plugin
        .as_deref()
        .map(|path| unsafe { LoadedPlugin::load(path, b"{}") })
        .transpose()?;
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
    fn ipc_rejects_frames_over_the_protocol_limit() {
        let mut bytes = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes().to_vec();
        bytes.extend(std::iter::repeat_n(b'x', MAX_FRAME_BYTES + 1));

        let error = read_frame::<IpcRequest>(&mut Cursor::new(bytes)).expect_err("oversized frame");

        assert!(error.to_string().contains("limite"));
    }

    #[test]
    fn loader_rejects_relative_plugin_paths_before_dynamic_loading() {
        let error =
            validate_plugin_path(std::path::Path::new("enterprise.so")).expect_err("relative path");

        assert!(error.to_string().contains("absoluto"));
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
