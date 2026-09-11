import type { GenerationTimings, LegacyHopTrace, PhaseTrace, RequestTrace, TelemetrySummary, TpTelemetry } from "./chatStream";
import { isPhasedHop } from "./chatStream";
import type { LayerPlacement } from "./types";

export type TelemetryAnalysisKind =
  | "tp-network"
  | "pipeline-network"
  | "outside-node"
  | "remote-telemetry-incomplete"
  | "unattributed"
  | "queue"
  | "backend"
  | "balanced";

export interface TelemetryAnalysis {
  kind: TelemetryAnalysisKind;
  severity: "critical" | "warning" | "ok";
  confidence: "low" | "medium" | "high";
  titleKey: string;
  detailKey: string;
  nextStepKey: string;
  evidence: string;
  tpObserved: boolean;
}

export interface TokenLatencyRegression {
  fixed_ms: number;
  per_token_ms: number;
  r2: number;
  samples: number;
}

/**
 * Direct per-frame traces report observed decode calls but have no expected
 * span count. A zero denominator is therefore "not measured", not complete
 * coverage. Remote-stream traces set the expectation as callbacks arrive.
 */
export function decodeCoverageMeasured(
  telemetry: Pick<TelemetrySummary, "expected_decode_spans" | "decode_span_expectation_available">,
) {
  return telemetry.expected_decode_spans > 0
    && telemetry.decode_span_expectation_available !== false;
}

/**
 * Fits the fixed and per-token terms from an exported measurement set. This
 * is an analysis aid for the old outside-node report, not a runtime residual:
 * the UI must never derive a distributed gap by subtracting span sums.
 */
export function fitTokenLatencyRegression(
  samples: Array<{ tokens: number; latency_ms: number }>,
): TokenLatencyRegression | null {
  if (samples.length < 2) return null;
  const meanX = samples.reduce((sum, sample) => sum + sample.tokens, 0) / samples.length;
  const meanY = samples.reduce((sum, sample) => sum + sample.latency_ms, 0) / samples.length;
  const denominator = samples.reduce((sum, sample) => sum + (sample.tokens - meanX) ** 2, 0);
  if (denominator === 0) return null;
  const covariance = samples.reduce(
    (sum, sample) => sum + (sample.tokens - meanX) * (sample.latency_ms - meanY),
    0,
  );
  const perToken = covariance / denominator;
  const fixed = meanY - perToken * meanX;
  const totalSquares = samples.reduce((sum, sample) => sum + (sample.latency_ms - meanY) ** 2, 0);
  const residualSquares = samples.reduce((sum, sample) => {
    const predicted = fixed + perToken * sample.tokens;
    return sum + (sample.latency_ms - predicted) ** 2;
  }, 0);
  return {
    fixed_ms: fixed,
    per_token_ms: perToken,
    r2: totalSquares === 0 ? 1 : 1 - residualSquares / totalSquares,
    samples: samples.length,
  };
}

function phases(trace: RequestTrace): Array<{ hop: string; phase: string; data: PhaseTrace }> {
  return trace.hops.flatMap((hop) => {
    const label = `L${hop.start_layer}-L${hop.end_layer}`;
    if (!isPhasedHop(hop)) return [];
    return [
      { hop: label, phase: "prefill", data: hop.prefill },
      { hop: label, phase: "decode", data: hop.decode },
    ];
  });
}

function total(rows: Array<{ data: PhaseTrace }>, selector: (phase: PhaseTrace) => number) {
  return rows.reduce((sum, row) => sum + selector(row.data), 0);
}

function formatMs(value: number) {
  return `${Math.round(value)} ms`;
}

/** RTT observado nas chamadas do stream, ou ausência para traces antigos. */
export function executionRttMs(trace: RequestTrace): number | null {
  const phased = trace.hops.filter(isPhasedHop);
  if (phased.length === 0) return null;
  const phases = phased.flatMap((hop) => [hop.prefill, hop.decode]);
  if (phases.some((phase) => typeof phase.round_trip_ms !== "number")) return null;
  return phases.reduce((sum, phase) => sum + phase.round_trip_ms, 0);
}

/** Tempo de transporte que o trace atribuiu fora do processamento do peer. */
export function executionNetworkMs(trace: RequestTrace): number | null {
  const phased = trace.hops.filter(isPhasedHop);
  if (phased.length > 0) {
    const phases = phased.flatMap((hop) => [hop.prefill, hop.decode]);
    if (phases.some((phase) => typeof phase.network_ms !== "number")) return null;
    return phases.reduce((sum, phase) => sum + phase.network_ms, 0);
  }
  const legacy = trace.hops.filter((hop): hop is LegacyHopTrace => !isPhasedHop(hop));
  if (legacy.length === 0 || legacy.some((hop) => typeof hop.network_ms !== "number")) return null;
  return legacy.reduce((sum, hop) => sum + hop.network_ms, 0);
}

