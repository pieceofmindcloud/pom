import clsx from "clsx";
import { Sparkles } from "lucide-react";
import { Button } from "./ui";
import {
  analyzeTelemetry,
  decodeCoverageMeasured,
  effectivePlacementCounts,
  executionNetworkMs,
  executionPipelineEdges,
  executionRttMs,
} from "../lib/telemetryAnalysis";
import { bytes, ms, num } from "../lib/format";
import { isPhasedHop, type GenerationTimings, type LegacyHopTrace, type PhasedHopTrace, type PhaseTrace, type RequestTrace, type TraceSpan } from "../lib/chatStream";
import type { ChatTelemetryNetwork, ClusterView, ExecutionEngine, PeerRuntime } from "../lib/types";
import { useI18n } from "../i18n/provider";

export interface ExecutionTelemetryEnvironment {
  snapshotCaptured: boolean;
  cluster?: ClusterView | null;
  network?: ChatTelemetryNetwork;
  networkLoading?: boolean;
  networkError?: boolean;
  runtimes: Record<string, PeerRuntime | undefined>;
  runtimesLoading?: boolean;
}

function metricRate(
  value: number | null | undefined,
  t: ReturnType<typeof useI18n>["t"],
) {
  return value == null ? t("chat.rateUnavailable") : `${num(value, 1)} ${t("chat.tokensPerSecondUnit")}`;
}

export function TimingSummary({
  timings,
  trace,
}: {
  timings: GenerationTimings;
  trace?: RequestTrace;
}) {
  const { t } = useI18n();
  const timing = (value: number | null | undefined) => value == null ? t("chat.telemetry.notMeasured") : ms(value);
  const pipelineTtft = timings.pipeline_first_token_ms ?? timings.ttft_ms;
  const networkTotalMs = trace ? executionNetworkMs(trace) : null;
  const rttTotalMs = trace ? executionRttMs(trace) : null;
  return (
    <div className="mt-2 grid max-w-[38rem] grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-1 text-[0.68rem]">
      <span className="text-faint">{t("chat.generationSpeed")}</span>
      <span className="font-mono text-muted">{metricRate(timings.generation_tokens_per_sec, t)} · {timings.generation_tokens} {t("chat.tokensUnit")}</span>
      <span className="text-faint">{t("chat.prefillSpeed")}</span>
      <span className="font-mono text-muted">{metricRate(timings.prefill_tokens_per_sec, t)} · {timings.prefill_tokens} {t("chat.promptTokensUnit")}</span>
      <span className="text-faint">{t("chat.ttft")}</span>
      <span className="font-mono text-muted">{ms(timings.ttft_ms)}</span>
      <span className="text-faint">{t("chat.ttftPipeline")}</span>
      <span className="font-mono text-muted">{timing(pipelineTtft)}</span>
      <span className="text-faint">{t("chat.telemetry.serverTtft")}</span>
      <span className="font-mono text-muted">{timing(timings.server_ttft_ms)}</span>
      <span className="text-faint">{t("chat.telemetry.engineTtft")}</span>
      <span className="font-mono text-muted">{timing(timings.engine_ttft_ms)}</span>
      <span className="text-faint">{t("chat.ttftClient")}</span>
      <span className="font-mono text-muted">{timing(timings.client_ttft_ms)}</span>
      <span className="text-faint">{t("chat.totalTime")}</span>
      <span className="font-mono text-muted">{ms(timings.total_ms)} · {num(timings.total_tokens_per_sec, 1)} {t("chat.tokensPerSecondUnit")}</span>
      {trace && (
        <span className="col-span-2 font-mono text-muted">
          {networkTotalMs == null ? t("chat.telemetry.networkTotalMissing") : t("chat.telemetry.networkTotal", { ms: networkTotalMs })}
        </span>
      )}
      {trace && (
        <span className="col-span-2 font-mono text-muted">
          {rttTotalMs == null ? t("chat.telemetry.rttTotalMissing") : t("chat.telemetry.rttTotal", { ms: rttTotalMs })}
        </span>
      )}
    </div>
  );
}

