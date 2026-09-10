//! Feeding the prompt through the pipeline in chunks.
//!
//! The prompt used to cross as one block: peer 1 decoded every token in a
//! single `llama_batch` and shipped the whole hidden-state tensor to peer 2.
//! That made memory scale with the prompt on both sides, forced the prompt to
//! fit in `n_batch`, and put a multi-gigabyte body on the wire — on an internet
//! link, a request that dies.
//!
//! Here the prompt advances in chunks. Each chunk is decoded at its own
//! position and its hidden state travels on its own, which means:
//!
//! * memory stops scaling with the prompt;
//! * a lost chunk costs one chunk, not the request — it is simply resent, and
//!   the receiving peer rewinds its KV cache to that chunk's position so a
//!   repeat is never counted twice;
//! * the prompt no longer has to fit in a single batch.
//!
//! What travels is defined in [`crate::wire`], where precision and encoding are
//! a parameter rather than an assumption.

pub(crate) use crate::infer_server::Speculate;
pub(crate) use crate::infer_tunnel::TunnelOutboundSender;
pub(crate) use crate::pipeline::PipelinePlan;
pub(crate) use crate::relay::CHUNK_ATTEMPTS;
pub(crate) use crate::wire::{FrameEncoding, TensorShape, WireDtype};
pub(crate) use anyhow::{anyhow, Result};
pub(crate) use futures::stream::{FuturesOrdered, StreamExt};
pub(crate) use serde::Serialize;
pub(crate) use serde_json::json;
pub(crate) use std::future::Future;
pub(crate) use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
pub(crate) use std::time::Instant;
pub(crate) use tokio::sync::{mpsc, Notify, OwnedSemaphorePermit, Semaphore};
pub(crate) use tracing::info;

#[path = "pipeline.rs"]
mod pipeline;
#[path = "step.rs"]
mod step;
#[path = "support.rs"]
mod support;
#[path = "tests.rs"]
mod tests;

pub(super) use pipeline::*;
pub(super) use step::*;
pub(super) use support::*;
