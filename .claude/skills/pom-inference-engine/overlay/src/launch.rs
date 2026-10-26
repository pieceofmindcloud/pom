//! What is particular to __NAME__: where its runtime is, how its server is
//! started for one deployment, and what it can do. Everything else in the
//! plugin (supervision, the POM generation route, the service, the status)
//! is engine-agnostic. Adapt this file first; `feature-levels.md` of the
//! `pom-inference-engine` skill lists what each constant unlocks.
//!
//! The default below starts an OpenAI-compatible server in the llama.cpp
//! `llama-server` style. Replace the program, the arguments and the health
//! path with the engine's own, keeping: loopback only, the port given here,
//! the model path the POM sends, and no secret on the command line.

use crate::engine::LoadRequest;
use std::path::{Path, PathBuf};

/// GGUF `general.architecture` values this engine runs; the POM's deploy
/// screen offers the service only for these. Empty = the engine reads no
/// GGUF and the service is offered for nothing until this is filled.
pub const ARCHITECTURES: &[&str] = &[];
/// The engine reads images through the deployment's projector (`mmproj_path`).
pub const VISION: bool = false;
/// The engine runs a POM layer slice as one stage of a split (feature L6).
pub const STAGES: bool = false;
/// POM settings that apply to this engine's deployments (`pom_settings`).
pub const POM_SETTINGS: &[&str] = &[];
/// How long a model may take to load before the load is an error.
pub const LOAD_TIMEOUT_SECS: u64 = 1800;
/// The environment variable an operator sets to a runtime built by hand.
pub const RUNTIME_ENV: &str = "POM___CODE_UPPER___RUNTIME_DIR";

/// One start of the engine's server.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Added to a cleared environment (`PATH` and `HOME` are kept).
    pub env: Vec<(String, String)>,
    /// A GET that answers 200 once the model is loaded.
    pub health_path: String,
    /// The model name the engine's OpenAI API expects in requests.
    pub served_model: String,
}

/// What a usable runtime directory holds.
pub fn is_runtime(dir: &Path) -> bool {
    dir.join("bin/server").is_file()
}

/// The runtime, first match wins: the operator's directory, then the
/// `build/runtime` of the source this library was built from (the POM's
/// development rebuild compiles the mounted source, so the node reads the
/// runtime straight from the mount), then the plugin directory's copy.
pub fn find_runtime(plugin_dir: &Path) -> Option<PathBuf> {
    let candidates = [
        std::env::var_os(RUNTIME_ENV).map(PathBuf::from),
        Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("build/runtime")),
        Some(plugin_dir.join("runtime")),
    ];
    candidates.into_iter().flatten().find(|dir| is_runtime(dir))
}

/// The version a runtime reports, for the dashboard (`RUNTIME.json`).
pub fn runtime_version(runtime: &Path) -> Option<String> {
    let text = std::fs::read_to_string(runtime.join("RUNTIME.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value["version"].as_str().map(str::to_owned)
}

pub fn command(runtime: &Path, request: &LoadRequest, port: u16) -> Result<Launch, String> {
    let mut args = vec![
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "--model".into(),
        request.model_path.display().to_string(),
        "--alias".into(),
        request.model_label.clone(),
        "--ctx-size".into(),
        request.context.to_string(),
        "--parallel".into(),
        request.sessions.to_string(),
    ];
    if let Some(mmproj) = request.mmproj_path.as_ref().filter(|_| VISION) {
        args.extend(["--mmproj".into(), mmproj.display().to_string()]);
    }
    // The deployment's engine settings (`settings.rs`) map to flags here.
    for (key, value) in &request.settings {
        let value = match value {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        args.extend([format!("--{}", key.replace('_', "-")), value]);
    }
    let mut env = vec![(
        "LD_LIBRARY_PATH".into(),
        runtime.join("lib").display().to_string(),
    )];
    if !request.gpus.is_empty() {
        let gpus: Vec<String> = request.gpus.iter().map(u32::to_string).collect();
        env.push(("CUDA_VISIBLE_DEVICES".into(), gpus.join(",")));
    }
    Ok(Launch {
        program: runtime.join("bin/server"),
        args,
        env,
        health_path: "/health".into(),
        served_model: request.model_label.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_server_listens_on_loopback_on_the_given_port() {
        let request =
            LoadRequest::from_json(&json!({"model_path": "/models/m.gguf", "gpus": [1]})).unwrap();
        let launch = command(Path::new("/rt"), &request, 4242).unwrap();
        let args = launch.args.join(" ");
        assert!(args.contains("--host 127.0.0.1 --port 4242"));
        assert!(args.contains("--model /models/m.gguf"));
        assert!(launch
            .env
            .contains(&("CUDA_VISIBLE_DEVICES".into(), "1".into())));
        assert!(!args.contains("key"), "no secret on the command line");
    }
}
