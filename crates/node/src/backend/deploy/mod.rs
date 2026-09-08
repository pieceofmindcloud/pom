//! Putting a model that is already sliced into service across the cluster.
//!
//! The pieces all existed and none of them was the path: the catalog knows who
//! holds which slice, `transfer` moves a file between two machines, and `launch`
//! binds a range to a peer. What was missing was the decision in the middle —
//! *which* machine should serve *which* slice, and what has to be copied first
//! for that to be possible. This module is only that decision, plus the
//! sequencing to carry it out; it starts no transfer mechanism and no launch
//! mechanism of its own.
//!
//! Two routes, deliberately split the way the slicing screen already splits:
//! `plan` is read-only and cheap enough for the screen to call while the
//! operator is still choosing, and `deploy` re-surveys the cluster rather than
//! trusting the preview it was handed.
//!
//! ## The operator's choice is part of the request
//!
//! The planner is a *suggestion*, not the decision. `selection` carries the
//! operator's own answer to the two questions the planner guesses at — which
//! ranges to put on the wire, and which machine takes each one — and everything
//! below validates that answer instead of quietly recomputing its own. Omitting
//! `selection` keeps the old behaviour exactly: the node plans it, and the
//! screen shows what it planned.
//!
//! Three things fall out of the architecture rather than out of taste, and the
//! validation leans on all three:
//!
//! * **The pipeline order is the layer order.** `pipeline.rs` sweeps from layer
//!   0 upwards, so the machine holding layer 0 is the one that receives every
//!   prompt. "Choosing the order" is therefore the same act as choosing who
//!   takes which range — there is no second knob, and pretending otherwise
//!   would be a lie on screen.
//! * **A node may serve multiple ranges.** Each selected range remains a
//!   pipeline step, even when the same machine owns separated ranges. When
//!   `merge` is on, adjacent ranges on the same machine become one *merged
//!   unit*: the machine joins the slices into a single file (`splitter::merge`)
//!   and serves the union. A merged step carries `parts`, one per slice, each
//!   with its own source.
//!   The same exact range on different machines is an allowed replica.
//! * **Overlap is not a preference.** Two different cuts covering the same
//!   layer would make the pipeline ambiguous, so that overlap is refused. The
//!   same exact range on different machines is an explicit parallel route. A
//!   *gap* is different: it makes the global coverage incomplete and therefore
//!   refuses the pipeline before launch.
//!
//! ## A whole model is the degenerate case, not a second path
//!
//! `kind: "whole"` deploys one un-sliced GGUF onto one machine. Everything
//! above still applies, because the survey reads that file as a single held
//! range covering layer 0 to the last one: the coverage arithmetic finds it
//! complete by construction, and the transfer and launch phases are the ones
//! the slice path
//! already uses. What changes is the wording of the refusals — "a fatia L0-31"
//! is not what an operator who picked a whole model is looking at.

use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{info, warn};

use crate::auth::ConnectionAuth;
use crate::gateway::GatewayState;
use crate::network::PeerInfo;
use crate::plan::DEFAULT_RESERVE_PCT;
use crate::webui::{
    jobs, launch, new_job, set_job, set_job_progress, CapacityPreview, CapacityPreviewRequest,
    InferenceMode, LaunchRequest, LaunchTarget, LayerCut, MergeSource, ModelLayerOptions,
    ReconfigureSlice, RuntimeParams, SliceRange, SliceRangesRequest, PEER_TIMEOUT,
};

const MIB: u64 = 1024 * 1024;

#[path = "routes.rs"] mod routes_impl;
mod types;
mod survey;
mod planning;
mod preview;
mod launch;
mod lifecycle;
mod node;

pub use routes_impl::*;
pub use types::*;
use survey::*;
#[cfg(test)]
pub(crate) use survey::survey_for_tests;
pub use planning::*;
use preview::*;
use launch::*;
pub use lifecycle::*;
pub use node::*;

include!("tests/deployments.rs");
include!("tests/whole_route.rs");
include!("tests/undeploy.rs");
include!("tests/redeploy_preflight.rs");
#[cfg(test)]
mod tests {
    include!("tests/core_a.rs");
    include!("tests/core_b.rs");
}
