/**
 * The test chat.
 *
 * It talks to the cluster the same way any other client does — `POST
 * /v1/chat/completions` with `stream: true`, signed with the connection key —
 * so what shows up here is exactly what a harness would get. No separate path,
 * no shortcut through the local node.
 */
import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import {
  ArrowUp,
  Boxes,
  Check,
  ChevronDown,
  Copy,
  MessagesSquare,
  Pencil,
  Plus,
  Square,
  Trash2,
  TriangleAlert,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Markdown } from "../components/Markdown";
import { ExecutionTelemetry, TimingSummary, type ExecutionTelemetryEnvironment } from "../components/ExecutionTelemetry";
import { Badge, Button, Empty } from "../components/ui";
import { ApiError, streamSse } from "../lib/api";
import { isNearBottom } from "../lib/chatScroll";
import {
  consumeChatDelta,
  consumeChatError,
  emptyChatStream,
  finishChatStream,
  streamErrorMessage,
} from "../lib/chatStream";
import {
  buildTelemetryAnalysisPrompt,
} from "../lib/telemetryAnalysis";
import {
  emptySession,
  newId,
  titleFrom,
  turnsOnly,
  useChatSessions,
  type ChatMessage,
  type ChatSession,
} from "../lib/chatStore";
import { ms, num, since } from "../lib/format";
import type { RequestTrace } from "../lib/chatStream";
import {
  useChatTelemetryNetwork,
  useCluster,
  useNode,
  usePeerRuntimes,
  useServableModels,
} from "../lib/queries";
import { useStableRecord } from "../lib/stableRecord";
import { chatModelOptions } from "../lib/chatModels";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import type { ClusterView, PeerRuntime } from "../lib/types";

export { ExecutionTelemetry } from "../components/ExecutionTelemetry";
export type { ExecutionTelemetryEnvironment } from "../components/ExecutionTelemetry";

/**
 * Something that happened to the conversation, on the line where it happened.
 *
 * Drawn as a rule with a label sitting on it rather than a bubble: it is not a
 * turn, and giving it the shape of one would make the transcript claim somebody
 * said it.
 */
function EventMark({ message }: { message: ChatMessage }) {
  return (
    <motion.div
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      transition={{ duration: 0.2, ease: [0.22, 1, 0.36, 1] }}
      className="flex items-center gap-3"
      role="note"
    >
      <span className="h-px flex-1 bg-line" />
      <span className="flex shrink-0 items-center gap-1.5 rounded-full border border-line bg-raised px-2.5 py-1 text-[0.7rem] text-muted">
        <Boxes className="h-3 w-3 text-faint" />
        {message.content}
      </span>
      <span className="h-px flex-1 bg-line" />
    </motion.div>
  );
}

function ModelIntroMark({ model }: { model: string }) {
  const { t } = useI18n();
  return <EventMark message={{ id: "model-at-start", role: "event", content: t("chat.modelAtStart", { model }), at: "" }} />;
}

export function ChatError({ message }: { message: string }) {
  const { t } = useI18n();
  const [copied, setCopied] = useState(false);

  const copy = async () => {
    if (!navigator.clipboard) return;
    try {
      await navigator.clipboard.writeText(message);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1600);
    } catch {
      // The message remains selectable when the browser denies clipboard access.
    }
  };

  return (
    <div
      role="alert"
      className="mt-2 min-w-0 rounded-lg border border-rose/30 bg-rose/[0.07] px-3 py-2.5"
    >
      <div className="flex items-start justify-between gap-3">
        <div className="flex min-w-0 items-start gap-2.5">
          <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0 text-rose" />
          <p className="text-[0.82rem] font-medium text-fg">{t("chat.serverError")}</p>
        </div>
        <button
          type="button"
          onClick={() => void copy()}
          aria-label={t("chat.copyError")}
          className="flex shrink-0 items-center gap-1.5 rounded-md border border-line bg-surface px-2 py-1 text-[0.7rem] text-muted transition-colors hover:text-fg"
        >
          {copied ? <Check className="h-3 w-3 text-aqua" /> : <Copy className="h-3 w-3" />}
          {copied ? t("chat.errorCopied") : t("chat.copyError")}
        </button>
      </div>
      <p className="break-path mt-2 max-h-72 overflow-auto whitespace-pre-wrap select-text rounded-md border border-rose/20 bg-ink/40 px-2.5 py-2 text-[0.82rem] leading-relaxed text-muted">
        {message}
      </p>
    </div>
  );
}

