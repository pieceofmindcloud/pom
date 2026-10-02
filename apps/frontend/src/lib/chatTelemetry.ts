/**
 * The data contract of the `chat.telemetry` plugin slot.
 *
 * The open-source Chat only draws the compact `TelemetryLine`. When an active
 * plugin claims the `chat.telemetry` slot (`slots` in `pom-plugin-ui/v1`), the
 * Chat mounts that component at the end of the line with these props, and
 * only then fetches the per-node environment (network RTT, runtimes) that the
 * detailed view needs.
 */
import type { GenerationTimings, RequestTrace } from "./chatStream";
import type { ChatTelemetryNetwork, ClusterView, PeerRuntime } from "./types";

export const CHAT_TELEMETRY_SLOT = "chat.telemetry";

/** Cluster state captured when the answer finished, plus live per-node data. */
export interface ChatTelemetryEnvironment {
  snapshotCaptured: boolean;
  cluster?: ClusterView | null;
  network?: ChatTelemetryNetwork;
  networkLoading?: boolean;
  networkError?: boolean;
  runtimes: Record<string, PeerRuntime | undefined>;
  runtimesLoading?: boolean;
}

/** A chat turn the plugin asks the Chat to send, e.g. "analyze this trace". */
export interface ChatTelemetryQuestion {
  /** What the transcript shows as the user's message. */
  visibleText: string;
  /** What is actually sent to the model (prompt plus serialized telemetry). */
  requestContent: string;
}

export interface ChatTelemetrySlotProps {
  messageId: string;
  trace: RequestTrace;
  timings?: GenerationTimings;
  source?: string;
  environment?: ChatTelemetryEnvironment;
  /** Model that produced the answer, when known. */
  model?: string;
  /** Present when the Chat can send a follow-up turn about this telemetry. */
  onAsk?: (question: ChatTelemetryQuestion) => void;
  /** True while a stream is running or no model is selected. */
  askDisabled: boolean;
}
