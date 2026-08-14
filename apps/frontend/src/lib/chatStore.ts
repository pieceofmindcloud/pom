/**
 * Conversation history.
 *
 * Kept in this browser's `localStorage`, not on the node. The chat is a test
 * surface for the operator's own machine, and putting transcripts on a peer
 * would mean deciding who else in the cluster gets to read them — a question
 * nobody has asked. The trade is stated in `apps/frontend/README.md`: sessions do not
 * follow you to another browser, and clearing site data clears them.
 */
import { useCallback, useEffect, useState } from "react";
import { getActiveLocale } from "../i18n/locales";
import type { RequestTrace } from "./chatStream";

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
  /** Set when the answer failed; the turn stays visible with its reason. */
  error?: string;
  /** `x_rdma_p2p.source`-style hop chain, when the node reported one. */
  source?: string;
  /** Thinking blocks emitted by models with reasoning enabled. */
  reasoning?: string;
  /** Authoritative final throughput from the generating node. */
  tokensPerSecond?: number;
  tokensGenerated?: number;
  durationMs?: number;
  /** Detailed execution trace returned only after a completed request. */
  trace?: RequestTrace;
  at: string;
}

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
  messages: ChatMessage[];
  updatedAt: string;
}

/** What the model is allowed to see: the turns, without the marks around them. */
export const turnsOnly = (messages: ChatMessage[]) =>
  messages.filter((m) => m.role === "user" || m.role === "assistant");

const STORAGE = "codai.chat.sessions";
/** Sessions kept before the oldest is dropped, so storage cannot grow forever. */
const MAX_SESSIONS = 50;

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
    messages: [],
    updatedAt: new Date().toISOString(),
  };
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
