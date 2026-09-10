import type { ExecutionEngine, GenerationTimings, WireDtype } from "./types";

export type { ExecutionEngine } from "./types";
export type { GenerationTimings } from "./types";

export interface StreamMetrics {
  tokens_per_sec?: number;
  tokens_generated?: number;
  duration_ms?: number;
  timings?: GenerationTimings;
  /** TTFT measured by the browser from request dispatch to first token. */
  client_ttft_ms?: number | null;
  trace?: RequestTrace;
  source?: string;
}

export type RequestState = "cold" | "warm" | "unknown";

export interface TraceSpan {
  name: string;
  span_id: string;
  parent_span_id?: string | null;
  host: string;
  /** Wall-clock value for correlation only. Never subtract across hosts. */
  started_at_ms: number;
  /** Monotonic offset from the local request start. */
  start_offset_ms: number;
  duration_ms: number;
  parallel_group?: string | null;
}

export interface FirstTokenTrace {
  engine_produced_at_ms?: number | null;
  node_serialized_at_ms?: number | null;
  node_sent_at_ms?: number | null;
  coordinator_received_at_ms?: number | null;
  gateway_stream_at_ms?: number | null;
  client_received_at_ms?: number | null;
  engine_ttft_ms?: number | null;
  node_serialized_ms?: number | null;
  node_to_gateway_ms?: number | null;
  gateway_to_client_ms?: number | null;
}

export interface StreamingTrace {
  time_to_first_chunk_ms?: number | null;
  time_to_first_token_ms?: number | null;
  stream_duration_ms?: number | null;
  time_last_token_to_response_end_ms?: number | null;
  chunks: number;
  bytes: number;
  tokens_per_chunk?: number | null;
}

export interface RequestTrace {
  trace_id?: string;
  profile?: "off" | "basic" | "trace";
  request_state?: RequestState;
  spans?: TraceSpan[];
  outside_node_total_ms?: number | null;
  outside_node_accounted_ms?: number | null;
  outside_node_unaccounted_ms?: number | null;
  first_token?: FirstTokenTrace;
  streaming?: StreamingTrace;
  server_ttft_ms?: number | null;
  engine_ttft_ms?: number | null;
  client_ttft_ms?: number | null;
  decode_calls?: number;
  generated_tokens?: number;
  tokens_per_decode_call?: number | null;
  connection_new_calls?: number;
  connection_reused_calls?: number;
  connection_unknown_calls?: number;
  https_fallback_calls?: number;
  https_fallback_reasons?: string[];
  model: string;
  input_tokens: number;
  /** Tokens do prompt que o pipeline não reprocessou porque já estavam no KV
   *  da sessão do turno anterior. Medida do executado, não da escolha do
   *  cache. Ausente em trace de nó antigo. */
  cached_tokens?: number;
  /** Prefixo comum encontrado antes de decidir se o KV podia ser reaproveitado. */
  prefix_match_tokens?: number;
  /** Resultado executado do reuso: full, cold, miss ou perda de sessão. */
  prefix_reuse_status?: string;
  /** Tokens que cruzaram a fronteira de inferência nesta execução. */
  prompt_tokens_transmitted?: number;
  output_tokens: number;
  prefill_ms: number;
  total_bytes_sent: number;
  total_bytes_received: number;
  hops: HopTrace[];
  transport_trace_complete?: boolean;
  telemetry?: TelemetrySummary;
  profiling_spans?: TelemetrySpan[];
  token_cycle?: TokenCycleTelemetry;
  prefill_overlap_enabled?: boolean;
  prefill_max_inflight_chunks?: number;
  prefill_overlapped_chunks?: number;
  prefill_pipeline_depth_max?: number;
}

export interface ClientStreamObservation {
  requestStartedAt: number;
  firstChunkAt?: number;
  firstTokenAt?: number;
  lastTokenAt?: number;
  responseEndedAt: number;
  /** Browser wall clock for cross-host log correlation only. */
  clientReceivedAtMs?: number;
  chunks: number;
  bytes: number;
}

function elapsedMs(start: number | undefined, end: number | undefined) {
  return start == null || end == null ? null : Math.max(0, Math.round(end - start));
}

