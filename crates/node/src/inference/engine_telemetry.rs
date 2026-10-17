//! What the POM measures about a plugin's generation, whatever the engine is. A plugin reports only its basic data
//! when its generation ends (the `done` event: token counts, the prompt and decode times it knows, the finish).
//! Everything else is the POM's: the stage links between machines (their time, the wire, the bytes, the channel
//! they used), the transport summary of each hop, the trace the request audit and the admins read, and the speed of
//! each plugin's answers, which the `/health` publishes.
//!
//! When a plugin does not report a field itself, the llama.cpp server's `timings` object is read in its place, so a
//! server that already speaks that needs no adapter.

use super::plugin_stage_link::{take_own_wait, LinkPhase};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// The basic data a plugin reports when its generation ends.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EngineReport {
    pub(crate) prompt_tokens: u32,
    pub(crate) cached_tokens: u32,
    pub(crate) generated: u32,
    /// The engine's prompt and decode times in ms, when it measured them.
    pub(crate) prefill_ms: Option<f64>,
    pub(crate) decode_ms: Option<f64>,
    /// The tokens the prompt time and the decode time cover.
    pub(crate) prompt_n: u64,
    pub(crate) decode_n: u64,
    /// Speculative drafts the engine proposed and the ones it kept, when it speculates.
    pub(crate) draft_proposed: u64,
    pub(crate) draft_accepted: u64,
    pub(crate) finish: String,
    pub(crate) stop_sequence: Option<String>,
}

impl EngineReport {
    /// Reads the basic data from a plugin's `done` event. `pieces` counts the text pieces streamed, for an engine
    /// that does not count its generated tokens.
    pub(crate) fn from_done(done: &Value, pieces: u32) -> Self {
        let timings = &done["timings"];
        let count = |key: &str| done[key].as_u64();
        let time = |key: &str, fallback: &str| {
            done[key]
                .as_f64()
                .or_else(|| timings[fallback].as_f64())
                .filter(|ms| ms.is_finite() && *ms >= 0.0)
        };
        let number = |fallback: &str, key: &str| {
            timings[fallback]
                .as_u64()
                .or_else(|| done[key].as_u64())
                .unwrap_or(0)
        };
        Self {
            prompt_tokens: count("prompt_tokens").unwrap_or(0) as u32,
            cached_tokens: count("cached_tokens").unwrap_or(0) as u32,
            generated: count("completion_tokens")
                .map(|count| count as u32)
                .unwrap_or(pieces),
            prefill_ms: time("prefill_ms", "prompt_ms"),
            decode_ms: time("decode_ms", "predicted_ms"),
            prompt_n: number("prompt_n", "prompt_n"),
            decode_n: number("predicted_n", "decode_n"),
            draft_proposed: number("draft_n", "draft_proposed"),
            draft_accepted: number("draft_n_accepted", "draft_accepted"),
            finish: done["finish"].as_str().unwrap_or("stop").to_owned(),
            stop_sequence: done["stop_sequence"].as_str().map(str::to_owned),
        }
    }
}

/// The speed a plugin's answers measured on its last generations, as the `/health` publishes it (`perf`, the same
/// fields a llama.cpp server gives). An exponential average, so one long answer does not decide it.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct EnginePerf {
    pub(crate) prefill_ms_per_token: Option<f64>,
    pub(crate) decode_ms_per_step: Option<f64>,
}

fn engine_perf() -> &'static Mutex<HashMap<String, EnginePerf>> {
    static PERF: OnceLock<Mutex<HashMap<String, EnginePerf>>> = OnceLock::new();
    PERF.get_or_init(|| Mutex::new(HashMap::new()))
}

fn blend(previous: Option<f64>, sample: f64) -> f64 {
    previous.map_or(sample, |previous| previous * 0.7 + sample * 0.3)
}

/// Folds one sample of milliseconds per token into the plugin's speed for `phase`.
fn fold(code: &str, phase: LinkPhase, sample: f64) {
    let Ok(mut perf) = engine_perf().lock() else {
        return;
    };
    let entry = perf.entry(code.to_owned()).or_default();
    match phase {
        LinkPhase::Prefill => entry.prefill_ms_per_token = Some(blend(entry.prefill_ms_per_token, sample)),
        LinkPhase::Decode => entry.decode_ms_per_step = Some(blend(entry.decode_ms_per_step, sample)),
    }
}

