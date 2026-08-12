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
  MessagesSquare,
  Plus,
  Square,
  Trash2,
  TriangleAlert,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Markdown } from "../components/Markdown";
import { Badge, Button, Empty, Select } from "../components/ui";
import { ApiError, streamSse } from "../lib/api";
import { isNearBottom } from "../lib/chatScroll";
import { consumeChatDelta, emptyChatStream, finishChatStream } from "../lib/chatStream";
import {
  emptySession,
  newId,
  titleFrom,
  useChatSessions,
  type ChatMessage,
  type ChatSession,
} from "../lib/chatStore";
import { bytes, since } from "../lib/format";
import type { RequestTrace } from "../lib/chatStream";
import { useCluster, useServableModels } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import { localizeServerText } from "../i18n/locales";

function Bubble({ message, streaming }: { message: ChatMessage; streaming: boolean }) {
  const { locale } = useI18n();
  const isUser = message.role === "user";
  const labels = locale === "pt-BR"
    ? { thinking: "Pensamento", speed: "tok/s", tokens: "tokens", time: "s" }
    : { thinking: "Thinking", speed: "tok/s", tokens: "tokens", time: "s" };
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
              <summary className="cursor-pointer select-none text-xs font-medium text-iris">{labels.thinking}</summary>
              <div className="mt-2 text-sm text-muted"><Markdown>{message.reasoning}</Markdown></div>
            </details>
          )}
          {message.content && <Markdown>{message.content}</Markdown>}
          {message.error && (
            <div className="mt-2 flex items-start gap-2.5 rounded-lg border border-rose/30 bg-rose/[0.07] px-3 py-2.5">
              <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0 text-rose" />
              <p className="break-path text-[0.82rem] leading-relaxed text-muted">
                {localizeServerText(locale, message.error)}
              </p>
            </div>
          )}
          {message.source && (
            <p className="mt-2 font-mono text-[0.68rem] text-faint">{message.source}</p>
          )}
          {message.tokensPerSecond != null && (
            <div className="mt-2 flex flex-wrap items-center gap-2 font-mono text-[0.68rem] text-faint">
              <span>{message.tokensPerSecond.toFixed(1)} {labels.speed}{message.tokensGenerated != null && ` · ${message.tokensGenerated} ${labels.tokens}`}{message.durationMs != null && ` · ${(message.durationMs / 1000).toFixed(1)} ${labels.time}`}</span>
              {message.trace && <ExecutionTelemetry trace={message.trace} source={message.source} />}
            </div>
          )}
        </div>
      )}
    </motion.div>
  );
}

