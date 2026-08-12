export interface StreamMetrics {
  tokens_per_sec?: number;
  tokens_generated?: number;
  duration_ms?: number;
  trace?: RequestTrace;
}

export interface RequestTrace {
  model: string;
  input_tokens: number;
  output_tokens: number;
  prefill_ms: number;
  total_bytes_sent: number;
  total_bytes_received: number;
  hops: HopTrace[];
}

/** One hop's totals inside a single phase. Prefill and decode never mix. */
export interface PhaseTrace {
  processing_ms: number;
  network_ms: number;
  bytes_sent: number;
  bytes_received: number;
  calls: number;
  client_encode_ms: number;
  response_headers_ms: number;
  response_body_ms: number;
  client_decode_ms: number;
  node_frame_decode_ms: number;
  node_input_prepare_ms: number;
  node_queue_ms: number;
  node_setup_ms: number;
  node_decode_ms: number;
  node_backend_enqueue_ms: number;
  /** Zero unless the peer runs with RDMA_P2P_PROFILE_SYNC=1. */
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
  node_sampling_ms: number;
  node_encode_ms: number;
  node_p50_ms: number;
  node_p95_ms: number;
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
  node_setup_ms: number;
  node_decode_ms: number;
  node_residual_ms: number;
  node_sampling_ms: number;
  node_encode_ms: number;
  node_decode_p50_ms: number;
  node_decode_p95_ms: number;
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
  trace?: RequestTrace;
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

/** Flushes an incomplete delimiter as literal text and records final metrics. */
export function finishChatStream(previous: ChatStreamState, metrics?: StreamMetrics): ChatStreamState {
  let state = append({ ...previous, pending: "" }, previous.pending);
  if (metrics?.tokens_per_sec != null) state = { ...state, tokensPerSecond: metrics.tokens_per_sec };
  if (metrics?.tokens_generated != null) state = { ...state, tokensGenerated: metrics.tokens_generated };
  if (metrics?.duration_ms != null) state = { ...state, durationMs: metrics.duration_ms };
  if (metrics?.trace) state = { ...state, trace: metrics.trace };
  return state;
}