/** Adds browser-local delivery observations to a server trace. */
export function withClientStreamTiming(
  metrics: StreamMetrics,
  observation: ClientStreamObservation,
): StreamMetrics {
  const clientTtft = elapsedMs(observation.requestStartedAt, observation.firstTokenAt);
  const firstChunk = elapsedMs(observation.requestStartedAt, observation.firstChunkAt);
  const streamDuration = observation.firstChunkAt == null
    ? null
    : Math.max(0, Math.round(observation.responseEndedAt - observation.firstChunkAt));
  const lastTokenToEnd = elapsedMs(observation.lastTokenAt, observation.responseEndedAt);
  const tokenCount = metrics.trace?.generated_tokens ?? metrics.trace?.output_tokens;
  const tokensPerChunk = observation.chunks > 0 && tokenCount != null && Number.isFinite(tokenCount)
    ? tokenCount / observation.chunks
    : null;
  const trace = metrics.trace
    ? {
        ...metrics.trace,
        client_ttft_ms: clientTtft,
        first_token: {
          ...(metrics.trace.first_token ?? {}),
          client_received_at_ms: observation.clientReceivedAtMs ?? null,
        },
        streaming: {
          ...(metrics.trace.streaming ?? {}),
          time_to_first_chunk_ms: firstChunk,
          time_to_first_token_ms: clientTtft,
          stream_duration_ms: streamDuration,
          time_last_token_to_response_end_ms: lastTokenToEnd,
          chunks: observation.chunks,
          bytes: observation.bytes,
          tokens_per_chunk: tokensPerChunk,
        },
      }
    : undefined;
  const timings = metrics.timings
    ? { ...metrics.timings, client_ttft_ms: clientTtft }
    : undefined;
  return { ...metrics, trace, timings };
}

export type TelemetryStatus = "collecting" | "response_complete" | "complete" | "incomplete" | "timeout";

export interface RemoteStreamTelemetry {
  open_ms?: number;
  lifetime_ms?: number;
  frames_sent: number;
  frames_received: number;
  bytes_sent: number;
  bytes_received: number;
  first_frame_latency_ms?: number;
  frame_latency_min_ms: number;
  frame_latency_p50_ms: number;
  frame_latency_p95_ms: number;
  frame_latency_p99_ms: number;
  frame_latency_max_ms: number;
  remote_compute_total_ms: number;
  remote_compute_min_ms: number;
  remote_compute_p50_ms: number;
  remote_compute_p95_ms: number;
  remote_compute_p99_ms: number;
  remote_compute_max_ms: number;
  remote_stream_wait_total_ms: number;
  remote_stream_wait_p50_ms: number;
  remote_stream_wait_p95_ms: number;
  remote_stream_producer_wait_total_ms?: number;
  remote_stream_producer_wait_p50_ms?: number;
  remote_stream_producer_wait_p95_ms?: number;
  remote_stream_queue_wait_total_ms?: number;
  remote_stream_queue_wait_p50_ms?: number;
  remote_stream_queue_wait_p95_ms?: number;
  token_delivery_interval_min_ms?: number;
  token_delivery_interval_p50_ms?: number;
  token_delivery_interval_p95_ms?: number;
  token_delivery_interval_p99_ms?: number;
  token_delivery_interval_max_ms?: number;
  token_delivery_interval_samples?: number;
}

export interface TokenCycleTelemetry {
  samples: number;
  p50_ms: number;
  p95_ms: number;
  p99_ms: number;
  max_ms: number;
}

export interface TelemetrySummary {
  status: TelemetryStatus;
  response_complete?: boolean;
  expected_peers: number;
  reported_peers: number;
  missing_peers: string[];
  expected_decode_spans: number;
  reported_decode_spans: number;
  completeness_percent: number;
  duplicate_spans: number;
  late_spans: number;
  orphan_spans: number;
  wall_clock_ms?: number;
  critical_path_ms?: number;
  accumulated_compute_ms: number;
  accumulated_transport_ms: number;
  overlap_ms?: number;
  true_residual_ms?: number;
  remote_stream: RemoteStreamTelemetry;
}

export interface TelemetrySpan {
  trace_id: string;
  request_id: string;
  pipeline_id: string;
  peer_id: string;
  span_id: string;
  parent_span_id?: string;
  phase: "prefill" | "decode";
  token_index?: number;
  start_ns: number;
  end_ns: number;
  duration_ns: number;
  transport?: string;
  clock_domain: string;
}

export interface TransportSummaryEntry {
  transport:
    | "local"
    | "libp2p-tunnel"
    | "http"
    | "https"
    | "quic"
    // A ligação persistente entre andares: o mesmo HTTPS pinado, mas com o
    // quadro entrando num corpo já aberto em vez de numa requisição nova.
    | "https-link"
    | "remote-stream"
    | "unknown";
  calls: number;
  bytes_sent: number;
  bytes_received: number;
  rtt_min_ms: number;
  rtt_median_ms: number;
  rtt_p95_ms: number;
  rtt_max_ms: number;
  fallback_calls: number;
}