/// One frame this machine answered: its own work on it (its time here minus the waits its relays made), per token
/// of the frame. Every stage measures itself this way, so the speeds of the machines compare.
pub(crate) fn record_frame(code: &str, phase: LinkPhase, own_ms: f64, tokens: u32) {
    if own_ms > 0.0 && tokens > 0 {
        fold(code, phase, own_ms / f64::from(tokens));
    }
}

/// The head's speed for one generation: its engine's prompt and decode times, less the time it waited on the stages
/// below it, per token. The waits are taken here, so each generation counts once.
pub(crate) fn record_head_perf(code: &str, report: &EngineReport) {
    let prefill_wait = take_own_wait(code, LinkPhase::Prefill) as f64;
    let decode_wait = take_own_wait(code, LinkPhase::Decode) as f64;
    if let Some(ms) = report.prefill_ms.map(|ms| ms - prefill_wait) {
        if ms > 0.0 && report.prompt_n > 0 {
            fold(code, LinkPhase::Prefill, ms / report.prompt_n as f64);
        }
    }
    if let Some(ms) = report.decode_ms.map(|ms| ms - decode_wait) {
        if ms > 0.0 && report.decode_n > 0 {
            fold(code, LinkPhase::Decode, ms / report.decode_n as f64);
        }
    }
}

/// The speed measured so far for `code`, for the `/health`.
pub(crate) fn perf_for(code: &str) -> EnginePerf {
    engine_perf()
        .lock()
        .ok()
        .and_then(|perf| perf.get(code).copied())
        .unwrap_or_default()
}

fn in_flight_counts() -> &'static Mutex<HashMap<String, u32>> {
    static COUNTS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The generations this machine is serving for `code` right now: the `active_generations` its `/health` publishes.
pub(crate) fn in_flight(code: &str) -> u32 {
    in_flight_counts()
        .lock()
        .ok()
        .and_then(|counts| counts.get(code).copied())
        .unwrap_or(0)
}

/// One generation this machine serves, counted from its start until its stream ends or is dropped.
struct InFlight {
    code: String,
}

impl InFlight {
    fn start(code: &str) -> Self {
        if let Ok(mut counts) = in_flight_counts().lock() {
            *counts.entry(code.to_owned()).or_insert(0) += 1;
        }
        Self {
            code: code.to_owned(),
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut counts) = in_flight_counts().lock() {
            if let Some(count) = counts.get_mut(&self.code) {
                *count = count.saturating_sub(1);
            }
        }
    }
}

/// The stream of a generation this machine serves, counted as in flight until it ends (or the reader drops it).
pub(crate) fn track<S>(
    code: &str,
    upstream: S,
) -> impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static
where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin + Send + 'static,
{
    use futures::StreamExt;
    let guard = InFlight::start(code);
    futures::stream::unfold((upstream, guard), |(mut upstream, guard)| async move {
        let next = upstream.next().await?;
        Some((next, (upstream, guard)))
    })
}

/// The gateway side: the stage links the head reported in its `done` event join this machine's totals.
pub(crate) fn merge_reported_links(code: &str, done: &Value) {
    let Some(links) = done.get("links") else {
        return;
    };
    let Ok(links) =
        serde_json::from_value::<Vec<super::plugin_stage_link::ReportedLink>>(links.clone())
    else {
        return;
    };
    for link in links {
        super::plugin_stage_link::merge_reported_link(code, link);
    }
}