function Bubble({
  message,
  streaming,
  onAnalyze,
  analysisDisabled = false,
  telemetryEnvironment,
}: {
  message: ChatMessage;
  streaming: boolean;
  onAnalyze?: (trace: RequestTrace) => void;
  analysisDisabled?: boolean;
  telemetryEnvironment?: ExecutionTelemetryEnvironment;
}) {
  const { t } = useI18n();
  if (message.role === "event") return <EventMark message={message} />;
  const isUser = message.role === "user";
  return (
    <motion.div
      initial={{ opacity: 0, y: 6 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.2, ease: [0.22, 1, 0.36, 1] }}
      className={clsx("flex min-w-0 gap-3", isUser && "justify-end")}
    >
      {isUser ? (
        <div className="max-w-[min(42rem,85%)] rounded-2xl rounded-br-md border border-line bg-raised px-4 py-2.5">
          <p className="whitespace-pre-wrap text-[0.9rem] leading-relaxed text-fg">
            {message.content}
          </p>
        </div>
      ) : (
        <div className="min-w-0 flex-1">
          {message.reasoning && (
            <details open={streaming} className="mb-3 rounded-xl border border-iris/25 bg-iris/[0.05] px-3 py-2.5">
              <summary className="cursor-pointer select-none text-xs font-medium text-iris">{t("chat.thinking")}</summary>
              <div className="mt-2 text-sm text-muted"><Markdown>{message.reasoning}</Markdown></div>
            </details>
          )}
          {message.content && <Markdown>{message.content}</Markdown>}
          {message.error && (
            <ChatError message={message.error} />
          )}
          {message.source && (
            <p className="mt-2 font-mono text-[0.68rem] text-faint">{message.source}</p>
          )}
          {message.timings ? (
            <TimingSummary timings={message.timings} />
          ) : message.tokensPerSecond != null && (
            <div className="mt-2 flex flex-wrap items-center gap-2 font-mono text-[0.68rem] text-faint">
              <span>{t("chat.legacyTotalSpeed", { value: num(message.tokensPerSecond, 1) })}{message.tokensGenerated != null && ` · ${message.tokensGenerated} ${t("chat.tokensUnit")}`}{message.durationMs != null && ` · ${ms(message.durationMs)}`}</span>
            </div>
          )}
          {message.trace && (
            <div className="mt-2">
              <ExecutionTelemetry
                trace={message.trace}
                timings={message.timings}
                source={message.source}
                environment={telemetryEnvironment}
                onAnalyze={onAnalyze}
                analyzeDisabled={analysisDisabled || streaming}
              />
            </div>
          )}
        </div>
      )}
    </motion.div>
  );
}

/**
 * The conversation's name, edited where it is read.
 *
 * The captain names a thread after the model he is testing so he can come back
 * to it when reading the telemetry, and a name he cannot change is a name he
 * will not use. Clicking the title turns it into the field — no pencil to hunt
 * for, no dialog. Escape abandons the edit, Enter and blur keep it, and an
 * empty name is a no-op rather than a nameless conversation.
 */