function DistributedTelemetry({ trace }: { trace: RequestTrace }) {
  const { t } = useI18n();
  const telemetry = trace.telemetry;
  if (!telemetry) return null;
  const status = t(`chat.telemetry.status.${telemetry.status}`);
  const remote = telemetry.remote_stream;
  const hasRemote = remote.frames_received > 0 || remote.open_ms != null;
  return (
    <section className="mt-3 rounded-lg border border-line/70 bg-surface/30 px-3 py-2.5">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <h3 className="font-medium text-fg">{t("chat.telemetry.distributedTitle")}</h3>
        <span className={clsx("font-mono text-[0.66rem]", telemetry.status === "complete" ? "text-emerald-300" : "text-amber-300")}>
          {t("chat.telemetry.statusLabel", { status })}
        </span>
      </div>
      <div className="mt-2 grid min-w-0 gap-x-4 gap-y-1 text-[0.68rem] sm:grid-cols-2">
        <span className="text-faint">{t("chat.telemetry.peersCoverage")}</span>
        <span className="font-mono text-muted">{telemetry.reported_peers}/{telemetry.expected_peers}</span>
        <span className="text-faint">{t("chat.telemetry.decodeCoverage")}</span>
        <span className="font-mono text-muted">{decodeCoverageMeasured(telemetry)
          ? `${telemetry.reported_decode_spans}/${telemetry.expected_decode_spans} · ${num(telemetry.completeness_percent, 1)}%`
          : t("chat.telemetry.notMeasured")}</span>
        <span className="text-faint">{t("chat.telemetry.criticalPath")}</span>
        <span className="font-mono text-muted">{telemetry.critical_path_ms == null ? t("chat.telemetry.noMeasurement") : ms(telemetry.critical_path_ms)}</span>
        <span className="text-faint">{t("chat.telemetry.accumulatedCompute")}</span>
        <span className="font-mono text-muted">{ms(telemetry.accumulated_compute_ms)}</span>
        <span className="text-faint">{t("chat.telemetry.accumulatedTransport")}</span>
        <span className="font-mono text-muted">{ms(telemetry.accumulated_transport_ms)}</span>
        <span className="text-faint">{t("chat.telemetry.overlapMeasured")}</span>
        <span className="font-mono text-muted">{telemetry.overlap_ms == null ? t("chat.telemetry.noMeasurement") : ms(telemetry.overlap_ms)}</span>
        <span className="text-faint">{t("chat.telemetry.trueResidual")}</span>
        <span className="font-mono text-muted">{telemetry.true_residual_ms == null ? t("chat.telemetry.noMeasurement") : ms(telemetry.true_residual_ms)}</span>
      </div>
      {telemetry.missing_peers.length > 0 && <p className="mt-2 break-words text-[0.68rem] text-amber-300">{t("chat.telemetry.missingPeers", { peers: telemetry.missing_peers.join(", ") })}</p>}
      {hasRemote && <div className="mt-2 border-t border-line/50 pt-2 text-[0.68rem]">
        <p className="font-medium uppercase tracking-[0.1em] text-faint">{t("chat.telemetry.remoteStreamTitle")}</p>
        <p className="mt-1 font-mono text-muted">{t("chat.telemetry.remoteStreamFrames", { received: remote.frames_received, sent: remote.frames_sent, bytes: bytes(remote.bytes_received) })}</p>
        <p className="font-mono text-muted">{t("chat.telemetry.remoteStreamLatency", { first: remote.first_frame_latency_ms ?? 0, p50: remote.frame_latency_p50_ms, p95: remote.frame_latency_p95_ms, max: remote.frame_latency_max_ms })}</p>
        <p className="font-mono text-muted">{t("chat.telemetry.remoteStreamCompute", { total: remote.remote_compute_total_ms, p50: remote.remote_compute_p50_ms, p95: remote.remote_compute_p95_ms })}</p>
        <p className="font-mono text-muted">{t("chat.telemetry.remoteStreamWait", { total: remote.remote_stream_wait_total_ms, p50: remote.remote_stream_wait_p50_ms, p95: remote.remote_stream_wait_p95_ms })}</p>
        <p className="font-mono text-muted">{t("chat.telemetry.remoteStreamWaitSplit", { producerTotal: remote.remote_stream_producer_wait_total_ms ?? 0, producerP95: remote.remote_stream_producer_wait_p95_ms ?? 0, channelTotal: remote.remote_stream_channel_send_wait_total_ms ?? 0, channelP95: remote.remote_stream_channel_send_wait_p95_ms ?? 0, downstreamTotal: remote.remote_stream_downstream_wait_total_ms ?? 0, downstreamP95: remote.remote_stream_downstream_wait_p95_ms ?? 0, queueTotal: remote.remote_stream_queue_wait_total_ms ?? 0, queueP95: remote.remote_stream_queue_wait_p95_ms ?? 0, deliveryTotal: remote.remote_stream_delivery_total_ms ?? 0, deliveryP95: remote.remote_stream_delivery_p95_ms ?? 0 })}</p>
        {(remote.token_delivery_interval_samples ?? 0) > 0 && <p className="font-mono text-muted">{t("chat.telemetry.remoteStreamTokenDelivery", { samples: remote.token_delivery_interval_samples ?? 0, p50: remote.token_delivery_interval_p50_ms ?? 0, p95: remote.token_delivery_interval_p95_ms ?? 0, p99: remote.token_delivery_interval_p99_ms ?? 0, max: remote.token_delivery_interval_max_ms ?? 0 })}</p>}
      </div>}
      {(telemetry.duplicate_spans > 0 || telemetry.late_spans > 0 || telemetry.orphan_spans > 0) && <p className="mt-2 font-mono text-[0.66rem] text-faint">{t("chat.telemetry.integrity", { duplicates: telemetry.duplicate_spans, late: telemetry.late_spans, orphan: telemetry.orphan_spans })}</p>}
    </section>
  );
}

function memoryLabel(value: number | undefined, locale: string) {
  if (!Number.isFinite(value) || !value || value < 0) return null;
  if (value >= 1024) {
    return `${new Intl.NumberFormat(locale === "pt-BR" ? "pt-BR" : "en-US", { maximumFractionDigits: 1 }).format(value / 1024)} GB`;
  }
  return `${new Intl.NumberFormat(locale === "pt-BR" ? "pt-BR" : "en-US").format(Math.round(value))} MB`;
}

function rttLabel(value: number, locale: string) {
  if (!Number.isFinite(value)) return null;
  return `${new Intl.NumberFormat(locale === "pt-BR" ? "pt-BR" : "en-US", { maximumFractionDigits: 1 }).format(value)} ms`;
}

function engineLabel(
  t: ReturnType<typeof useI18n>["t"],
  engine: ExecutionEngine | undefined,
) {
  switch (engine) {
    case "llama.cpp": return t("chat.telemetry.engineLlamaCpp");
    case "moe": return t("chat.telemetry.engineMoe");
    default: return t("chat.telemetry.engineUnknown");
  }
}

function executionLabel(
  t: ReturnType<typeof useI18n>["t"],
  phase: PhaseTrace,
) {
  const engine = engineLabel(t, phase.executed_engine);
  if (phase.mixed_execution) {
    const engines = (phase.executed_engines ?? []).map((value) => engineLabel(t, value)).join(" + ");
    return `${t("chat.telemetry.engineMixed")}: ${engines || engine}`;
  }
  if (phase.engine_mismatch) {
    return `${engine} · ${t("chat.telemetry.engineMismatch")}`;
  }
  return engine;
}

function phasePathLabel(
  t: ReturnType<typeof useI18n>["t"],
  selectedPath: string | undefined,
) {
  return t("chat.telemetry.phasePath", {
    value: selectedPath ?? t("chat.telemetry.pathNotReported"),
  });
}

function executionPathsForNode(trace: RequestTrace, nodeId: string) {
  const paths = trace.hops
    .filter((hop) => hop.node_id === nodeId)
    .flatMap((hop) => isPhasedHop(hop)
      ? [hop.prefill.selected_path, hop.decode.selected_path]
      : [hop.selected_path])
    .filter((path): path is string => typeof path === "string" && path.length > 0);
  return Array.from(new Set(paths));
}

function executionPathLabel(
  t: ReturnType<typeof useI18n>["t"],
  paths: string[],
) {
  if (paths.length === 0) return t("chat.telemetry.pathNotReported");
  if (paths.length > 1) return t("chat.telemetry.pathMixed");
  return paths[0];
}

