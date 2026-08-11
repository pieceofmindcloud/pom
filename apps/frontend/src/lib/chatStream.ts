export interface StreamMetrics {
  tokens_per_sec?: number;
  tokens_generated?: number;
  duration_ms?: number;
}

export interface ChatStreamState {
  content: string;
  reasoning: string;
  pending: string;
  inReasoning: boolean;
  tokensPerSecond?: number;
  tokensGenerated?: number;
  durationMs?: number;
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
  return state;
}