export interface TransportSwitch {
  from: TransportSummaryEntry["transport"];
  to: TransportSummaryEntry["transport"];
  calls: number;
}

export interface TransportSummary {
  transports: TransportSummaryEntry[];
  fallback_calls: number;
  transport_switches: TransportSwitch[];
}

export interface TpTelemetry {
  enabled: boolean;
  rank: number;
  size: number;
  expected_reduces: number;
  reduces: number;
  wait_ms: number;
  reduce_p50_ms: number;
  reduce_p95_ms: number;
  request_bytes: number;
  response_bytes: number;
  direct_calls: number;
  tunnel_calls: number;
  fallback_calls: number;
  broadcast_calls: number;
  broadcast_bytes: number;
}

/** One hop's totals inside a single phase. Prefill and decode never mix. */
export interface PhaseTrace {
  processing_ms: number;
  round_trip_ms: number;
  network_ms: number;
  bytes_sent: number;
  bytes_received: number;
  calls: number;
  reported_spans?: number;
  client_encode_ms: number;
  response_headers_ms: number;
  response_body_ms: number;
  client_decode_ms: number;
  /**
   * Chamadas deste salto que foram cifradas.
   *
   * Contagem e não booleano: uma fase soma várias chamadas, e se só parte
   * delas foi por TLS a tela precisa dizer isso em vez de escolher um dos dois.
   */
  tls_calls: number;
  /**
   * Soma dos handshakes TLS deste salto, medidos no servidor.
   *
   * Quase sempre zero: o handshake acontece uma vez por conexão e as conexões
   * ficam no pool. É custo de estabelecimento, não custo por token.
   */
  tls_handshake_ms: number;
  /** Distribuição limitada das travessias observadas deste hop. */
  transport_summary?: TransportSummary;
  node_frame_decode_ms: number;
  node_input_prepare_ms: number;
  node_queue_ms: number;
  node_lock_wait_ms: number;
  node_setup_ms: number;
  node_decode_ms: number;
  node_backend_enqueue_ms: number;
  /** Backend completion timing. Never infer POM_PROFILE_SYNC from this value. */
  node_backend_completion_ms: number;
  node_residual_capture_d2h_sync_ms: number;
  node_residual_ms: number;
  node_logits_access_ms: number;
  node_logits_clone_ms: number;
  node_repetition_penalty_ms: number;
  node_softmax_ms: number;
  node_top_k_ms: number;
  node_top_p_ms: number;
  node_selection_ms: number;
  node_token_text_ms: number;
  /** Estado oculto que cruzou este hop nesta fase, separado dos bytes de frame. */
  hidden_state_bytes_sent?: number;
  hidden_state_bytes_received?: number;
  /** Formato efetivamente observado; ausente em traces anteriores à medição. */
  hidden_state_dtype?: WireDtype;
  node_sampling_ms: number;
  node_encode_ms: number;
  node_p50_ms: number;
  node_p95_ms: number;
  /** Caminho escolhido pelo GGUF, separado do motor que executou. */
  selected_path?: string;
  /** Motor único observado nesta fase, ou unknown quando houve mistura. */
  executed_engine?: ExecutionEngine;
  /** Evidência completa para diagnosticar uma fase mista. */
  executed_engines?: ExecutionEngine[];
  engine_mismatch?: boolean;
  mixed_execution?: boolean;
  /** Executor MoE e razão observados nesta fase, quando o nó os publicou. */
  moe_executor?: string;
  moe_executors?: string[];
  moe_executor_dtypes?: number[];
  moe_executor_reason?: string;
  tp?: TpTelemetry | null;
}

export interface PhasedHopTrace {
  node_id: string;
  start_layer: number;
  end_layer: number;
  prefill: PhaseTrace;
  decode: PhaseTrace;
}

/**
 * The shape hops had before prefill and decode were split apart.
 *
 * Two places still hand us one: a chat session saved in localStorage before the
 * change, and a peer still running the older binary. Neither can be told to go
 * away, so the table has to render both shapes.
 */