/// Puts the stage links of this request on the trace. Each machine's wire, round trips and bytes are recorded on its
/// hop, in the phase its frames belonged to. A machine's `processing_ms` is its own work: the time it served the
/// frames (reported in its answer) less the round trip to the machine below it, which the served time already
/// includes. The head serves the prompt, so its own prompt time is the engine's, less the same round trip.
pub(crate) fn apply_stage_links(trace: &mut crate::prefill::RequestTrace, code: &str, prompt_ms: u64) {
    use super::plugin_stage_link::{take_channel_stats, take_link_stats, LinkPhase};
    let links = take_link_stats(code);
    let ids: Vec<String> = trace.hops.iter().map(|hop| hop.node_id.clone()).collect();
    for (node_id, phase, link) in &links {
        let Some(hop) = trace.hops.iter_mut().find(|hop| &hop.node_id == node_id) else {
            continue;
        };
        let part = match phase {
            LinkPhase::Prefill => &mut hop.prefill,
            LinkPhase::Decode => &mut hop.decode,
        };
        part.reported_spans += u64::from(link.calls);
        part.round_trip_ms += link.round_trip_ms;
        part.network_ms += link.network_ms;
        part.bytes_sent += link.bytes_sent;
        part.bytes_received += link.bytes_received;
        part.hidden_state_bytes_sent += link.bytes_sent;
        part.hidden_state_bytes_received += link.bytes_received;
        trace.total_bytes_sent += link.bytes_sent;
        trace.total_bytes_received += link.bytes_received;
    }
    let sum = |node: &str, phase: LinkPhase, field: fn(&super::plugin_stage_link::LinkStats) -> u64| -> u64 {
        links
            .iter()
            .filter(|(n, p, _)| n == node && *p == phase)
            .map(|(_, _, link)| field(link))
            .sum()
    };
    let last = ids.len().saturating_sub(1);
    for (index, node_id) in ids.iter().enumerate() {
        for phase in [LinkPhase::Prefill, LinkPhase::Decode] {
            let below = match index.checked_sub(1) {
                Some(lower) => sum(&ids[lower], phase, |link| link.round_trip_ms),
                None => 0,
            };
            // The head's own time: its engine's prompt and decode times, less what it waited on below it.
            let own = if index == last {
                match phase {
                    LinkPhase::Prefill => prompt_ms,
                    LinkPhase::Decode => trace.hops[index].decode.node_decode_ms.round() as u64,
                }
                .saturating_sub(below)
            } else {
                sum(node_id, phase, |link| link.served_ms).saturating_sub(below)
            };
            let hop = &mut trace.hops[index];
            match phase {
                LinkPhase::Prefill => hop.prefill.processing_ms = own,
                LinkPhase::Decode => hop.decode.processing_ms = own,
            }
        }
    }
    // The channels the links used: each becomes one entry of its hop's transport summary, as the llama.cpp path has.
    for (node_id, phase, channel) in take_channel_stats(code) {
        let Some(hop) = trace.hops.iter_mut().find(|hop| hop.node_id == node_id) else {
            continue;
        };
        let part = match phase {
            LinkPhase::Prefill => &mut hop.prefill,
            LinkPhase::Decode => &mut hop.decode,
        };
        part.transport_summary
            .transports
            .push(channel_entry(&channel));
        part.transport_summary.fallback_calls += channel.fallback_calls;
    }
}

/// One channel's entry in a hop's transport summary. The round trips and the network time are means (with the
/// maximum as the p95): the links report aggregates, not the sample list.
fn channel_entry(channel: &super::plugin_stage_link::ChannelStats) -> crate::prefill::TransportSummaryEntry {
    use crate::relay::InferenceTransport;
    let transport = match channel.channel.as_str() {
        "quic" => InferenceTransport::Quic,
        "tunnel" => InferenceTransport::P2pTunnel,
        "local" => InferenceTransport::Local,
        _ => InferenceTransport::Http,
    };
    let calls = u64::from(channel.calls.max(1));
    crate::prefill::TransportSummaryEntry::from_aggregates(
        transport,
        channel.calls,
        channel.bytes_sent,
        channel.bytes_received,
        channel.fallback_calls,
        (
            channel.round_trip_min_ms,
            channel.round_trip_sum_ms / calls,
            channel.round_trip_max_ms,
        ),
        (
            channel.network_min_ms,
            channel.network_sum_ms / calls,
            channel.network_max_ms,
        ),
    )
}

/// The trace of one generation: the hops of the plan, the phases each machine served, the drafts and the rounds,
/// with the stage links applied by the caller. `code` names the plugin the route goes through.
pub(crate) fn build_trace(
    trace_id: String,
    model: &str,
    plan: &crate::pipeline::PipelinePlan,
    code: &str,
    report: &EngineReport,
) -> crate::prefill::RequestTrace {
    let steps: Vec<(&str, u32, u32)> = plan
        .steps
        .iter()
        .map(|step| (step.node_id.as_str(), step.start_layer, step.end_layer))
        .collect();
    let mut trace = crate::prefill::RequestTrace::new_with_trace_id(trace_id, model, &steps);
    let proposed = report.draft_proposed;
    let accepted = report.draft_accepted.min(proposed);
    // A decode round yields one token plus the drafts it accepted; the first token comes from the prompt.
    let generated = report.generated;
    let rounds = u64::from(generated)
        .saturating_sub(accepted)
        .saturating_sub(1)
        .max(u64::from(generated > 0));
    let path = format!("plugin/{code}");
    let last = trace.hops.len().saturating_sub(1);
    for (index, hop) in trace.hops.iter_mut().enumerate() {
        for phase in [&mut hop.prefill, &mut hop.decode] {
            phase.executed_engine = crate::executor::ExecutionEngine::Plugin;
            phase.executed_engines = vec![crate::executor::ExecutionEngine::Plugin];
            phase.selected_path = Some(path.clone());
        }
        hop.prefill.calls = 1;
        hop.prefill.reported_spans = 1;
        hop.decode.calls = rounds.min(u64::from(u32::MAX)) as u32;
        hop.decode.reported_spans = rounds;
        if index == last {
            hop.decode.node_decode_ms = report.decode_ms.unwrap_or(0.0);
        }
    }
    for _ in 0..rounds {
        trace.note_decode_round();
    }
    trace.mtp_draft_tokens_proposed = proposed;
    trace.mtp_draft_tokens_accepted = accepted;
    trace.mtp_draft_tokens_rejected = proposed - accepted;
    trace.mtp_acceptance_rate = (proposed > 0).then(|| accepted as f64 / proposed as f64);
    trace
}