/** Pares adjacentes que a execução realmente atravessou, sem incluir TP workers. */
export function executionPipelineEdges(trace: RequestTrace): Array<{
  from_node_id: string;
  to_node_id: string;
}> {
  const nodes = trace.hops.map((hop) => hop.node_id);
  const edges: Array<{ from_node_id: string; to_node_id: string }> = [];
  for (let index = 1; index < nodes.length; index += 1) {
    const from_node_id = nodes[index - 1];
    const to_node_id = nodes[index];
    if (from_node_id !== to_node_id) edges.push({ from_node_id, to_node_id });
  }
  return edges;
}

export interface PlacementCounts {
  gpu: number;
  cpu: number;
  mixed: number;
  unknown: number;
}

function bufferKind(buffer: string): "gpu" | "cpu" | "unknown" {
  const normalized = buffer.toLowerCase();
  if (normalized.includes("cpu") || normalized.includes("host")) return "cpu";
  if (["cuda", "metal", "vulkan", "opencl", "hip", "sycl", "gpu"].some((name) => normalized.includes(name))) {
    return "gpu";
  }
  return "unknown";
}

/** Conta camadas pela colocação efetiva dos buffers, não pelo pedido de GPU. */
export function effectivePlacementCounts(layers: LayerPlacement[]): PlacementCounts {
  return layers.reduce<PlacementCounts>((counts, layer) => {
    const kinds = new Set(
      layer.buffers
        .filter((buffer) => buffer.bytes > 0)
        .map((buffer) => bufferKind(buffer.buffer)),
    );
    if (kinds.has("gpu") && kinds.has("cpu")) counts.mixed += 1;
    else if (kinds.has("gpu")) counts.gpu += 1;
    else if (kinds.has("cpu")) counts.cpu += 1;
    else counts.unknown += 1;
    return counts;
  }, { gpu: 0, cpu: 0, mixed: 0, unknown: 0 });
}