function ExecutionNetwork({
  trace,
  environment,
}: {
  trace: RequestTrace;
  environment: ExecutionTelemetryEnvironment;
}) {
  const { t, locale } = useI18n();
  const edges = executionPipelineEdges(trace);
  const measurements = environment.network?.measurements ?? [];
  return (
    <section className="mt-3 rounded-lg border border-line/70 bg-surface/30 px-3 py-2.5">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <h3 className="font-medium text-fg">{t("chat.telemetry.pipelineTitle")}</h3>
        <span className="text-[0.66rem] text-faint">{t("chat.telemetry.pipelineRttTitle")}</span>
      </div>
      <p className="mt-1 text-[0.68rem] leading-relaxed text-faint">{t("chat.telemetry.pipelineRttExplain")}</p>
      {edges.length === 0 ? (
        <p className="mt-2 text-[0.72rem] text-muted">{t("chat.telemetry.noEdges")}</p>
      ) : environment.networkLoading ? (
        <p className="mt-2 text-[0.72rem] text-muted">{t("chat.telemetry.networkLoading")}</p>
      ) : environment.networkError || !environment.network ? (
        <p className="mt-2 text-[0.72rem] text-muted">{t("chat.telemetry.networkUnavailable")}</p>
      ) : (
        <div className="mt-2 grid min-w-0 gap-1.5">
          {edges.map((edge) => {
            const measurement = measurements.find(
              (item) => item.from_node_id === edge.from_node_id && item.to_node_id === edge.to_node_id,
            );
            return (
              <div key={`${edge.from_node_id}:${edge.to_node_id}`} className="flex min-w-0 flex-wrap items-baseline justify-between gap-x-3 gap-y-1 rounded-md border border-line/50 px-2.5 py-1.5">
                <span className="min-w-0 truncate font-mono text-[0.68rem] text-fg">
                  {edge.from_node_id.slice(0, 10)} <span className="text-faint">-&gt;</span> {edge.to_node_id.slice(0, 10)}
                </span>
                <span className="shrink-0 font-mono text-[0.68rem] text-muted">
                  {(measurement && rttLabel(measurement.rtt_ms, locale)) || t("chat.telemetry.noMeasurement")}
                </span>
                {measurement && <span className="w-full text-[0.64rem] text-faint">{t("chat.telemetry.transportValue", { value: measurement.transport })}</span>}
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}

function ExecutionEnvironment({
  trace,
  environment,
}: {
  trace: RequestTrace;
  environment: ExecutionTelemetryEnvironment;
}) {
  const { t, locale } = useI18n();
  const nodeIds = Array.from(new Set(trace.hops.map((hop) => hop.node_id)));
  const hasTp = trace.hops.some((hop) => isPhasedHop(hop) && (hop.prefill.tp?.enabled || hop.decode.tp?.enabled));
  return (
    <section className="mt-3 rounded-lg border border-line/70 bg-surface/30 px-3 py-2.5">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <h3 className="font-medium text-fg">{t("chat.telemetry.environmentTitle")}</h3>
        <span className="text-[0.66rem] text-faint">{t("chat.telemetry.machineCount", { count: nodeIds.length })}</span>
      </div>
      <p className="mt-1 text-[0.68rem] leading-relaxed text-faint">{t("chat.telemetry.environmentScopeExplain")}</p>
      {!environment.snapshotCaptured ? (
        <p className="mt-2 text-[0.72rem] text-muted">{t("chat.telemetry.snapshotMissing")}</p>
      ) : (
        <>
          {hasTp && <p className="mt-2 text-[0.68rem] leading-relaxed text-faint">{t("chat.telemetry.tpMachineScope")}</p>}
          <div className="mt-2 grid min-w-0 gap-2 lg:grid-cols-2">
            {nodeIds.map((nodeId) => {
              const peer = environment.cluster?.peers.find((candidate) => candidate.node_id === nodeId);
              const node = peer?.node;
              const runtime = environment.runtimes[nodeId];
              const health = runtime?.loaded ? runtime.health : undefined;
              const system = node?.system;
              const config = node?.config;
              const gpuNames = system?.gpus?.map((gpu) => gpu.name).filter(Boolean) ?? [];
              const gpuValue = gpuNames.length > 0 ? gpuNames.join(", ") : system?.gpu_kind || null;
              const placement = health?.placement;
              const counts = placement ? effectivePlacementCounts(placement.layers) : null;
              const currentPath = health?.selected_path;
              const currentPathLabel = currentPath ?? t("chat.telemetry.pathNotReported");
              const observedPaths = executionPathsForNode(trace, nodeId);
              const observedPath = executionPathLabel(t, observedPaths);
              const pathMismatch = currentPath != null
                && observedPaths.length === 1
                && observedPaths[0] !== currentPath;
              const syncState = typeof health?.profile_sync_enabled === "boolean"
                ? health.profile_sync_enabled
                  ? t("chat.telemetry.profileSyncActive")
                  : t("chat.telemetry.profileSyncOff")
                : t("chat.telemetry.profileSyncUnknown");
              return (
                <article key={nodeId} className="min-w-0 rounded-md border border-line/60 bg-raised/40 px-3 py-2.5">
                  <div className="flex min-w-0 items-start justify-between gap-2">
                    <div className="min-w-0">
                      <h4 className="truncate font-medium text-fg">{node?.hostname || peer?.short_id || nodeId.slice(0, 10)}</h4>
                      <p className="truncate font-mono text-[0.64rem] text-faint">{nodeId}</p>
                    </div>
                    <div className="flex shrink-0 flex-wrap justify-end gap-1">
                      <span className="rounded-full border border-line px-2 py-0.5 text-[0.62rem] text-faint">{t("chat.telemetry.currentPath", { value: currentPathLabel })}</span>
                      <span className="rounded-full border border-line px-2 py-0.5 text-[0.62rem] text-faint">{t("chat.telemetry.observedPath", { value: observedPath })}</span>
                      <span className="rounded-full border border-line px-2 py-0.5 text-[0.62rem] text-faint">{t("chat.telemetry.profileSync", { state: syncState })}</span>
                      {health?.moe_executor && <span className="rounded-full border border-line px-2 py-0.5 text-[0.62rem] text-faint">{t("chat.telemetry.moeExecutor", { value: health.moe_executor })}</span>}
                    </div>
                  </div>
                  {pathMismatch && <p className="mt-2 break-words text-amber-300">{t("chat.telemetry.environmentPathMismatch", { current: currentPath, observed: observedPaths[0] })}</p>}
                  <div className="mt-2 grid min-w-0 gap-2 text-[0.68rem] sm:grid-cols-2">
                    <div className="min-w-0">
                      <p className="mb-1 font-medium uppercase tracking-[0.1em] text-faint">{t("chat.telemetry.capacity")}</p>
                      <p>{system?.cpu_count != null ? t("chat.telemetry.cpu", { count: system.cpu_count }) : t("chat.telemetry.notInformed")}</p>
                      <p>{system?.ram ? t("chat.telemetry.ram", { available: memoryLabel(system.ram.available_mb, locale) ?? t("chat.telemetry.notInformed"), total: memoryLabel(system.ram.total_mb, locale) ?? t("chat.telemetry.notInformed") }) : t("chat.telemetry.ram", { available: t("chat.telemetry.notInformed"), total: t("chat.telemetry.notInformed") })}</p>
                      <p className="break-words">{t("chat.telemetry.gpu", { value: gpuValue ?? t("chat.telemetry.notInformed") })}</p>
                    </div>
                    <div className="min-w-0">
                      <p className="mb-1 font-medium uppercase tracking-[0.1em] text-faint">{t("chat.telemetry.configIntent")}</p>
                      {config ? (
                        <div className="space-y-0.5">
                          <p>{t("chat.telemetry.requestedGpuLayers", { count: config.n_gpu_layers })}</p>
                          <p>{t("chat.telemetry.nCtx", { count: config.n_ctx })} · {t("chat.telemetry.nBatch", { count: config.n_batch })}</p>
                          <p>{t("chat.telemetry.fitMemory", { value: config.fit_memory ? t("chat.telemetry.configOn") : t("chat.telemetry.configOff") })} · {t("chat.telemetry.offloadKqv", { value: config.offload_kqv ? t("chat.telemetry.configOn") : t("chat.telemetry.configOff") })}</p>
                          <p>{t("chat.telemetry.sessions", { count: config.max_sessions })}</p>
                        </div>
                      ) : <p>{t("chat.telemetry.notInformed")}</p>}
                    </div>
                  </div>
                  <div className="mt-2 border-t border-line/50 pt-2 text-[0.68rem]">
                    {health?.moe_executor_reason && <p className="mb-2 break-words text-amber-300">{t("chat.telemetry.moeExecutorReason", { reason: health.moe_executor_reason })}</p>}
                    <p className="mb-1 font-medium uppercase tracking-[0.1em] text-faint">{t("chat.telemetry.effectivePlacement")}</p>
                    {environment.runtimesLoading && !runtime ? (
                      <p className="text-muted">{t("chat.telemetry.runtimeLoading")}</p>
                    ) : !runtime ? (
                      <p className="text-muted">{t("chat.telemetry.notInformed")}</p>
                    ) : !runtime.loaded ? (
                      <p className="text-muted">{t("chat.telemetry.runtimeNotLoaded", { reason: runtime.reason ?? t("chat.telemetry.notInformed") })}</p>
                    ) : !placement || !counts ? (
                      <p className="text-muted">{t("chat.telemetry.placementMissing")}</p>
                    ) : (
                      <div className="grid gap-0.5 sm:grid-cols-2">
                        <p>{t("chat.telemetry.gpuBackedLayers", { count: counts.gpu })}</p>
                        <p>{t("chat.telemetry.cpuBackedLayers", { count: counts.cpu })}</p>
                        <p>{t("chat.telemetry.mixedLayers", { count: counts.mixed })}</p>
                        {counts.unknown > 0 && <p>{t("chat.telemetry.unknownLayers", { count: counts.unknown })}</p>}
                      </div>
                    )}
                  </div>
                </article>
              );
            })}
          </div>
        </>
      )}
    </section>
  );
}

function TpTable({ rows }: { rows: Array<{ hop: PhasedHopTrace; label: string; phase: PhaseTrace }> }) {
  const { t } = useI18n();
  return <div className="mt-3 overflow-x-auto rounded-lg border border-line/60 bg-surface/30 px-2.5 py-2">
    <p className="mb-1.5 font-medium text-fg">{t("chat.telemetry.tpTitle")}</p>
    <table className="w-full min-w-[54rem] border-collapse font-mono text-[0.66rem] leading-relaxed">
      <thead className="text-faint"><tr className="border-b border-line/70 text-left">
        <th className="py-1 pr-3 font-normal">{t("chat.telemetry.hop")}</th>
        <th className="py-1 pr-3 font-normal">{t("chat.telemetry.phase")}</th>
        <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.tpReduces")}</th>
        <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.tpWait")}</th>
        <th className="py-1 pr-3 text-right font-normal">p50 / p95</th>
        <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.tpBytes")}</th>
        <th className="py-1 text-right font-normal">{t("chat.telemetry.tpChannel")}</th>
      </tr></thead>
      <tbody>{rows.map(({ hop, label, phase }) => {
        const tp = phase.tp;
        if (!tp) return null;
        const channel = tp.tunnel_calls > 0
          ? t("chat.telemetry.tpTunnel", { calls: tp.tunnel_calls, fallback: tp.fallback_calls })
          : t("chat.telemetry.tpDirect", { calls: tp.direct_calls });
        return <tr key={`${hop.node_id}:${hop.start_layer}:${label}`} className="border-t border-line/40">
          <td className="py-1 pr-3 text-fg">L{hop.start_layer}-L{hop.end_layer}</td>
          <td className="py-1 pr-3">{label}</td>
          <td className="py-1 pr-3 text-right">{tp.reduces} / {tp.expected_reduces}</td>
          <td className="py-1 pr-3 text-right">{tp.wait_ms} ms</td>
          <td className="py-1 pr-3 text-right">{tp.reduce_p50_ms} / {tp.reduce_p95_ms} ms</td>
          <td className="py-1 pr-3 text-right">{bytes(tp.request_bytes + tp.response_bytes)} + {bytes(tp.broadcast_bytes)} {t("chat.telemetry.tpBroadcastShort")}</td>
          <td className="py-1 text-right">{channel}</td>
        </tr>;
      })}</tbody>
    </table>
  </div>;
}

function tlsLine(
  t: ReturnType<typeof useI18n>["t"],
  phase: { calls: number; tls_calls: number; tls_handshake_ms: number },
) {
  if (phase.calls === 0) return t("chat.telemetry.tlsIdle");
  if (phase.tls_calls === 0) return t("chat.telemetry.tlsOff");
  if (phase.tls_calls < phase.calls) {
    return t("chat.telemetry.tlsPartial", {
      cifradas: phase.tls_calls,
      total: phase.calls,
      handshake: phase.tls_handshake_ms,
    });
  }
  return t("chat.telemetry.tlsOn", { handshake: phase.tls_handshake_ms });
}

function transportLine(
  t: ReturnType<typeof useI18n>["t"],
  phase: PhaseTrace,
) {
  const summary = phase.transport_summary;
  if (!summary || summary.transports.length === 0) return t("chat.telemetry.transportMissing");
  const transports = summary.transports.map((entry) => {
    const base = t("chat.telemetry.transportSummary", {
      transport: entry.transport,
      calls: entry.calls,
      sent: bytes(entry.bytes_sent),
      received: bytes(entry.bytes_received),
      min: entry.rtt_min_ms,
      median: entry.rtt_median_ms,
      p95: entry.rtt_p95_ms,
      max: entry.rtt_max_ms,
      netMin: entry.net_rtt_min_ms,
      netMedian: entry.net_rtt_median_ms,
      netP95: entry.net_rtt_p95_ms,
      netMax: entry.net_rtt_max_ms,
      fallback: entry.fallback_calls,
    });
    if (entry.transport !== "quic" || !(entry.quic_request_body_send_calls ?? 0)) return base;
    const requestSend = t("chat.telemetry.quicRequestSend", {
      ms: (entry.quic_request_body_send_us ?? 0) / 1_000,
      calls: entry.quic_request_body_send_calls ?? 0,
    });
    return `${base} · ${requestSend}`;
  });
  const switches = summary.transport_switches.map((switchEntry) => t("chat.telemetry.transportSwitch", {
    from: switchEntry.from,
    to: switchEntry.to,
    calls: switchEntry.calls,
  }));
  return [
    t("chat.telemetry.transportFallbackTotal", { calls: summary.fallback_calls }),
    ...transports,
    ...switches,
  ].join(" · ");
}

function PhaseRow({
  hop,
  label,
  phase,
  first,
}: {
  hop: PhasedHopTrace;
  label: string;
  phase: PhaseTrace;
  first?: boolean;
}) {
  const { t } = useI18n();
  const gatewayMs = phase.client_encode_ms + phase.client_decode_ms;
  const named =
    phase.node_frame_decode_ms + phase.node_input_prepare_ms + phase.node_queue_ms +
    phase.node_lock_wait_ms +
    phase.node_setup_ms + phase.node_decode_ms + (phase.node_mtp_draft_ms ?? 0) + phase.node_residual_ms +
    phase.node_sampling_ms + phase.node_encode_ms;
  const nodeResidual = Math.max(0, phase.processing_ms - named);
  const idle = phase.calls === 0;
  const hiddenStateSent = phase.hidden_state_bytes_sent ?? 0;
  const hiddenStateReceived = phase.hidden_state_bytes_received ?? 0;

  return <>
    <tr className={clsx("border-t border-line/40", first && "border-t-line/70", idle && "text-faint")}>
      {first
        ? <td className="py-1 pr-3 align-top text-fg" rowSpan={4}>L{hop.start_layer}-L{hop.end_layer}<br /><span className="text-faint">{hop.node_id.slice(0, 10)}</span></td>
        : null}
      <td className="py-1 pr-3">{label}</td>
      <td className="py-1 pr-3">
        <span className="block">{executionLabel(t, phase)}</span>
        <span className="block text-faint">{phasePathLabel(t, phase.selected_path)}</span>
      </td>
      <td className="py-1 pr-3 text-right">{phase.calls}</td>
      <td className="py-1 pr-3 text-right">{gatewayMs} ms</td>
      <td className="py-1 pr-3 text-right">{phase.node_decode_ms} ms</td>
      <td className="py-1 pr-3 text-right">{phase.node_residual_ms} ms</td>
      <td className="py-1 pr-3 text-right">{phase.node_sampling_ms} ms</td>
      <td className="py-1 pr-3 text-right">{phase.network_ms} ms</td>
      <td className="py-1 text-right">
        {bytes(phase.bytes_sent + phase.bytes_received)}
        {phase.hidden_state_dtype && (
          <span className="block text-[0.6rem] text-aqua">
            {t("chat.telemetry.hiddenState", {
              sent: bytes(hiddenStateSent),
              received: bytes(hiddenStateReceived),
              dtype: phase.hidden_state_dtype,
            })}
          </span>
        )}
      </td>
    </tr>
    <tr className="text-faint">
      <td className="pb-1.5 pr-3 align-top" colSpan={9}>
        <span className="block">{t("chat.telemetry.nodeRow", { frame: phase.node_frame_decode_ms, prepare: phase.node_input_prepare_ms, queue: phase.node_queue_ms, engineWait: phase.node_lock_wait_ms, setup: phase.node_setup_ms, mtpDraft: phase.node_mtp_draft_ms ?? 0, frameOut: phase.node_encode_ms })}{nodeResidual > 0 ? ` · ${t("chat.telemetry.nodeResidual", { ms: nodeResidual })}` : ""}</span>
        <span className="block">{t("chat.telemetry.backendRowCurrent", { call: phase.node_backend_call_ms, sync: phase.node_backend_sync_ms, p50: phase.node_p50_ms, p95: phase.node_p95_ms })}</span>
        <span className="block">{t("chat.telemetry.residualCaptureRow", {
          ready: phase.node_residual_capture_ready_wait_ms ?? 0,
          tensorGet: phase.node_residual_capture_tensor_get_ms ?? 0,
          postprocess: phase.node_residual_capture_postprocess_ms ?? 0,
          total: phase.node_residual_capture_d2h_sync_ms,
          bytes: phase.node_residual_capture_bytes ?? 0,
          copies: phase.node_residual_capture_copies ?? 0,
        })}</span>
        <span className="block">{t("chat.telemetry.residualWireRow", {
          f8: phase.node_residual_wire_f8_conversion_ms ?? 0,
          serialize: phase.node_residual_wire_frame_serialize_ms ?? 0,
          calls: phase.node_residual_wire_encode_calls ?? 0,
        })}</span>
        <span className="block">{t("chat.telemetry.samplingRow", { access: phase.node_logits_access_ms, clone: phase.node_logits_clone_ms, penalty: phase.node_repetition_penalty_ms, softmax: phase.node_softmax_ms, topK: phase.node_top_k_ms, topP: phase.node_top_p_ms, selection: phase.node_selection_ms, text: phase.node_token_text_ms })}</span>
        <span className="block">{t("chat.telemetry.gatewayRow", { encode: phase.client_encode_ms, headers: phase.response_headers_ms, body: phase.response_body_ms, decode: phase.client_decode_ms })}</span>
        {phase.hidden_state_dtype && (
          <span className="block">{t("chat.telemetry.hiddenState", { sent: bytes(hiddenStateSent), received: bytes(hiddenStateReceived), dtype: phase.hidden_state_dtype })}</span>
        )}
        <span className="block">{tlsLine(t, phase)}</span>
        <span className="block break-words">{transportLine(t, phase)}</span>
        {phase.moe_executor_reason && <span className="block break-words text-amber-300">{t("chat.telemetry.moeExecutorReason", { reason: phase.moe_executor_reason })}</span>}
      </td>
    </tr>
  </>;
}

function LegacyHopRows({ hop }: { hop: LegacyHopTrace }) {
  const { t } = useI18n();
  const gatewayMs = hop.client_encode_ms + hop.client_decode_ms;
  const named =
    hop.node_frame_decode_ms + hop.node_input_prepare_ms + hop.node_queue_ms +
    hop.node_setup_ms + hop.node_decode_ms + hop.node_residual_ms +
    hop.node_sampling_ms + hop.node_encode_ms;
  const nodeResidual = Math.max(0, hop.prefill_ms + hop.decode_ms - named);

  return <>
    <tr className="border-t border-line/70">
      <td className="py-1 pr-3 align-top text-fg" rowSpan={2}>L{hop.start_layer}-L{hop.end_layer}<br /><span className="text-faint">{hop.node_id.slice(0, 10)}</span></td>
      <td className="py-1 pr-3">{t("chat.telemetry.legacyBoth")}</td>
      <td className="py-1 pr-3">
        <span className="block">{engineLabel(t, hop.executed_engine)}</span>
        <span className="block text-faint">{phasePathLabel(t, hop.selected_path)}</span>
      </td>
      <td className="py-1 pr-3 text-right">{hop.calls}</td>
      <td className="py-1 pr-3 text-right">{gatewayMs} ms</td>
      <td className="py-1 pr-3 text-right">{hop.node_decode_ms} ms</td>
      <td className="py-1 pr-3 text-right">{hop.node_residual_ms} ms</td>
      <td className="py-1 pr-3 text-right">{hop.node_sampling_ms} ms</td>
      <td className="py-1 pr-3 text-right">{hop.network_ms} ms</td>
      <td className="py-1 text-right">{bytes(hop.bytes_sent + hop.bytes_received)}</td>
    </tr>
    <tr className="text-faint">
      <td className="pb-1.5 pr-3 align-top" colSpan={9}>
        <span className="block">{t("chat.telemetry.legacyNote", { prefillCalls: hop.prefill_calls, decodeCalls: hop.decode_calls })}</span>
        <span className="block">{t("chat.telemetry.legacyNodeRow", { frame: hop.node_frame_decode_ms, prepare: hop.node_input_prepare_ms, queue: hop.node_queue_ms, setup: hop.node_setup_ms, frameOut: hop.node_encode_ms, unattributed: nodeResidual })}</span>
        <span className="block">{t("chat.telemetry.legacyDecodeRow", { decodeP50: hop.node_decode_p50_ms, decodeP95: hop.node_decode_p95_ms, encode: hop.client_encode_ms, headers: hop.response_headers_ms, body: hop.response_body_ms, decode: hop.client_decode_ms })}</span>
      </td>
    </tr>
  </>;
}

function spanLabel(t: ReturnType<typeof useI18n>["t"], span: TraceSpan) {
  const labels: Record<string, string> = {
    validation: "chat.telemetry.spanValidation",
    orchestration: "chat.telemetry.spanOrchestration",
    pipeline_selection: "chat.telemetry.spanPipelineSelection",
    route_resolution: "chat.telemetry.spanRouteResolution",
    session_acquire: "chat.telemetry.spanSessionAcquire",
    first_token_return: "chat.telemetry.spanFirstTokenReturn",
  };
  const key = labels[span.name];
  return key ? t(key) : t("chat.telemetry.spanUnknown", { name: span.name });
}

function measurementLabel(
  t: ReturnType<typeof useI18n>["t"],
  value: number | null | undefined,
) {
  return value == null ? t("chat.telemetry.notMeasured") : ms(value);
}

function countLabel(
  t: ReturnType<typeof useI18n>["t"],
  value: number | null | undefined,
) {
  return value == null ? t("chat.telemetry.notMeasured") : num(value, 2);
}

function ExecutionTimeline({ trace }: { trace: RequestTrace }) {
  const { t } = useI18n();
  const spans = [...(trace.spans ?? [])].sort((a, b) => a.start_offset_ms - b.start_offset_ms);
  const maxEnd = Math.max(1, ...spans.map((span) => span.start_offset_ms + span.duration_ms));
  const longest = spans.reduce<TraceSpan | undefined>(
    (current, span) => !current || span.duration_ms > current.duration_ms ? span : current,
    undefined,
  );
  const unknown = trace.outside_node_unaccounted_ms;
  const principal = unknown != null && (!longest || unknown > longest.duration_ms)
    ? t("chat.telemetry.unaccounted")
    : longest
      ? spanLabel(t, longest)
      : t("chat.telemetry.notMeasured");
  const firstToken = trace.first_token;
  const streaming = trace.streaming;

  return (
    <section className="mt-3 rounded-lg border border-line/70 bg-surface/30 px-3 py-2.5">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <h3 className="font-medium text-fg">{t("chat.telemetry.timelineTitle")}</h3>
        <span className="font-mono text-[0.64rem] text-faint">
          {trace.trace_id ? `trace ${trace.trace_id}` : t("chat.telemetry.traceMissing")}
          {trace.profile ? ` · ${trace.profile}` : ""}
          {trace.request_state ? ` · ${trace.request_state}` : ""}
        </span>
      </div>
      <div className="mt-2 grid gap-1.5 sm:grid-cols-4">
        <div className="rounded-md border border-line/50 px-2.5 py-1.5">
          <p className="text-[0.62rem] uppercase tracking-[0.1em] text-faint">{t("chat.telemetry.outsideTotal")}</p>
          <p className="font-mono text-fg">{measurementLabel(t, trace.outside_node_total_ms)}</p>
        </div>
        <div className="rounded-md border border-line/50 px-2.5 py-1.5">
          <p className="text-[0.62rem] uppercase tracking-[0.1em] text-faint">{t("chat.telemetry.outsideAccounted")}</p>
          <p className="font-mono text-fg">{measurementLabel(t, trace.outside_node_accounted_ms)}</p>
        </div>
        <div className="rounded-md border border-line/50 px-2.5 py-1.5">
          <p className="text-[0.62rem] uppercase tracking-[0.1em] text-faint">{t("chat.telemetry.outsideUnaccounted")}</p>
          <p className="font-mono text-fg">{measurementLabel(t, trace.outside_node_unaccounted_ms)}</p>
        </div>
        <div className="rounded-md border border-aqua/25 bg-aqua/[0.04] px-2.5 py-1.5">
          <p className="text-[0.62rem] uppercase tracking-[0.1em] text-faint">{t("chat.telemetry.principalBottleneck")}</p>
          <p className="truncate font-mono text-aqua">{principal}</p>
        </div>
      </div>
      {spans.length === 0 ? (
        <p className="mt-2 text-[0.68rem] text-muted">{t("chat.telemetry.timelineUnavailable")}</p>
      ) : (
        <div className="mt-3 grid gap-1.5">
          {spans.map((span) => (
            <div key={span.span_id} className="grid min-w-0 grid-cols-[7.5rem_minmax(0,1fr)_4.5rem] items-center gap-2 text-[0.66rem]">
              <span className="truncate text-muted">{spanLabel(t, span)}</span>
              <div className="relative h-5 min-w-0 rounded bg-surface/70">
                <div
                  className="absolute inset-y-0 rounded bg-iris/65"
                  style={{
                    left: `${(span.start_offset_ms / maxEnd) * 100}%`,
                    width: `${Math.max(1, (span.duration_ms / maxEnd) * 100)}%`,
                  }}
                  title={`${spanLabel(t, span)}: ${ms(span.duration_ms)}`}
                />
              </div>
              <span className="text-right font-mono text-faint">{ms(span.duration_ms)}</span>
            </div>
          ))}
        </div>
      )}
      <div className="mt-3 grid gap-1 text-[0.66rem] text-faint sm:grid-cols-2">
        <p>{t("chat.telemetry.firstTokenEngine", { value: measurementLabel(t, firstToken?.engine_ttft_ms ?? trace.engine_ttft_ms) })}</p>
        <p>{t("chat.telemetry.firstTokenNode", { value: measurementLabel(t, firstToken?.node_serialized_ms) })}</p>
        <p>{t("chat.telemetry.firstTokenTransport", { value: measurementLabel(t, firstToken?.node_to_gateway_ms) })}</p>
        <p>{t("chat.telemetry.firstTokenClient", { value: measurementLabel(t, trace.client_ttft_ms) })}</p>
      </div>
      <div className="mt-1 grid gap-1 text-[0.66rem] text-faint sm:grid-cols-2">
        <p>{t("chat.telemetry.streamingSummary", {
          chunks: streaming ? streaming.chunks : t("chat.telemetry.notMeasured"),
          firstChunk: measurementLabel(t, streaming?.time_to_first_chunk_ms),
          duration: measurementLabel(t, streaming?.stream_duration_ms),
          lastToken: measurementLabel(t, streaming?.time_last_token_to_response_end_ms),
          bytes: streaming ? bytes(streaming.bytes) : t("chat.telemetry.notMeasured"),
          tokensPerChunk: countLabel(t, streaming?.tokens_per_chunk),
        })}</p>
        <p>{t("chat.telemetry.decodeSummary", { calls: trace.decode_calls == null ? t("chat.telemetry.notMeasured") : trace.decode_calls, tokens: trace.generated_tokens == null ? t("chat.telemetry.notMeasured") : trace.generated_tokens, perCall: countLabel(t, trace.tokens_per_decode_call) })}</p>
      </div>
      {(trace.connection_new_calls != null || trace.connection_reused_calls != null) && (
        <p className="mt-1 text-[0.66rem] text-faint">
          {t("chat.telemetry.connectionSummary", {
            fresh: trace.connection_new_calls ?? 0,
            reused: trace.connection_reused_calls ?? 0,
            unknown: trace.connection_unknown_calls ?? 0,
          })}
        </p>
      )}
    </section>
  );
}

function hasDetailedTrace(trace: RequestTrace) {
  return trace.trace_id != null
    || trace.spans != null
    || trace.outside_node_total_ms != null
    || trace.outside_node_accounted_ms != null
    || trace.outside_node_unaccounted_ms != null
    || trace.first_token != null
    || trace.streaming != null
    || trace.decode_calls != null
    || trace.connection_new_calls != null;
}

/** The shared visual telemetry card used by Chat and the request inspector. */
export function ExecutionTelemetry({
  trace,
  timings,
  source,
  environment,
  onAnalyze,
  analyzeDisabled = false,
}: {
  trace: RequestTrace;
  timings?: GenerationTimings;
  source?: string;
  environment?: ExecutionTelemetryEnvironment;
  onAnalyze?: (trace: RequestTrace) => void;
  analyzeDisabled?: boolean;
}) {
  const { t } = useI18n();
  const analysis = analyzeTelemetry(trace, timings);
  const tpRows = trace.hops.flatMap((hop) => {
    if (!isPhasedHop(hop)) return [];
    return [
      { hop, label: t("chat.telemetry.prefill"), phase: hop.prefill },
      { hop, label: t("chat.telemetry.decode"), phase: hop.decode },
    ].filter((row) => row.phase.tp?.enabled);
  });
  const rttTotalMs = executionRttMs(trace);
  const networkTotalMs = executionNetworkMs(trace);

  return <div className="w-full">
    {timings && <TimingSummary timings={timings} trace={trace} />}
    <details className="mt-2 w-full rounded-lg border border-line bg-raised px-3 py-2 text-[0.72rem] text-muted">
      <summary className="cursor-pointer font-medium text-fg">{t("chat.telemetry.title")} {source ? `· ${source}` : ""}</summary>
      <div className="mt-2 flex flex-wrap gap-x-3 gap-y-1 font-mono"><span>{t("chat.telemetry.tokensIn", { count: trace.input_tokens })}</span>{trace.cached_tokens ? <span title={t("chat.telemetry.tokensCachedHint")}>{t("chat.telemetry.tokensCached", { count: trace.cached_tokens })}</span> : null}{trace.prefix_match_tokens != null ? <span>{t("chat.telemetry.prefixCandidate", { count: trace.prefix_match_tokens })}</span> : null}{trace.prompt_tokens_transmitted != null ? <span>{t("chat.telemetry.tokensTransmitted", { count: trace.prompt_tokens_transmitted })}</span> : null}{trace.prefix_reuse_status ? <span>{t("chat.telemetry.prefixStatus", { status: trace.prefix_reuse_status })}</span> : null}{trace.gateway_prefix_miss_reason ? <span>{t("chat.telemetry.prefixMissReason", { reason: trace.gateway_prefix_miss_reason })}</span> : null}{trace.prefill_chunk_tokens ? <span>{t("chat.telemetry.prefillChunk", { count: trace.prefill_chunk_tokens })}</span> : null}<span>{t("chat.telemetry.tokensOut", { count: trace.output_tokens })}</span><span>{t("chat.telemetry.prefillMs", { ms: trace.prefill_ms })}</span><span>{rttTotalMs == null ? t("chat.telemetry.rttTotalMissing") : t("chat.telemetry.rttTotal", { ms: rttTotalMs })}</span><span>{networkTotalMs == null ? t("chat.telemetry.networkTotalMissing") : t("chat.telemetry.networkTotal", { ms: networkTotalMs })}</span><span>{bytes(trace.total_bytes_sent + trace.total_bytes_received)}</span></div>
      {hasDetailedTrace(trace) && <ExecutionTimeline trace={trace} />}
      <DistributedTelemetry trace={trace} />
      <p className="mt-1 break-words font-mono text-[0.68rem] text-faint">{t("chat.telemetry.transportHttpsFallback", { calls: trace.https_fallback_calls ?? 0, reasons: trace.https_fallback_reasons?.join("; ") || "-" })}</p>
      {trace.prefill_overlap_enabled && <p className="mt-1 font-mono text-[0.68rem] text-faint">{t("chat.telemetry.overlap", { chunks: trace.prefill_overlapped_chunks ?? 0, depth: trace.prefill_pipeline_depth_max ?? 1, limit: trace.prefill_max_inflight_chunks ?? 8 })}</p>}
      {trace.transport_trace_complete === false && <p className="mt-1 text-[0.68rem] text-amber-300">{t("chat.telemetry.transportTraceIncomplete")}</p>}
      <div className={clsx("mt-3 rounded-lg border px-3 py-2.5", analysis.severity === "critical" ? "border-rose/35 bg-rose/[0.06]" : analysis.severity === "warning" ? "border-amber/35 bg-amber/[0.06]" : "border-line/70 bg-surface/40")}>
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <p className="text-[0.68rem] font-semibold uppercase tracking-[0.12em] text-faint">{t("chat.telemetry.analysis.label")}</p>
          <p className="mt-1 font-medium text-fg">{t(analysis.titleKey)}</p>
          <p className="mt-1 max-w-prose leading-relaxed text-muted">{t(analysis.detailKey)}</p>
          <p className="mt-1 font-mono text-[0.68rem] text-faint">{analysis.evidence}</p>
          <p className="mt-1 text-[0.68rem] text-faint">{t(analysis.nextStepKey)}</p>
        </div>
        {onAnalyze && (
          <Button
            size="sm"
            variant="outline"
            icon={<Sparkles className="h-3.5 w-3.5" />}
            disabled={analyzeDisabled}
            onClick={() => onAnalyze(trace)}
          >
            {t("chat.telemetry.analyze")}
          </Button>
        )}
      </div>
      </div>
      <div className="mt-2 overflow-x-auto">
      <table className="w-full min-w-[46rem] border-collapse font-mono text-[0.66rem] leading-relaxed">
        <thead className="text-faint">
          <tr className="border-b border-line/70 text-left">
            <th className="py-1 pr-3 font-normal">{t("chat.telemetry.hop")}</th>
            <th className="py-1 pr-3 font-normal">{t("chat.telemetry.phase")}</th>
            <th className="py-1 pr-3 font-normal">{t("chat.telemetry.engineHeader")}</th>
            <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.calls")}</th>
            <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.gatewayHeader")}</th>
            <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.backendHeader")}</th>
            <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.residualHeader")}</th>
            <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.samplingHeader")}</th>
            <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.transportHeader")}</th>
            <th className="py-1 text-right font-normal">{t("chat.telemetry.bytesHeader")}</th>
          </tr>
        </thead>
        <tbody>
          {trace.hops.flatMap((hop) => {
            const id = `${hop.node_id}:${hop.start_layer}`;
            if (!isPhasedHop(hop)) return [<LegacyHopRows key={id} hop={hop} />];
            return [
              <PhaseRow key={`${id}:prefill`} hop={hop} label={t("chat.telemetry.prefill")} phase={hop.prefill} first />,
              <PhaseRow key={`${id}:decode`} hop={hop} label={t("chat.telemetry.decode")} phase={hop.decode} />,
            ];
          })}
        </tbody>
      </table>
      </div>
      {tpRows.length > 0 && <TpTable rows={tpRows} />}
      {environment && <ExecutionNetwork trace={trace} environment={environment} />}
      {environment && <ExecutionEnvironment trace={trace} environment={environment} />}
      <p className="mt-2 text-faint">{t("chat.telemetry.footnote")}</p>
    </details>
  </div>;
}