function TitleField({
  title,
  onRename,
}: {
  title: string;
  onRename: (next: string) => void;
}) {
  const { t } = useI18n();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(title);
  const field = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (editing) field.current?.select();
  }, [editing]);

  function commit() {
    const next = draft.trim();
    if (next && next !== title) onRename(next);
    setEditing(false);
  }

  if (editing) {
    return (
      <input
        ref={field}
        value={draft}
        aria-label={t("chat.renameAria")}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            commit();
          }
          if (e.key === "Escape") {
            e.preventDefault();
            setDraft(title);
            setEditing(false);
          }
        }}
        // Sized and weighted exactly like the heading it replaces, so the row
        // does not jump when it swaps in.
        className="h-7 w-full min-w-0 max-w-[22rem] rounded-md border border-iris/60 bg-ink px-2 font-display text-[0.95rem] font-semibold text-fg outline-none ring-2 ring-iris/20"
      />
    );
  }

  return (
    <button
      type="button"
      title={t("chat.renameHint")}
      onClick={() => {
        setDraft(title);
        setEditing(true);
      }}
      className="group flex h-7 min-w-0 items-center gap-1.5 rounded-md px-2 -mx-2 text-left transition-colors duration-150 hover:bg-line/40 focus:outline-none focus-visible:ring-2 focus-visible:ring-iris/40"
    >
      <h1 className="truncate font-display text-[0.95rem] font-semibold text-fg">{title}</h1>
      <Pencil className="h-3 w-3 shrink-0 text-transparent transition-colors duration-150 group-hover:text-faint group-focus-visible:text-faint" />
    </button>
  );
}

/**
 * Which machines answer the next message.
 *
 * Under the composer, not up in the title bar: it is part of writing the
 * message, and every change to it is written into the transcript so a thread
 * that switched model halfway still reads correctly later.
 */
function ModelPicker({
  value,
  options,
  onChange,
}: {
  value: string;
  options: string[];
  onChange: (next: string) => void;
}) {
  const { t } = useI18n();
  if (options.length === 0) {
    return value ? (
      <span className="inline-flex h-8 items-center gap-2 rounded-lg border border-line bg-raised px-2.5 font-mono text-[0.75rem] text-muted">
        <Boxes className="h-3.5 w-3.5 text-faint" />
        {value}
      </span>
    ) : null;
  }
  return (
    <div className="group relative inline-flex h-8 min-w-0 items-center rounded-lg border border-line bg-raised transition-colors duration-150 hover:border-line-strong focus-within:border-iris focus-within:ring-2 focus-within:ring-iris/25">
      <Boxes className="pointer-events-none absolute left-2.5 h-3.5 w-3.5 text-faint transition-colors duration-150 group-hover:text-muted" />
      <select
        value={value}
        aria-label={t("chat.model")}
        onChange={(e) => onChange(e.target.value)}
        // `appearance-none` so the pill is ours; the arrow below replaces the
        // platform one, which sat outside this border on every OS.
        className="h-8 w-full min-w-0 max-w-[18rem] cursor-pointer appearance-none truncate bg-transparent pl-8 pr-7 text-[0.78rem] text-fg focus:outline-none"
      >
        {options.map((m) => (
          <option key={m} value={m}>
            {m}
          </option>
        ))}
      </select>
      <ChevronDown className="pointer-events-none absolute right-2 h-3.5 w-3.5 text-faint transition-colors duration-150 group-hover:text-muted" />
    </div>
  );
}

