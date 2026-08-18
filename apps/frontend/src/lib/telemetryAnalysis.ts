import type { PhaseTrace, RequestTrace, TpTelemetry } from "./chatStream";
import { isPhasedHop } from "./chatStream";

export type TelemetryAnalysisKind =
  | "tp-network"
  | "pipeline-network"
  | "queue"
  | "backend"
  | "balanced";

export interface TelemetryAnalysis {
  kind: TelemetryAnalysisKind;
  severity: "critical" | "warning" | "ok";
  titleKey: string;
  detailKey: string;
  nextStepKey: string;
  evidence: string;
  tpObserved: boolean;
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

export function analyzeTelemetry(trace: RequestTrace): TelemetryAnalysis {
  const rows = phases(trace);
  const tpRows = rows.filter((row): row is typeof row & { data: PhaseTrace & { tp: TpTelemetry } } => Boolean(row.data.tp?.enabled));
  const tpWait = total(tpRows, (phase) => phase.tp?.wait_ms ?? 0);
  const decode = total(rows, (phase) => phase.node_decode_ms);
  const pipelineNetwork = total(rows, (phase) => phase.network_ms + phase.node_residual_ms);
  const lockWait = total(rows, (phase) => phase.node_lock_wait_ms);
  const processing = total(rows, (phase) => phase.processing_ms);

  if (tpRows.length > 0 && tpWait > 0 && tpWait >= Math.max(100, decode * 0.35)) {
    return {
      kind: "tp-network",
      severity: tpWait >= decode * 0.7 ? "critical" : "warning",
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
      titleKey: "chat.telemetry.analysis.pipelineTitle",
      detailKey: "chat.telemetry.analysis.pipelineDetail",
      nextStepKey: "chat.telemetry.analysis.pipelineNextStep",
      evidence: `pipeline transport + residual ${formatMs(pipelineNetwork)} / processing ${formatMs(processing)}`,
      tpObserved: tpRows.length > 0,
    };
  }

  if (lockWait > 0 && lockWait >= Math.max(100, decode * 0.5)) {
    return {
      kind: "queue",
      severity: "warning",
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
    titleKey: "chat.telemetry.analysis.balancedTitle",
    detailKey: "chat.telemetry.analysis.balancedDetail",
    nextStepKey: "chat.telemetry.analysis.balancedNextStep",
    evidence: `processing ${formatMs(processing)}; TP ${tpRows.length > 0 ? "observed" : "not recorded in this phase"}`,
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
