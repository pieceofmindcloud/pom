/**
 * Conversation history.
 *
 * Kept in this browser's `localStorage`, not on the node. The chat is a test
 * surface for the operator's own machine, and putting transcripts on a peer
 * would mean deciding who else in the cluster gets to read them — a question
 * nobody has asked. The trade is stated in `apps/frontend/README.md`: sessions do not
 * follow you to another browser, and clearing site data clears them.
 */
import {
  createContext,
  createElement,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type Dispatch,
  type ReactNode,
  type SetStateAction,
} from "react";
import { ApiError, streamSse } from "./api";
import { getActiveLocale } from "../i18n/locales";
import {
  consumeChatDelta,
  consumeChatError,
  emptyChatStream,
  finishChatStream,
  streamErrorMessage,
  withClientStreamTiming,
  type GenerationTimings,
  type RequestTrace,
  type StreamMetrics,
} from "./chatStream";
import { storageKey } from "./brand";
import type { ClusterView } from "./types";

export interface ChatAttachment {
  id: string;
  name: string;
  kind: "image" | "text" | "file";
  data: string;
  mediaType?: string;
  size?: number;
  resized?: boolean;
}

export interface ChatMessage {
  id: string;
  /**
   * `event` is not a turn: it is something that happened *to* the conversation,
   * written into the transcript where it happened. Switching model mid-thread
   * is the case that forced it — the answers before and after came from
   * different machines, and without a mark in the flow the transcript reads as
   * one model contradicting itself. Events never travel in the request; see
   * `turnsOnly`.
   */
  role: "user" | "assistant" | "event";
  content: string;
  /** Files shown in the user bubble, including the image data sent to vision. */
  attachments?: ChatAttachment[];
  /** Content sent to the model when the visible bubble is a compact summary. */
  requestContent?: ChatRequestContent;
  /** Set when the answer failed; the turn stays visible with its reason. */
  error?: string;
  /** `x_pom.source`-style hop chain, when the node reported one. */
  source?: string;
  /** Thinking blocks emitted by models with reasoning enabled. */
  reasoning?: string;
  /** How many tokens the model spent thinking, counted from the stream. */
  reasoningTokens?: number;
  /** Authoritative final throughput from the generating node. */
  tokensPerSecond?: number;
  tokensGenerated?: number;
  durationMs?: number;
  /** Request-level TTFT, prefill, generation and total timings. */
  timings?: GenerationTimings;
  /** Detailed execution trace returned only after a completed request. */
  trace?: RequestTrace;
  at: string;
}

export type ChatRequestContent = string | ChatRequestPart[];

export type ChatRequestPart =
  | { type: "text"; text: string }
  | { type: "image_url"; image_url: { url: string } };

export interface ChatSession {
  id: string;
  title: string;
  /**
   * True once someone typed a name for this conversation.
   *
   * Without it the first message would overwrite the name the operator just
   * chose — and the name is the whole point: the captain labels a thread with
   * the model under test so he can find it again when reading the telemetry.
   */
  titled?: boolean;
  model: string | null;
  /** Model selected before the first visible turn of this conversation. */
  modelAtStart?: string | null;
  /** Per-inference Think choice, kept for every message in this conversation. */
  thinkingEnabled?: boolean;
  /** Per-conversation choice to add retrieved context (RAG). Absent means off. */
  ragEnabled?: boolean;
  /** The collection retrieval is kept to in this conversation. Absent means all. */
  ragCollection?: string;
  messages: ChatMessage[];
  updatedAt: string;
}

export interface StartChatStreamOptions {
  session: ChatSession;
  history: ChatMessage[];
  replyId: string;
  model: string;
  maxTokens?: number;
  /** Explicit layer-0 peer chosen in the Chat head selector. */
  inferenceHead?: string;
  /** Per-inference choice captured when the user sends this message. */
  thinkingEnabled: boolean;
  /** Whether this message asks the gateway for retrieved context. Absent: no RAG server, nothing is sent. */
  ragEnabled?: boolean;
  /** The collection the retrieval of this message is kept to. */
  ragCollection?: string;
  emptyResponseMessage: string;
  interruptedMessage: string;
  cluster: ClusterView | null;
  onUnauthorized?: () => void;
}

export interface ChatStreamResult {
  sessionId: string;
  messageId: string;
  trace: RequestTrace;
  timings?: GenerationTimings;
  source?: string;
  cluster: ClusterView | null;
}