function SessionList({
  sessions,
  activeId,
  onPick,
  onNew,
  onDelete,
}: {
  sessions: ChatSession[];
  activeId: string;
  onPick: (s: ChatSession) => void;
  onNew: () => void;
  onDelete: (id: string) => void;
}) {
  const { t } = useI18n();
  return (
    <aside className="hidden w-[248px] shrink-0 flex-col border-r border-line bg-surface md:flex">
      <div className="p-3">
        <Button
          variant="primary"
          className="w-full"
          icon={<Plus className="h-4 w-4" />}
          onClick={onNew}
        >
          {t("chat.new")}
        </Button>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
        {sessions.length === 0 ? (
          <p className="px-3 py-6 text-center text-xs leading-relaxed text-faint">
            {t("chat.localOnly")}
          </p>
        ) : (
          sessions.map((s) => (
            <div
              key={s.id}
              className={clsx(
                "group relative mb-0.5 rounded-lg transition-colors duration-150",
                s.id === activeId ? "bg-iris/12 ring-1 ring-inset ring-iris/25" : "hover:bg-line/40",
              )}
            >
              <button
                onClick={() => onPick(s)}
                className="block w-full px-3 py-2 pr-8 text-left"
              >
                <span className="block truncate text-[0.82rem] text-fg">{s.title}</span>
                <span className="block text-[0.68rem] text-faint">
                  {since(s.updatedAt)} · {s.messages.length}{" "}
                  {t("chat.message", { count: s.messages.length })}
                </span>
              </button>
              <button
                onClick={() => onDelete(s.id)}
                aria-label={t("chat.delete", { title: s.title })}
                className="absolute right-1.5 top-1/2 hidden -translate-y-1/2 rounded p-1.5 text-faint transition-colors hover:text-rose group-hover:block"
              >
                <Trash2 className="h-3.5 w-3.5" />
              </button>
            </div>
          ))
        )}
      </div>
    </aside>
  );
}