export interface LegacyHopTrace {
  node_id: string;
  start_layer: number;
  end_layer: number;
  prefill_ms: number;
  decode_ms: number;
  network_ms: number;
  bytes_sent: number;
  bytes_received: number;
  calls: number;
  prefill_calls: number;
  decode_calls: number;
  client_encode_ms: number;
  response_headers_ms: number;
  response_body_ms: number;
  client_decode_ms: number;
  node_frame_decode_ms: number;
  node_input_prepare_ms: number;
  node_queue_ms: number;
  node_lock_wait_ms: number;
  node_setup_ms: number;
  node_decode_ms: number;
  node_residual_ms: number;
  node_sampling_ms: number;
  node_encode_ms: number;
  node_decode_p50_ms: number;
  node_decode_p95_ms: number;
  /** Peers antigos normalmente não informam estes campos. */
  selected_path?: string;
  executed_engine?: ExecutionEngine;
  executed_engines?: ExecutionEngine[];
  engine_mismatch?: boolean;
  mixed_execution?: boolean;
  moe_executor?: string;
  moe_executors?: string[];
  moe_executor_dtypes?: number[];
  moe_executor_reason?: string;
}

export type HopTrace = PhasedHopTrace | LegacyHopTrace;

/**
 * A legacy hop cannot be split after the fact: its timings and bytes were summed
 * across both phases before they ever reached us. Inventing a prefill row and a
 * decode row out of one total would put a number under a label it does not
 * belong to, so the table says so instead.
 */
export function isPhasedHop(hop: HopTrace): hop is PhasedHopTrace {
  return typeof (hop as PhasedHopTrace).prefill === "object"
    && (hop as PhasedHopTrace).prefill !== null;
}

export interface ChatStreamState {
  content: string;
  reasoning: string;
  pending: string;
  inReasoning: boolean;
  tokensPerSecond?: number;
  tokensGenerated?: number;
  durationMs?: number;
  timings?: GenerationTimings;
  trace?: RequestTrace;
  source?: string;
  error?: string;
}

const OPEN = "<think>";
const CLOSE = "</think>";

export function emptyChatStream(): ChatStreamState {
  return { content: "", reasoning: "", pending: "", inReasoning: false };
}

function suffixThatMayStart(text: string, marker: string): number {
  const limit = Math.min(text.length, marker.length - 1);
  for (let length = limit; length > 0; length -= 1) {
    if (text.endsWith(marker.slice(0, length))) return length;
  }
  return 0;
}

function append(state: ChatStreamState, text: string): ChatStreamState {
  if (!text) return state;
  return state.inReasoning
    ? { ...state, reasoning: state.reasoning + text }
    : { ...state, content: state.content + text };
}

/** Separates `<think>` blocks without leaking split delimiters into the reply. */
export function consumeChatDelta(previous: ChatStreamState, delta: string): ChatStreamState {
  let state = { ...previous, pending: "" };
  let buffer = previous.pending + delta;

  while (buffer) {
    const marker = state.inReasoning ? CLOSE : OPEN;
    const markerIndex = buffer.indexOf(marker);
    if (markerIndex >= 0) {
      state = append(state, buffer.slice(0, markerIndex));
      state = { ...state, inReasoning: !state.inReasoning };
      buffer = buffer.slice(markerIndex + marker.length);
      continue;
    }

    const pendingLength = suffixThatMayStart(buffer, marker);
    state = append(state, buffer.slice(0, buffer.length - pendingLength));
    return { ...state, pending: buffer.slice(buffer.length - pendingLength) };
  }

  return state;
}

/** Reads the message from an OpenAI-compatible error envelope sent in SSE. */
export function streamErrorMessage(payload: unknown): string | undefined {
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) return undefined;
  const error = (payload as { error?: unknown }).error;
  if (typeof error === "string") return error.trim() ? error : undefined;
  if (!error || typeof error !== "object" || Array.isArray(error)) return undefined;
  const message = (error as { message?: unknown }).message;
  return typeof message === "string" && message.trim() ? message : undefined;
}

/** Stores a server error without discarding content already rendered. */
export function consumeChatError(previous: ChatStreamState, error: string): ChatStreamState {
  return error ? { ...previous, error } : previous;
}

/** Flushes an incomplete delimiter as literal text and records final metrics. */
export function finishChatStream(previous: ChatStreamState, metrics?: StreamMetrics): ChatStreamState {
  let state = append({ ...previous, pending: "" }, previous.pending);
  if (metrics?.tokens_per_sec != null) state = { ...state, tokensPerSecond: metrics.tokens_per_sec };
  if (metrics?.tokens_generated != null) state = { ...state, tokensGenerated: metrics.tokens_generated };
  if (metrics?.duration_ms != null) state = { ...state, durationMs: metrics.duration_ms };
  if (metrics?.timings) {
    state = {
      ...state,
      timings: metrics.client_ttft_ms === undefined
        ? metrics.timings
        : { ...metrics.timings, client_ttft_ms: metrics.client_ttft_ms },
    };
  }
  if (metrics?.trace) state = { ...state, trace: metrics.trace };
  if (metrics?.source != null) state = { ...state, source: metrics.source };
  return state;
}