#[cfg(test)]
mod speed_tests {
    use super::*;

    fn report(prompt_n: u64, prefill_ms: f64, decode_n: u64, decode_ms: f64) -> EngineReport {
        EngineReport {
            prompt_n,
            prefill_ms: Some(prefill_ms),
            decode_n,
            decode_ms: Some(decode_ms),
            ..Default::default()
        }
    }

    #[test]
    fn engine_speed_is_a_blend_of_the_answers() {
        record_head_perf("strata-speed-test", &report(100, 1000.0, 50, 500.0));
        let first = perf_for("strata-speed-test");
        assert_eq!(first.prefill_ms_per_token, Some(10.0));
        assert_eq!(first.decode_ms_per_step, Some(10.0));
        // a slower answer moves the average by 30%, it does not replace it
        record_head_perf("strata-speed-test", &report(100, 2000.0, 50, 1500.0));
        let second = perf_for("strata-speed-test");
        assert!((second.prefill_ms_per_token.unwrap() - 13.0).abs() < 1e-9);
        assert!((second.decode_ms_per_step.unwrap() - 16.0).abs() < 1e-9);
    }

    #[test]
    fn a_stage_speed_is_its_own_work_per_token_of_each_frame() {
        record_frame("strata-stage-test", LinkPhase::Decode, 30.0, 3);
        let first = perf_for("strata-stage-test");
        assert_eq!(first.decode_ms_per_step, Some(10.0));
        assert_eq!(first.prefill_ms_per_token, None, "a phase nothing ran in is not measured");
        record_frame("strata-stage-test", LinkPhase::Decode, 0.0, 3);
        record_frame("strata-stage-test", LinkPhase::Decode, 9.0, 0);
        assert_eq!(perf_for("strata-stage-test").decode_ms_per_step, Some(10.0));
    }

    #[test]
    fn the_basic_report_reads_the_plugin_fields_and_falls_back_to_timings() {
        let own = EngineReport::from_done(
            &serde_json::json!({"prompt_tokens": 70, "cached_tokens": 4, "completion_tokens": 12,
                "prefill_ms": 800.0, "decode_ms": 400.0, "prompt_n": 66, "decode_n": 12,
                "draft_proposed": 9, "draft_accepted": 7, "finish": "length"}),
            0,
        );
        assert_eq!((own.prompt_tokens, own.cached_tokens, own.generated), (70, 4, 12));
        assert_eq!((own.prefill_ms, own.decode_ms), (Some(800.0), Some(400.0)));
        assert_eq!((own.draft_proposed, own.draft_accepted), (9, 7));
        assert_eq!(own.finish, "length");
        let llama = EngineReport::from_done(
            &serde_json::json!({"prompt_tokens": 64, "timings": {"prompt_ms": 1000.0,
                "predicted_ms": 2000.0, "prompt_n": 64, "predicted_n": 40, "draft_n": 5,
                "draft_n_accepted": 2}}),
            40,
        );
        assert_eq!((llama.prefill_ms, llama.decode_ms), (Some(1000.0), Some(2000.0)));
        assert_eq!((llama.prompt_n, llama.decode_n), (64, 40));
        assert_eq!((llama.draft_proposed, llama.draft_accepted), (5, 2));
        assert_eq!(llama.generated, 40, "the pieces count when the engine does not count its tokens");
    }

    #[test]
    fn a_served_generation_is_in_flight_until_its_stream_ends_or_is_dropped() {
        let code = "strata-flight-test";
        let chunks = futures::stream::iter(vec![Ok::<bytes::Bytes, reqwest::Error>(
            bytes::Bytes::from_static(b"a"),
        )]);
        let tracked = track(code, chunks);
        assert_eq!(in_flight(code), 1);
        drop(tracked);
        assert_eq!(in_flight(code), 0);
    }
}
