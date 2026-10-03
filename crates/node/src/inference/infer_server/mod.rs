//! HTTP inference server — the Rust replacement for `inference_server.py`.
//!
//! Exposes:
//!
//!   POST /v1/prefill          — one chunk of the prompt
//!   POST /v1/step             — one autoregressive token step (optionally
//!                                speculative — see `step_blocking`)
//!   POST /v1/step-stream      - remote decode stream with opportunistic batches
//!   POST /v1/session/release  — free a session's KV slot
//!   POST /v1/session/rewind   - keep the prompt and discard generated suffix
//!   POST /v1/session/rollback — discard an unconfirmed speculative batch
//!   POST /v1/session/slot     - save/restore/erase a KV cache snapshot on disk
//!                                (`--slot-save-path`, like llama-server)
//!   POST /v1/session/cache/clear - drop the host-memory KV cache (`--cache-ram`)
//!   GET  /health              — readiness probe
//!
//! Wire format is defined in [`crate::wire`] (binary framing, f16 on the wire).
//! Decode calls run in `spawn_blocking` so they don't starve the tokio runtime.

pub(crate) use crate::llm::{sample_token_profiled, LlmEngine, ModelCache, SamplingConfig};
pub(crate) use crate::pipeline::PipelineStep;
pub(crate) use crate::runtime::NodeConfig;
pub(crate) use crate::wire::{self, TensorShape, WireDtype};
pub(crate) use anyhow::{bail, Context, Result};
pub(crate) use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
pub(crate) use serde::Deserialize;
pub(crate) use serde_json::{json, Value};
pub(crate) use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
pub(crate) use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};
pub(crate) use tokio::net::TcpListener;
pub(crate) use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
pub(crate) use tracing::{debug, info, warn};

#[path = "core.rs"]
mod core;
#[path = "direct.rs"]
mod direct;
#[path = "perf.rs"]
mod perf;
#[path = "prefill.rs"]
mod prefill;
#[path = "prefix.rs"]
mod prefix;
#[path = "ram_cache.rs"]
mod ram_cache;
#[path = "server.rs"]
mod server;
#[path = "step.rs"]
mod step;
#[path = "step_blocking.rs"]
mod step_blocking;

pub(crate) use core::DirectForwardOrder;
pub(super) use core::*;
pub(crate) use direct::forward_direct_ordered;
pub(super) use direct::*;
pub(super) use prefill::*;
pub(super) use prefix::*;
use ram_cache::*;
pub(crate) use server::http_client;
pub(super) use server::*;
pub(super) use step::*;
pub(super) use step_blocking::*;