export function Chat() {
  const { t, locale } = useI18n();
  const { sessions, upsert, remove } = useChatSessions();
  const models = useServableModels();
  const cluster = useCluster();
  const node = useNode();
  const toast = useToast();

  const available = useMemo(
    () => chatModelOptions([
      ...(models.data ?? []).map((m) => m.id),
      ...(cluster.data?.model_inventory.filter((entry) => entry.complete).map((entry) => entry.model_id) ?? []),
    ]),
    [cluster.data?.model_inventory, models.data],
  );

  const [session, setSession] = useState<ChatSession>(() => sessions[0] ?? emptySession(null));
  const [draft, setDraft] = useState("");
  const [streaming, setStreaming] = useState(false);
  const [telemetryCapture, setTelemetryCapture] = useState<{
    messageId: string;
    trace: RequestTrace;
    cluster: ClusterView | null;
  } | null>(null);
  const [telemetrySnapshots, setTelemetrySnapshots] = useState<Record<string, ExecutionTelemetryEnvironment>>({});
  const abort = useRef<AbortController | null>(null);
  const thread = useRef<HTMLDivElement>(null);
  const autoFollow = useRef(true);
  const composer = useRef<HTMLTextAreaElement>(null);

  const telemetryNodeIds = useMemo(
    () => Array.from(new Set(telemetryCapture?.trace.hops.map((hop) => hop.node_id) ?? [])).sort(),
    [telemetryCapture],
  );
  const telemetryNetwork = useChatTelemetryNetwork(telemetryNodeIds, telemetryCapture != null, telemetryCapture?.messageId);
  const runtimeQueries = usePeerRuntimes(telemetryNodeIds, telemetryCapture != null, telemetryCapture?.messageId);
  const runtimeByNode = useStableRecord(
    Object.fromEntries(
      telemetryNodeIds.map((nodeId, index) => [nodeId, runtimeQueries[index]?.data]),
    ) as Record<string, PeerRuntime | undefined>,
  );
  const runtimesLoading = runtimeQueries.some((query) => query.isLoading);
  const telemetryEnvironment = useMemo<ExecutionTelemetryEnvironment | undefined>(
    () => telemetryCapture
      ? {
          snapshotCaptured: true,
          cluster: telemetryCapture.cluster,
          network: telemetryNetwork.data,
          networkLoading: telemetryNetwork.isLoading,
          networkError: telemetryNetwork.isError,
          runtimes: runtimeByNode,
          runtimesLoading,
        }
      : undefined,
    [
      telemetryCapture,
      telemetryNetwork.data,
      telemetryNetwork.isLoading,
      telemetryNetwork.isError,
      runtimeByNode,
      runtimesLoading,
    ],
  );

  useEffect(() => {
    if (!telemetryCapture || !telemetryEnvironment) return;
    setTelemetrySnapshots((previous) => {
      const current = previous[telemetryCapture.messageId];
      if (
        current?.cluster === telemetryEnvironment.cluster &&
        current?.network === telemetryEnvironment.network &&
        current?.runtimes === telemetryEnvironment.runtimes &&
        current?.networkLoading === telemetryEnvironment.networkLoading &&
        current?.networkError === telemetryEnvironment.networkError &&
        current?.runtimesLoading === telemetryEnvironment.runtimesLoading
      ) return previous;
      return { ...previous, [telemetryCapture.messageId]: telemetryEnvironment };
    });
  }, [telemetryCapture, telemetryEnvironment]);

  // A session with no model yet takes whatever the network is serving.
  const model = session.model && available.includes(session.model) ? session.model : available[0] ?? cluster.data?.active_model ?? "";

  useEffect(() => {
    const element = thread.current;
    if (element && autoFollow.current) element.scrollTop = element.scrollHeight;
  }, [session.messages, streaming]);

  useEffect(() => () => abort.current?.abort(), []);

  const grow = useCallback(() => {
    const el = composer.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 220)}px`;
  }, []);

  async function send(options?: { visibleText?: string; requestContent?: string }) {
    const text = options?.visibleText ?? draft.trim();
    if (!text || streaming || !model) return;

    const user: ChatMessage = {
      id: newId(),
      role: "user",
      content: text,
      requestContent: options?.requestContent,
      at: new Date().toISOString(),
    };
    const reply: ChatMessage = {
      id: newId(),
      role: "assistant",
      content: "",
      at: new Date().toISOString(),
    };

    // Snapshot the turns before this one: the request carries the conversation,
    // and the placeholder reply must not travel with it.
    const history = [...session.messages, user];
    let live: ChatSession = {
      ...session,
      model,
      modelAtStart: session.modelAtStart ?? model,
      // A name the operator typed outranks the one we would guess from the
      // first thing they said.
      title:
        session.titled || turnsOnly(session.messages).length > 0
          ? session.title
          : titleFrom(text),
      messages: [...history, reply],
    };
    setSession(live);
    setDraft("");
    setTelemetryCapture(null);
    setStreaming(true);
    autoFollow.current = true;
    window.requestAnimationFrame(grow);

    const controller = new AbortController();
    abort.current = controller;

    const patchReply = (patch: Partial<ChatMessage>) => {
      live = {
        ...live,
        messages: live.messages.map((m) => (m.id === reply.id ? { ...m, ...patch } : m)),
      };
      setSession(live);
    };

    try {
      let streamed = emptyChatStream();
      for await (const data of streamSse(
        "/v1/chat/completions",
        {
          model,
          stream: true,
          // Otherwise the server falls back to its own conservative default
          // (openai.rs's DEFAULT_MAX_TOKENS = 512), independent of whatever
          // the operator actually configured for this node — a response
          // could never run longer than that no matter the deployment.
          // `config` is null before a model is deployed; omitting the field
          // then just leaves that same server-side default in place.
          ...(node.data?.config?.max_new_tokens
            ? { max_tokens: node.data.config.max_new_tokens }
            : {}),
          messages: turnsOnly(history).map((m) => ({ role: m.role, content: m.content })),
        },
        controller.signal,
      )) {
        if (data === "[DONE]") break;
        try {
          const chunk = JSON.parse(data);
          const streamError = streamErrorMessage(chunk);
          if (streamError) {
            streamed = consumeChatError(streamed, streamError);
            patchReply({ error: streamed.error });
            continue;
          }
          const delta = chunk?.choices?.[0]?.delta?.content;
          if (typeof delta === "string" && delta) {
            streamed = consumeChatDelta(streamed, delta);
            patchReply({ content: streamed.content, reasoning: streamed.reasoning || undefined });
          }
          const metrics = chunk?.x_pom;
          if (metrics && typeof metrics === "object") {
            streamed = finishChatStream(streamed, metrics);
            patchReply({
              content: streamed.content,
              reasoning: streamed.reasoning || undefined,
              tokensPerSecond: streamed.tokensPerSecond,
              tokensGenerated: streamed.tokensGenerated,
              durationMs: streamed.durationMs,
              timings: streamed.timings,
              trace: streamed.trace,
              source: typeof metrics.source === "string" ? metrics.source : undefined,
            });
          }
        } catch {
          /* a keep-alive or a partial frame — nothing to render */
        }
      }
      streamed = finishChatStream(streamed);
      patchReply({ content: streamed.content, reasoning: streamed.reasoning || undefined });
      if (streamed.trace) {
        setTelemetryCapture({ messageId: reply.id, trace: streamed.trace, cluster: cluster.data ?? null });
      }
      if (!streamed.content && !streamed.reasoning && !streamed.error) {
        patchReply({
          error:
            t("chat.emptyResponse"),
        });
      }
    } catch (e) {
      if (controller.signal.aborted) {
        patchReply({ error: t("chat.interrupted") });
      } else {
        const detail = e instanceof Error ? e.message : String(e);
        patchReply({ error: detail });
        if (e instanceof ApiError && (e.status === 401 || e.status === 403)) {
          toast.error(
            t("chat.keyRejected"),
            t("chat.keyRejectedDetail"),
          );
        }
      }
    } finally {
      setStreaming(false);
      abort.current = null;
      upsert(live);
    }
  }

  function analyzeTrace(trace: RequestTrace) {
    void send({
      visibleText: t("chat.telemetry.analysisPrompt"),
      requestContent: buildTelemetryAnalysisPrompt(trace, locale === "pt-BR"),
    });
  }

  /**
   * Switch the model, and say so in the transcript.
   *
   * The mark is only written once the thread has turns in it: on an empty
   * conversation there is nothing for the change to be *between*, and a
   * transcript that opens with "model changed to X" before anybody spoke reads
   * like a bug.
   */
  function chooseModel(next: string) {
    if (!next || next === model) return;
    const changed = turnsOnly(session.messages).length > 0;
    const updated: ChatSession = {
      ...session,
      model: next,
      modelAtStart: changed ? session.modelAtStart : next,
      messages: changed
        ? [
            ...session.messages,
            {
              id: newId(),
              role: "event",
              content: t("chat.modelChanged", { model: next }),
              at: new Date().toISOString(),
            },
          ]
        : session.messages,
    };
    setSession(updated);
    autoFollow.current = true;
    // Straight to storage: the operator may switch model and walk away, and a
    // thread that only saves on send would lose both the choice and its mark.
    if (updated.messages.length > 0) upsert(updated);
  }

  function rename(next: string) {
    const updated: ChatSession = { ...session, title: next, titled: true };
    setSession(updated);
    if (updated.messages.length > 0) upsert(updated);
  }

  function startNew() {
    abort.current?.abort();
    setSession(emptySession(model || null));
    setDraft("");
    setTelemetryCapture(null);
  }

  function deleteSession(id: string) {
    remove(id);
    setTelemetrySnapshots((previous) => {
      if (!(id in previous)) return previous;
      const next = { ...previous };
      delete next[id];
      return next;
    });
    if (id === session.id) {
      setSession(emptySession(model || null));
      setTelemetryCapture(null);
    }
  }

  const noModel = available.length === 0 && !cluster.data?.active_model;
  const firstTraceModel = session.messages.find((message) => message.trace?.model)?.trace?.model;
  const modelAtStart = session.modelAtStart ?? firstTraceModel ?? session.model ?? model;

  return (
    <div className="flex h-full min-h-0">
      <SessionList
        sessions={sessions}
        activeId={session.id}
        onPick={(s) => {
          abort.current?.abort();
          setSession(s);
          setTelemetryCapture(null);
        }}
        onNew={startNew}
        onDelete={deleteSession}
      />

      <div className="flex min-w-0 flex-1 flex-col">
        <div className="flex h-14 shrink-0 items-center justify-between gap-3 border-b border-line px-4 sm:px-6">
          <div className="flex min-w-0 flex-1 items-center gap-2">
            <TitleField
              // A conversation nobody has spoken in has no name to keep, so it
              // shows what the screen is instead of an empty title to rename.
              title={session.messages.length ? session.title : t("chat.test")}
              onRename={rename}
            />
            {cluster.data && !cluster.data.complete && (
              <Badge tone="amber">{t("chat.incomplete")}</Badge>
            )}
          </div>
          <div className="flex shrink-0 items-center gap-2">
            <Button
              size="sm"
              variant="ghost"
              icon={<Plus className="h-3.5 w-3.5" />}
              onClick={startNew}
              className="md:hidden"
            >
              {t("chat.newShort")}
            </Button>
          </div>
        </div>

        <div ref={thread} onScroll={(event) => { autoFollow.current = isNearBottom(event.currentTarget); }} className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden">
          <div className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6">
            {session.messages.length === 0 ? (
              <div className="pt-[12vh]">
                <Empty
                  icon={<MessagesSquare className="h-5 w-5" />}
                  title={
                    noModel
                      ? t("chat.noModel")
                      : t("chat.ask")
                  }
                >
                  {noModel
                    ? t("chat.noModelDetail")
                    : t("chat.askDetail")}
                </Empty>
              </div>
            ) : (
              <div className="space-y-6">
                <AnimatePresence initial={false}>
                  {modelAtStart && <ModelIntroMark model={modelAtStart} />}
                  {session.messages.map((m) => (
                    <Bubble
                      key={m.id}
                      message={m}
                      streaming={streaming && m.id === session.messages[session.messages.length - 1]?.id}
                      onAnalyze={analyzeTrace}
                      analysisDisabled={streaming || !model}
                      telemetryEnvironment={telemetryCapture?.messageId === m.id ? telemetryEnvironment : telemetrySnapshots[m.id]}
                    />
                  ))}
                </AnimatePresence>
                {streaming &&
                  !session.messages[session.messages.length - 1]?.content && (
                    <div className="flex items-center gap-2 text-sm text-muted">
                      <span className="flex gap-1">
                        {[0, 1, 2].map((i) => (
                          <span
                            key={i}
                            className="h-1.5 w-1.5 rounded-full bg-iris"
                            style={{
                              animation: "pulse-soft 1.2s ease-in-out infinite",
                              animationDelay: `${i * 0.16}s`,
                            }}
                          />
                        ))}
                      </span>
                      {t("chat.through")}
                    </div>
                  )}
              </div>
            )}
          </div>
        </div>

        <div className="shrink-0 border-t border-line bg-surface/80 px-4 py-3 backdrop-blur-md sm:px-6">
          <div className="mx-auto w-full max-w-3xl">
            <div className="flex items-end gap-2 rounded-xl border border-line bg-ink p-2 transition-colors duration-150 focus-within:border-iris/60 focus-within:ring-2 focus-within:ring-iris/20">
              <textarea
                ref={composer}
                rows={1}
                value={draft}
                placeholder={t("chat.placeholder")}
                onChange={(e) => {
                  setDraft(e.target.value);
                  grow();
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    void send();
                  }
                }}
                className="max-h-[220px] min-h-[2.25rem] flex-1 resize-none bg-transparent px-2 py-1.5 text-[0.9rem] leading-relaxed text-fg placeholder:text-faint focus:outline-none"
              />
              {streaming ? (
                <Button
                  variant="outline"
                  icon={<Square className="h-3.5 w-3.5" />}
                  onClick={() => abort.current?.abort()}
                >
                  {t("chat.stop")}
                </Button>
              ) : (
                <Button
                  variant="primary"
                  disabled={!draft.trim()}
                  onClick={() => void send()}
                  aria-label={t("chat.send")}
                  className="w-9 px-0"
                >
                  <ArrowUp className="h-4 w-4" />
                </Button>
              )}
            </div>
            {/* Under the box, on the same baseline: what answers on the left,
                how to send on the right. */}
            <div className="mt-2 flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
              <ModelPicker value={model} options={available} onChange={chooseModel} />
              <p className="text-[0.7rem] text-faint">{t("chat.composerHint")}</p>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