export function analyzeTelemetry(trace: RequestTrace, timings?: GenerationTimings): TelemetryAnalysis {
  const rows = phases(trace);
  const tpRows = rows.filter((row): row is typeof row & { data: PhaseTrace & { tp: TpTelemetry } } => Boolean(row.data.tp?.enabled));
  const tpWait = total(tpRows, (phase) => phase.tp?.wait_ms ?? 0);
  const decode = total(rows, (phase) => phase.node_decode_ms);
  const pipelineNetwork = total(rows, (phase) => phase.network_ms + phase.node_residual_ms);
  const lockWait = total(rows, (phase) => phase.node_lock_wait_ms);
  const processing = total(rows, (phase) => phase.processing_ms);
  // An outside-node diagnosis is valid only when the runtime supplied an
  // explicit measured field. Never reconstruct it from E2E minus phase sums.
  const outsideNode = trace.outside_node_unaccounted_ms;
  const outsideEvidence = outsideNode == null ? null : `unaccounted ${formatMs(outsideNode)}`;

  const telemetry = trace.telemetry;
  if (telemetry && telemetry.status !== "complete") {
    return {
      kind: "remote-telemetry-incomplete",
      severity: "warning",
      confidence: "high",
      titleKey: "chat.telemetry.analysis.remoteTelemetryIncompleteTitle",
      detailKey: "chat.telemetry.analysis.remoteTelemetryIncompleteDetail",
      nextStepKey: "chat.telemetry.analysis.remoteTelemetryIncompleteNextStep",
      evidence: `telemetry ${telemetry.reported_peers}/${telemetry.expected_peers} peers / ${telemetry.completeness_percent.toFixed(1)}% decode spans`,
      tpObserved: tpRows.length > 0,
    };
  }

  if (timings != null && outsideNode != null && outsideNode >= Math.max(100, processing * 0.35)) {
    return {
      kind: "outside-node",
      severity: outsideNode >= Math.max(500, processing * 0.7) ? "critical" : "warning",
      confidence: "high",
      titleKey: "chat.telemetry.analysis.outsideNodeTitle",
      detailKey: "chat.telemetry.analysis.outsideNodeDetail",
      nextStepKey: "chat.telemetry.analysis.outsideNodeNextStep",
      evidence: `total ${formatMs(timings.total_ms)} / node processing ${formatMs(processing)} / ${outsideEvidence}`,
      tpObserved: tpRows.length > 0,
    };
  }

  if (!telemetry && trace.transport_trace_complete === false) {
    return {
      kind: "remote-telemetry-incomplete",
      severity: "warning",
      confidence: "high",
      titleKey: "chat.telemetry.analysis.remoteTelemetryIncompleteTitle",
      detailKey: "chat.telemetry.analysis.remoteTelemetryIncompleteDetail",
      nextStepKey: "chat.telemetry.analysis.remoteTelemetryIncompleteNextStep",
      evidence: "transport trace incomplete; distributed spans are still unavailable",
      tpObserved: tpRows.length > 0,
    };
  }

  if (tpRows.length > 0 && tpWait > 0 && tpWait >= Math.max(100, decode * 0.35)) {
    return {
      kind: "tp-network",
      severity: tpWait >= decode * 0.7 ? "critical" : "warning",
      confidence: "high",
      titleKey: "chat.telemetry.analysis.tpTitle",
      detailKey: "chat.telemetry.analysis.tpDetail",
      nextStepKey: "chat.telemetry.analysis.tpNextStep",
      evidence: `TP wait ${formatMs(tpWait)} / decode ${formatMs(decode)}`,
      tpObserved: true,
    };
  }

  if (pipelineNetwork > 0 && pipelineNetwork >= Math.max(100, processing * 0.35)) {
    return {
      kind: "pipeline-network",
      severity: pipelineNetwork >= processing * 0.7 ? "critical" : "warning",
      confidence: "medium",
      titleKey: "chat.telemetry.analysis.pipelineTitle",
      detailKey: "chat.telemetry.analysis.pipelineDetail",
      nextStepKey: "chat.telemetry.analysis.pipelineNextStep",
      evidence: `pipeline transport ${formatMs(pipelineNetwork)} / processing ${formatMs(processing)}`,
      tpObserved: tpRows.length > 0,
    };
  }

  if (lockWait > 0 && lockWait >= Math.max(100, decode * 0.5)) {
    return {
      kind: "queue",
      severity: "warning",
      confidence: "high",
      titleKey: "chat.telemetry.analysis.queueTitle",
      detailKey: "chat.telemetry.analysis.queueDetail",
      nextStepKey: "chat.telemetry.analysis.queueNextStep",
      evidence: `engine wait ${formatMs(lockWait)} / decode ${formatMs(decode)}`,
      tpObserved: tpRows.length > 0,
    };
  }

  if (decode > 0 && decode >= Math.max(100, processing * 0.5)) {
    return {
      kind: "backend",
      severity: "warning",
      confidence: "high",
      titleKey: "chat.telemetry.analysis.backendTitle",
      detailKey: "chat.telemetry.analysis.backendDetail",
      nextStepKey: "chat.telemetry.analysis.backendNextStep",
      evidence: `llama_decode ${formatMs(decode)} / processing ${formatMs(processing)}`,
      tpObserved: tpRows.length > 0,
    };
  }

  return {
    kind: "balanced",
    severity: "ok",
    confidence: telemetry ? "high" : "low",
    titleKey: "chat.telemetry.analysis.balancedTitle",
    detailKey: "chat.telemetry.analysis.balancedDetail",
    nextStepKey: "chat.telemetry.analysis.balancedNextStep",
    evidence: `processing ${formatMs(processing)}; overlap ${telemetry?.overlap_ms != null ? formatMs(telemetry.overlap_ms) : "not measured"}; TP ${tpRows.length > 0 ? "observed" : "not recorded in this phase"}`,
    tpObserved: tpRows.length > 0,
  };
}

export function serializeTelemetryTables(trace: RequestTrace): string {
  const rows = phases(trace);
  const phaseTable = rows.map(({ hop, phase, data }) => ({
    hop,
    phase,
    calls: data.calls,
    gateway_ms: data.client_encode_ms + data.client_decode_ms,
    backend_ms: data.node_decode_ms,
    residual_ms: data.node_residual_ms,
    sampling_ms: data.node_sampling_ms,
    transport_ms: data.network_ms,
    bytes: data.bytes_sent + data.bytes_received,
  }));
  const tpTable = rows.map(({ hop, phase, data }) => ({
    hop,
    phase,
    tp: data.tp ?? null,
  }));

  return [
    "Tabela 1: fases e hops",
    JSON.stringify(phaseTable, null, 2),
    "Tabela 2: coletivas TP",
    JSON.stringify(tpTable, null, 2),
    "Nota: espera TP inclui a chamada síncrona e não é latência pura de pacote.",
  ].join("\n");
}

export function buildTelemetryAnalysisPrompt(trace: RequestTrace, portuguese: boolean): string {
  const instructions = portuguese
    ? "Analise esta execução distribuída em português. Use as duas tabelas abaixo. Cite os números medidos, separe fatos de hipóteses, diga se o gargalo é TP, pipeline, fila ou backend e proponha o próximo teste. Não invente dados ausentes."
    : "Analyze this distributed execution. Use both tables below. Cite measured numbers, separate facts from hypotheses, identify whether the bottleneck is TP, pipeline, queue or backend, and propose the next test. Do not invent missing data.";
  return `${instructions}\n\n${serializeTelemetryTables(trace)}`;
}