function ExecutionTelemetry({ trace, source }: { trace: RequestTrace; source?: string }) {
  return <details className="w-full rounded-lg border border-line bg-raised px-3 py-2 text-[0.72rem] text-muted" open>
    <summary className="cursor-pointer font-medium text-fg">Telemetria da execução {source ? `· ${source}` : ""}</summary>
    <div className="mt-2 flex flex-wrap gap-x-3 gap-y-1 font-mono"><span>{trace.input_tokens} in</span><span>{trace.output_tokens} out</span><span>{trace.prefill_ms} ms prefill</span><span>{bytes(trace.total_bytes_sent + trace.total_bytes_received)}</span></div>
    <div className="mt-2 space-y-2">{trace.hops.map((hop) => {
      const nodeTotal = hop.prefill_ms + hop.decode_ms;
      const namedNode = hop.node_frame_decode_ms + hop.node_input_prepare_ms + hop.node_queue_ms + hop.node_setup_ms + hop.node_decode_ms + hop.node_residual_ms + hop.node_sampling_ms + hop.node_encode_ms;
      const nodeOther = Math.max(0, nodeTotal - namedNode);
      return <div key={`${hop.node_id}:${hop.start_layer}`} className="rounded border border-line/70 bg-ink/40 px-2.5 py-2">
        <div className="flex flex-wrap justify-between gap-x-3 gap-y-1 text-fg"><span>L{hop.start_layer}-L{hop.end_layer} · {hop.node_id.slice(0, 10)}</span><span className="font-mono text-faint">{hop.calls} chamadas · {hop.prefill_calls} prefill + {hop.decode_calls} tokens · {bytes(hop.bytes_sent + hop.bytes_received)}</span></div>
        <div className="mt-1.5 grid gap-1 font-mono text-[0.66rem] leading-relaxed sm:grid-cols-2">
          <span>gateway: codificar {hop.client_encode_ms} ms · até headers {hop.response_headers_ms} ms · corpo {hop.response_body_ms} ms · decodificar {hop.client_decode_ms} ms</span>
          <span>nó: frame {hop.node_frame_decode_ms} ms · preparar {hop.node_input_prepare_ms} ms · fila {hop.node_queue_ms} ms · setup {hop.node_setup_ms} ms</span>
          <span>GPU/engine decode {hop.node_decode_ms} ms · residual D2H/sync {hop.node_residual_ms} ms · sampling {hop.node_sampling_ms} ms · frame out {hop.node_encode_ms} ms</span>
          <span>decode/token p50 {hop.node_decode_p50_ms} ms · p95 {hop.node_decode_p95_ms} ms · residual HTTP {hop.network_ms} ms{nodeOther > 0 ? ` · nó não atribuído ${nodeOther} ms` : ""}</span>
        </div>
      </div>;
    })}</div>
    <p className="mt-2 text-faint">“Até headers” inclui upload, rede e trabalho do nó. “Residual HTTP” é a parte da ida e volta fora do handler do nó, não latência de pacote pura. Residual D2H/sync mede a leitura do estado oculto que força GPU → CPU.</p>
  </details>;
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
  const { t } = useI18n();
  const { sessions, upsert, remove } = useChatSessions();
  const models = useServableModels();
  const cluster = useCluster();
  const toast = useToast();

  const available = useMemo(
    () => Array.from(new Set([...(models.data ?? []).map((m) => m.id), ...(cluster.data?.model_inventory.filter((entry) => entry.complete).map((entry) => entry.model_id) ?? [])])).filter(Boolean).sort(),
    [cluster.data?.model_inventory, models.data],
  );

  const [session, setSession] = useState<ChatSession>(() => sessions[0] ?? emptySession(null));
  const [draft, setDraft] = useState("");
  const [streaming, setStreaming] = useState(false);
  const abort = useRef<AbortController | null>(null);
  const thread = useRef<HTMLDivElement>(null);
  const autoFollow = useRef(true);
  const composer = useRef<HTMLTextAreaElement>(null);

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

  async function send() {
    const text = draft.trim();
    if (!text || streaming) return;

    const user: ChatMessage = {
      id: newId(),
      role: "user",
      content: text,
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
      title: session.messages.length === 0 ? titleFrom(text) : session.title,
      messages: [...history, reply],
    };
    setSession(live);
    setDraft("");
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
          messages: history.map((m) => ({ role: m.role, content: m.content })),
        },
        controller.signal,
      )) {
        if (data === "[DONE]") break;
        try {
          const chunk = JSON.parse(data);
          const delta = chunk?.choices?.[0]?.delta?.content;
          if (typeof delta === "string" && delta) {
            streamed = consumeChatDelta(streamed, delta);
            patchReply({ content: streamed.content, reasoning: streamed.reasoning || undefined });
          }
          const metrics = chunk?.x_rdma_p2p;
          if (metrics && typeof metrics === "object") {
            streamed = finishChatStream(streamed, metrics);
            patchReply({
              content: streamed.content,
              reasoning: streamed.reasoning || undefined,
              tokensPerSecond: streamed.tokensPerSecond,
              tokensGenerated: streamed.tokensGenerated,
              durationMs: streamed.durationMs,
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
      if (!streamed.content && !streamed.reasoning) {
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

  function startNew() {
    abort.current?.abort();
    setSession(emptySession(model || null));
    setDraft("");
  }

  function deleteSession(id: string) {
    remove(id);
    if (id === session.id) setSession(emptySession(model || null));
  }

  const noModel = available.length === 0 && !cluster.data?.active_model;

  return (
    <div className="flex h-full min-h-0">
      <SessionList
        sessions={sessions}
        activeId={session.id}
        onPick={(s) => {
          abort.current?.abort();
          setSession(s);
        }}
        onNew={startNew}
        onDelete={deleteSession}
      />

      <div className="flex min-w-0 flex-1 flex-col">
        <div className="flex h-14 shrink-0 items-center justify-between gap-3 border-b border-line px-4 sm:px-6">
          <div className="flex min-w-0 items-center gap-2">
            <h1 className="truncate font-display text-[0.95rem] font-semibold text-fg">
              {session.messages.length ? session.title : t("chat.test")}
            </h1>
            {cluster.data && !cluster.data.complete && (
              <Badge tone="amber">{t("chat.incomplete")}</Badge>
            )}
          </div>
          <div className="flex shrink-0 items-center gap-2">
            {available.length > 0 ? (
              <Select
                value={model}
                onChange={(e) => setSession({ ...session, model: e.target.value })}
                className="w-44"
                aria-label={t("chat.model")}
              >
                {available.map((m) => (
                  <option key={m} value={m}>
                    {m}
                  </option>
                ))}
              </Select>
            ) : (
              model && <Badge mono>{model}</Badge>
            )}
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
                  {session.messages.map((m) => (
                    <Bubble key={m.id} message={m} streaming={streaming && m.id === session.messages[session.messages.length - 1]?.id} />
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
            <p className="mt-2 text-center text-[0.7rem] text-faint">
              {t("chat.composerHint")}
            </p>
          </div>
        </div>
      </div>
    </div>
  );
}