/** What the model is allowed to see: the turns, without the marks around them. */
export const turnsOnly = (messages: ChatMessage[]) =>
  messages
    .filter((m) => m.role === "user" || m.role === "assistant")
    .map((m) => ({ ...m, content: m.requestContent ?? m.content }));

const STORAGE = storageKey("chat.sessions");
/** Sessions kept before the oldest is dropped, so storage cannot grow forever. */
const MAX_SESSIONS = 50;
/** Keep streaming progress visible without making every token a React commit. */
const STREAM_UPDATE_INTERVAL_MS = 50;

function read(): ChatSession[] {
  try {
    const raw = localStorage.getItem(STORAGE);
    const parsed = raw ? JSON.parse(raw) : [];
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

function write(sessions: ChatSession[]) {
  try {
    localStorage.setItem(STORAGE, JSON.stringify(sessions.slice(0, MAX_SESSIONS)));
  } catch {
    /* quota exhausted — the transcript is not worth breaking the screen over */
  }
}

function createStreamUpdateScheduler(publish: () => void) {
  let timer: ReturnType<typeof setTimeout> | null = null;
  let dirty = false;

  const flush = () => {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    if (!dirty) return;
    dirty = false;
    publish();
  };

  return {
    schedule() {
      dirty = true;
      if (timer !== null) return;
      timer = setTimeout(() => {
        timer = null;
        if (!dirty) return;
        dirty = false;
        publish();
      }, STREAM_UPDATE_INTERVAL_MS);
    },
    flush,
    cancel() {
      if (timer !== null) clearTimeout(timer);
      timer = null;
      dirty = false;
    },
  };
}

export const newId = () =>
  `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;

/** The first thing the operator said, which is what they will recognise it by. */
export function titleFrom(text: string): string {
  const line = text.trim().split("\n")[0] ?? "";
  return line.length > 52
    ? line.slice(0, 52) + "…"
    : line || (getActiveLocale() === "pt-BR" ? "Conversa sem título" : "Untitled chat");
}

export function emptySession(model: string | null): ChatSession {
  return {
    id: newId(),
    title: getActiveLocale() === "pt-BR" ? "Conversa nova" : "New chat",
    model,
    modelAtStart: model,
    thinkingEnabled: true,
    messages: [],
    updatedAt: new Date().toISOString(),
  };
}

/**
 * Whether there is any conversation to list: a session nobody wrote in (a new
 * chat, or one only touched by a toggle) does not count. Without one, the
 * history and its "New chat" are not drawn, so an empty Chat is only the
 * composer.
 */
export function hasConversations(sessions: ChatSession[]): boolean {
  return sessions.some((session) => session.messages.length > 0);
}

/** One subscription per mount, kept in sync across tabs of the same browser. */
export function useChatSessions() {
  const [sessions, setSessions] = useState<ChatSession[]>(read);

  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key === STORAGE) setSessions(read());
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  const persist = useCallback((next: ChatSession[]) => {
    setSessions(next);
    write(next);
  }, []);

  const upsert = useCallback(
    (session: ChatSession) => {
      const stamped = { ...session, updatedAt: new Date().toISOString() };
      persist([stamped, ...read().filter((s) => s.id !== session.id)]);
    },
    [persist],
  );

  const remove = useCallback(
    (id: string) => persist(read().filter((s) => s.id !== id)),
    [persist],
  );

  const clear = useCallback(() => persist([]), [persist]);

  return { sessions, upsert, remove, clear };
}

export interface ChatSessionController {
  sessions: ChatSession[];
  session: ChatSession;
  setSession: Dispatch<SetStateAction<ChatSession>>;
  upsert: (session: ChatSession) => void;
  selectSession: (session: ChatSession) => void;
  startNew: (model: string | null) => void;
  deleteSession: (id: string, model: string | null) => void;
  registerBeforeChange: (handler: (kind: "select" | "new" | "delete") => void) => () => void;
  streamingSessionId: string | null;
  lastStreamResult: ChatStreamResult | null;
  startStream: (options: StartChatStreamOptions) => void;
  stopStream: () => void;
}

const ChatSessionContext = createContext<ChatSessionController | null>(null);

/**
 * One active conversation is shared by the Chat screen and the infer menu.
 * Keeping it above both siblings prevents the sidebar from becoming a visual
 * copy of the history that cannot actually change the chat.
 */
export function ChatSessionProvider({ children }: { children: ReactNode }) {
  const { sessions, upsert, remove } = useChatSessions();
  const [session, setSession] = useState<ChatSession>(
    () => sessions[0] ?? emptySession(null),
  );
  const beforeChange = useRef<((kind: "select" | "new" | "delete") => void) | null>(null);
  const activeSessionId = useRef(session.id);
  const streamAbort = useRef<AbortController | null>(null);
  const deletedStreamSessions = useRef(new Set<string>());
  const [streamingSessionId, setStreamingSessionId] = useState<string | null>(null);
  const [lastStreamResult, setLastStreamResult] = useState<ChatStreamResult | null>(null);
  activeSessionId.current = session.id;

  const registerBeforeChange = useCallback((handler: (kind: "select" | "new" | "delete") => void) => {
    beforeChange.current = handler;
    return () => {
      if (beforeChange.current === handler) beforeChange.current = null;
    };
  }, []);

  const selectSession = useCallback((next: ChatSession) => {
    beforeChange.current?.("select");
    activeSessionId.current = next.id;
    setSession(next);
  }, []);

  const startNew = useCallback((model: string | null) => {
    beforeChange.current?.("new");
    const next = emptySession(model);
    activeSessionId.current = next.id;
    setSession(next);
  }, []);

  const deleteSession = useCallback((id: string, model: string | null) => {
    deletedStreamSessions.current.add(id);
    remove(id);
    if (activeSessionId.current === id) {
      beforeChange.current?.("delete");
      const next = emptySession(model);
      activeSessionId.current = next.id;
      setSession(next);
    }
  }, [remove]);

  const stopStream = useCallback(() => {
    streamAbort.current?.abort();
  }, []);

  const startStream = useCallback((options: StartChatStreamOptions) => {
    streamAbort.current?.abort();
    deletedStreamSessions.current.delete(options.session.id);
    const controller = new AbortController();
    streamAbort.current = controller;
    setStreamingSessionId(options.session.id);
    setLastStreamResult(null);

    void (async () => {
      let live = options.session;
      const publish = () => {
        if (streamAbort.current !== controller) return;
        if (activeSessionId.current === options.session.id) setSession(live);
        // A route can disappear while the response is arriving. Persisting the
        // partial transcript here lets the next mount render exactly what was
        // already received, even before the stream finishes.
        upsert(live);
      };
      const scheduler = createStreamUpdateScheduler(publish);
      const patchReply = (patch: Partial<ChatMessage>, immediate = false) => {
        live = {
          ...live,
          messages: live.messages.map((message) =>
            message.id === options.replyId ? { ...message, ...patch } : message,
          ),
        };
        // A new message owns the controller from this point on. Do not let a
        // late delta from the aborted request overwrite its newer transcript.
        if (streamAbort.current !== controller) return;
        if (immediate) scheduler.flush();
        else scheduler.schedule();
      };

      try {
        let streamed = emptyChatStream();
        const requestStartedAt = performance.now();
        let firstChunkAt: number | undefined;
        let firstTokenAt: number | undefined;
        let lastTokenAt: number | undefined;
        let clientReceivedAtMs: number | undefined;
        let streamChunks = 0;
        let streamBytes = 0;
        let finalMetrics: StreamMetrics | undefined;
        for await (const data of streamSse(
          "/v1/chat/completions",
          {
            model: options.model,
            ...(options.inferenceHead ? { inference_head: options.inferenceHead } : {}),
            // The conversation's id: a plugin engine keeps and restores the K/V state of a named conversation
            session: options.session.id,
            stream: true,
            ...(options.maxTokens ? { max_tokens: options.maxTokens } : {}),
            thinking_enabled: options.thinkingEnabled,
            ...(options.ragEnabled === undefined ? {} : { rag: options.ragEnabled }),
            ...(options.ragEnabled && options.ragCollection ? { rag_collection: options.ragCollection } : {}),
            messages: turnsOnly(options.history).map((message) => ({
              role: message.role,
              content: message.content,
            })),
          },
          controller.signal,
        )) {
          if (data === "[DONE]") break;
          const receivedAt = performance.now();
          firstChunkAt ??= receivedAt;
          streamChunks += 1;
          streamBytes += new TextEncoder().encode(data).byteLength;
          try {
            const chunk = JSON.parse(data);
            const streamError = streamErrorMessage(chunk);
            if (streamError) {
              streamed = consumeChatError(streamed, streamError);
              patchReply({ error: streamed.error });
              continue;
            }
            const delta = chunk?.choices?.[0]?.delta?.content;
            const toolDelta = chunk?.choices?.[0]?.delta?.tool_calls;
            const reasoningDelta = chunk?.choices?.[0]?.delta?.reasoning_content;
            const hasToken = (typeof delta === "string" && delta.length > 0)
              || (typeof reasoningDelta === "string" && reasoningDelta.length > 0)
              || (Array.isArray(toolDelta) && toolDelta.length > 0);
            if (hasToken) {
              firstTokenAt ??= receivedAt;
              clientReceivedAtMs ??= Date.now();
              lastTokenAt = receivedAt;
            }
            if (typeof delta === "string" && delta) {
              streamed = consumeChatDelta(streamed, delta);
              patchReply({ content: streamed.content, reasoning: streamed.reasoning || undefined, reasoningTokens: streamed.reasoningTokens || undefined });
            }
            const metrics = chunk?.x_pom;
            if (metrics && typeof metrics === "object") finalMetrics = metrics;
          } catch {
            /* Keep-alive or partial JSON frame. */
          }
        }
        const responseEndedAt = performance.now();
        if (finalMetrics) {
          const measuredMetrics = withClientStreamTiming(finalMetrics, {
            requestStartedAt,
            firstChunkAt,
            firstTokenAt,
            lastTokenAt,
            responseEndedAt,
            clientReceivedAtMs,
            chunks: streamChunks,
            bytes: streamBytes,
          });
          streamed = finishChatStream(streamed, measuredMetrics);
          patchReply({
            content: streamed.content,
            reasoning: streamed.reasoning || undefined, reasoningTokens: streamed.reasoningTokens || undefined,
            tokensPerSecond: streamed.tokensPerSecond,
            tokensGenerated: streamed.tokensGenerated,
            durationMs: streamed.durationMs,
            timings: streamed.timings,
            trace: streamed.trace,
            source: typeof finalMetrics.source === "string" ? finalMetrics.source : undefined,
          }, true);
        } else {
          streamed = finishChatStream(streamed);
        }
        patchReply({ content: streamed.content, reasoning: streamed.reasoning || undefined, reasoningTokens: streamed.reasoningTokens || undefined }, true);
        if (streamAbort.current === controller && streamed.trace) {
          setLastStreamResult({
            sessionId: options.session.id,
            messageId: options.replyId,
            trace: streamed.trace,
            timings: streamed.timings,
            source: streamed.source,
            cluster: options.cluster,
          });
        }
        if (!streamed.content && !streamed.reasoning && !streamed.error) {
          patchReply({ error: options.emptyResponseMessage }, true);
        }
      } catch (error) {
        if (controller.signal.aborted) {
          patchReply({ error: options.interruptedMessage }, true);
        } else {
          const detail = error instanceof Error ? error.message : String(error);
          patchReply({ error: detail }, true);
          if (error instanceof ApiError && (error.status === 401 || error.status === 403)) {
            options.onUnauthorized?.();
          }
        }
      } finally {
        if (streamAbort.current === controller) {
          scheduler.flush();
          if (!deletedStreamSessions.current.delete(live.id)) upsert(live);
          streamAbort.current = null;
          setStreamingSessionId(null);
        } else {
          scheduler.cancel();
        }
      }
    })();
  }, [setSession, upsert]);

  const value = useMemo<ChatSessionController>(() => ({
    sessions,
    session,
    setSession,
    upsert,
    selectSession,
    startNew,
    deleteSession,
    registerBeforeChange,
    streamingSessionId,
    lastStreamResult,
    startStream,
    stopStream,
  }), [
    sessions,
    upsert,
    session,
    selectSession,
    startNew,
    deleteSession,
    registerBeforeChange,
    streamingSessionId,
    lastStreamResult,
    startStream,
    stopStream,
  ]);

  return createElement(ChatSessionContext.Provider, { value }, children);
}

export function useChatSessionController(): ChatSessionController {
  const value = useContext(ChatSessionContext);
  if (!value) throw new Error("useChatSessionController must be used inside ChatSessionProvider");
  return value;
}

export function useOptionalChatSessionController() {
  return useContext(ChatSessionContext);
}
