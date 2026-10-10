/**
 * The chat.
 *
 * It talks to the cluster the same way any other client does — `POST
 * /v1/chat/completions` with `stream: true`, using the dedicated API key owned
 * by this UI, so what shows up here is exactly what a harness would get. No
 * separate inference path or shortcut through the local node.
 */
import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import {
  ArrowUp,
  Boxes,
  Check,
  ChevronDown,
  Copy,
  Database,
  Mic,
  MicOff,
  Paperclip,
  Pencil,
  Plus,
  Square,
  TriangleAlert,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Markdown } from "../components/Markdown";
import { ChatSessionList } from "../components/ChatSessionList";
import { ChatHero } from "../components/ChatHero";
import { MissingPiece } from "../components/MissingPiece";
import { Link } from "react-router-dom";
import { canAccessRoute } from "../lib/access";
import { ChatWaitingLine } from "../components/ChatWaitingLine";
import { PluginSlot, usePluginSlot } from "../components/PluginSlot";
import { TelemetryLine } from "../components/TelemetryLine";
import { InferenceHeadChoice } from "../components/InferenceHeadChoice";
import { Badge, Button } from "../components/ui";
import { isNearBottom } from "../lib/chatScroll";
import { copyText } from "../lib/clipboard";
import {
  CHAT_TELEMETRY_SLOT,
  type ChatTelemetryEnvironment,
  type ChatTelemetryQuestion,
  type ChatTelemetrySlotProps,
} from "../lib/chatTelemetry";
import type { ResolvedPluginSlot } from "../lib/pluginUi";
import {
  ChatSessionProvider,
  hasConversations,
  newId,
  titleFrom,
  turnsOnly,
  useChatSessionController,
  useOptionalChatSessionController,
  type ChatAttachment,
  type ChatMessage,
  type ChatRequestPart,
  type ChatSession,
} from "../lib/chatStore";
import {
  imageBudgetBytes,
  ImageResizeLimitError,
  resizeImageForVision,
} from "../lib/imageResize";
import { ms, num } from "../lib/format";
import type { GenerationTimings, RequestTrace } from "../lib/chatStream";
import {
  useChatTelemetryNetwork,
  useChatCapabilities,
  useCluster,
  useNode,
  usePeerRuntimes,
  useInferenceHeads,
  useServableModels,
} from "../lib/queries";
import { useStableRecord } from "../lib/stableRecord";
import { chatModelOptions } from "../lib/chatModels";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import type { ClusterView, PeerRuntime } from "../lib/types";
import { useProfile } from "../lib/profile";

export type { ChatTelemetryEnvironment } from "../lib/chatTelemetry";
export { OpenNativeChatWindowButton } from "../components/OpenNativeChatWindowButton";

function isTextFile(file: File): boolean {
  if (file.type.startsWith("text/")) return true;
  return /\.(c|cc|cpp|css|csv|go|h|hpp|html?|ini|java|js|json|jsx|log|md|py|rs|sh|sql|toml|ts|tsx|txt|xml|yaml|yml)$/i.test(file.name);
}

function attachmentFormat(attachment: ChatAttachment): string {
  if (attachment.kind === "image") return "IMG";
  const extension = attachment.name.match(/\.([^.]+)$/)?.[1];
  if (extension) return extension.slice(0, 6).toUpperCase();
  return "TXT";
}

type SpeechRecognitionResultLike = {
  isFinal: boolean;
  [index: number]: { transcript: string };
};

type SpeechRecognitionEventLike = {
  resultIndex: number;
  results: ArrayLike<SpeechRecognitionResultLike>;
};

type SpeechRecognitionLike = {
  lang: string;
  continuous: boolean;
  interimResults: boolean;
  onresult: ((event: SpeechRecognitionEventLike) => void) | null;
  onerror: (() => void) | null;
  onend: (() => void) | null;
  start: () => void;
  stop: () => void;
};

type SpeechRecognitionConstructor = new () => SpeechRecognitionLike;

type SpeechWindow = Window & {
  SpeechRecognition?: SpeechRecognitionConstructor;
  webkitSpeechRecognition?: SpeechRecognitionConstructor;
};

function speechRecognitionConstructor(): SpeechRecognitionConstructor | undefined {
  if (typeof window === "undefined") return undefined;
  // Browsers only hand the microphone to a secure page (HTTPS or localhost):
  // on plain HTTP the recognizer exists and every attempt fails, so the
  // button would be there and never work.
  if (window.isSecureContext === false) return undefined;
  const speechWindow = window as SpeechWindow;
  return speechWindow.SpeechRecognition ?? speechWindow.webkitSpeechRecognition;
}

function speechLanguage(locale: string): string {
  return locale === "pt-BR" ? "pt-BR" : "en-US";
}

function estimatedAttachmentBytes(attachment: ChatAttachment): number {
  return attachment.size ?? new TextEncoder().encode(attachment.data).byteLength;
}


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
    // The message remains selectable when the copy fails.
    if (!(await copyText(message))) return;
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1600);
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

interface ChatTelemetryCapture {
  messageId: string;
  trace: RequestTrace;
  timings?: GenerationTimings;
  source?: string;
  cluster: ClusterView | null;
}

interface ChatTelemetrySnapshot extends ChatTelemetryCapture {
  environment?: ChatTelemetryEnvironment;
}

export function Bubble({
  message,
  streaming,
  onAsk,
  analysisDisabled = false,
  telemetryPlugin = null,
  telemetryEnvironment,
  telemetryTrace,
  telemetryTimings,
  telemetrySource,
}: {
  message: ChatMessage;
  streaming: boolean;
  /** Sends a follow-up turn on behalf of the telemetry plugin. */
  onAsk?: (question: ChatTelemetryQuestion) => void;
  analysisDisabled?: boolean;
  /** The plugin claiming the `chat.telemetry` slot, when one is active. */
  telemetryPlugin?: ResolvedPluginSlot | null;
  telemetryEnvironment?: ChatTelemetryEnvironment;
  /** The final stream capture can arrive one render before the transcript patch. */
  telemetryTrace?: RequestTrace;
  /** The final stream timings can arrive with the trace before the transcript patch. */
  telemetryTimings?: GenerationTimings;
  /** The final stream source can arrive with the trace before the transcript patch. */
  telemetrySource?: string;
}) {
  const { t } = useI18n();
  const { profile } = useProfile();
  const showTelemetry = profile !== "infer";
  const trace = message.trace ?? telemetryTrace;
  const timings = message.timings ?? telemetryTimings;
  const source = message.source ?? telemetrySource;
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
          {message.content && (
            <p className="whitespace-pre-wrap text-[0.9rem] leading-relaxed text-fg">
              {message.content}
            </p>
          )}
          {message.attachments?.map((attachment) => attachment.kind === "image" ? (
            <figure key={attachment.id} className={clsx(message.content && "mt-3", "max-w-full")}>
              <img
                src={attachment.data}
                alt={t("chat.attachmentAlt", { name: attachment.name })}
                className="max-h-[28rem] max-w-full rounded-xl border border-line object-contain"
              />
              <figcaption className="mt-1.5 flex min-w-0 items-center gap-1.5 text-[0.7rem] text-faint" title={attachment.name}>
                <span className="min-w-0 truncate">{attachment.name}</span>
                {attachment.resized && (
                  <span className="shrink-0 rounded border border-amber/25 bg-amber/[0.08] px-1 py-0.5 text-[0.62rem] text-amber" title={t("chat.attachmentResized")}>
                    {t("chat.attachmentResized")}
                  </span>
                )}
              </figcaption>
            </figure>
          ) : (
            <div key={attachment.id} className={clsx(message.content && "mt-2", "flex items-center gap-2 text-[0.75rem] text-muted")}>
              <Paperclip className="h-3.5 w-3.5 shrink-0 text-faint" />
              <span className="truncate" title={attachment.name}>{attachment.name}</span>
            </div>
          ))}
        </div>
      ) : (
        <div className="min-w-0 flex-1">
          {message.reasoning && (
            <ThinkingBlock
              reasoning={message.reasoning}
              tokens={message.reasoningTokens}
              // Still thinking: no answer text has arrived yet.
              live={streaming && !message.content}
            />
          )}
          {message.content && <Markdown>{message.content}</Markdown>}
          {message.error && (
            <ChatError message={message.error} />
          )}
          {showTelemetry && !trace && !timings && source && (
            <p className="mt-2 font-mono text-[0.68rem] text-faint">{source}</p>
          )}
          {showTelemetry && !trace && !timings && message.tokensPerSecond != null && (
            <div className="mt-2 flex flex-wrap items-center gap-2 font-mono text-[0.68rem] text-faint">
              <span>{t("chat.legacyTotalSpeed", { value: num(message.tokensPerSecond, 1) })}{message.tokensGenerated != null && ` · ${message.tokensGenerated} ${t("chat.tokensUnit")}`}{message.durationMs != null && ` · ${ms(message.durationMs)}`}</span>
            </div>
          )}
          {showTelemetry && (trace || timings) && (
            <TelemetryLine trace={trace} timings={timings} source={source} className="mt-2">
              {trace && telemetryPlugin && !streaming && (
                <PluginSlot<ChatTelemetrySlotProps>
                  resolved={telemetryPlugin}
                  props={{
                    messageId: message.id,
                    trace,
                    timings,
                    source,
                    environment: telemetryEnvironment,
                    model: trace.model,
                    onAsk,
                    askDisabled: analysisDisabled || streaming,
                  }}
                />
              )}
            </TelemetryLine>
          )}
        </div>
      )}
    </motion.div>
  );
}

/** How many invitations `chat.prompt.<n>` has in `messages.csv`. */
const PROMPT_PLACEHOLDERS = 6;

/**
 * The composer's invitation, in the tone of the waiting lines: one per
 * conversation, picked from its id, so it does not change while typing.
 */
export function promptPlaceholderKey(sessionId: string): string {
  let hash = 0;
  for (const char of sessionId) hash = (hash * 31 + char.charCodeAt(0)) >>> 0;
  return `chat.prompt.${(hash % PROMPT_PLACEHOLDERS) + 1}`;
}

/**
 * A model's thinking, closed: one line saying it thought and for how many
 * tokens. The text opens only when the reader asks for it, while it is still
 * being written as well.
 */
function ThinkingBlock({ reasoning, tokens, live }: { reasoning: string; tokens?: number; live: boolean }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const count = tokens ?? 0;
  return (
    <div className="mb-3" data-testid="chat-thinking">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
        className="inline-flex items-center gap-2 rounded-full border border-iris/25 bg-iris/[0.05] px-3 py-1 text-xs font-medium text-iris transition-colors hover:bg-iris/[0.1] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris"
      >
        {live && (
          <span className="flex gap-0.5" aria-hidden="true">
            {[0, 1, 2].map((i) => (
              <span key={i} className="h-1 w-1 rounded-full bg-iris" style={{ animation: "pulse-soft 1.2s ease-in-out infinite", animationDelay: `${i * 0.16}s` }} />
            ))}
          </span>
        )}
        <span>{t(live ? "chat.thinkingLive" : "chat.thinkingDone")}</span>
        {count > 0 && <span className="font-mono text-[0.68rem] text-iris/80" data-testid="chat-thinking-tokens">{t("chat.thinkingTokens", { count })}</span>}
        <ChevronDown className={clsx("h-3 w-3 transition-transform", open && "rotate-180")} aria-hidden="true" />
      </button>
      <AnimatePresence initial={false}>
        {open && (
          <motion.div
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: "auto", opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={{ duration: 0.2 }}
            className="overflow-hidden"
          >
            <div className="mt-2 rounded-xl border border-iris/20 bg-iris/[0.04] px-3 py-2.5 text-sm text-muted"><Markdown>{reasoning}</Markdown></div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
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
/**
 * Retrieval for this conversation: off, every collection, or one of them.
 * A database icon next to the model picker; it lights up while retrieval is
 * on, and its menu opens upward because the composer sits at the bottom.
 */
function RagMenu({
  enabled,
  collection,
  collections,
  onChange,
}: {
  enabled: boolean;
  collection: string;
  collections: string[];
  onChange: (enabled: boolean, collection: string) => void;
}) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const outside = (event: MouseEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", outside);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("mousedown", outside);
      document.removeEventListener("keydown", escape);
    };
  }, [open]);
  const options = [
    { key: "off", label: t("chat.ragOff"), on: false, name: "" },
    { key: "all", label: t("chat.ragCollectionAll"), on: true, name: "" },
    ...collections.map((name) => ({ key: `c:${name}`, label: name, on: true, name })),
  ];
  const current = !enabled ? "off" : collection ? `c:${collection}` : "all";
  return (
    <div ref={root} className="relative shrink-0">
      <button
        type="button"
        aria-label={t("chat.ragToggle")}
        aria-pressed={enabled}
        aria-haspopup="menu"
        aria-expanded={open}
        title={enabled ? t("chat.ragOn", { collection: collection || t("chat.ragCollectionAll") }) : t("chat.ragToggleHint")}
        onClick={() => setOpen(!open)}
        className={clsx(
          "inline-flex h-8 w-8 items-center justify-center rounded-lg border transition-colors duration-150 focus:outline-none focus-visible:ring-2 focus-visible:ring-iris/25",
          enabled
            ? "border-aqua/50 bg-aqua/[0.1] text-aqua"
            : "border-line bg-raised text-faint hover:border-line-strong hover:text-muted",
        )}
      >
        <Database className="h-3.5 w-3.5" />
      </button>
      {open && (
        <div
          role="menu"
          aria-label={t("chat.ragCollection")}
          className="absolute bottom-full left-0 z-20 mb-1.5 max-h-64 w-56 overflow-y-auto rounded-lg border border-line bg-raised p-1 shadow-lg"
        >
          {options.map((option) => (
            <button
              key={option.key}
              type="button"
              role="menuitemradio"
              aria-checked={current === option.key}
              onClick={() => {
                onChange(option.on, option.name);
                setOpen(false);
              }}
              className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-[0.78rem] text-fg hover:bg-line/40"
            >
              <Check className={clsx("h-3.5 w-3.5 shrink-0 text-aqua", current !== option.key && "invisible")} />
              <span className="min-w-0 truncate">{option.label}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

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

function ChatScreen() {
  const { t, locale } = useI18n();
  const { profile } = useProfile();
  const showTelemetry = profile !== "infer";
  const historyLivesInMainMenu = profile === "infer";
  const {
    sessions,
    session,
    setSession,
    upsert,
    selectSession,
    startNew: startNewSession,
    deleteSession: removeSession,
    registerBeforeChange,
    streamingSessionId,
    lastStreamResult,
    startStream,
    stopStream,
  } = useChatSessionController();
  const models = useServableModels();
  const cluster = useCluster();
  const node = useNode();
  const toast = useToast();

  const available = useMemo(
    () => chatModelOptions([
      ...(models.data ?? []).map((m) => m.id),
      ...(node.data?.active_model ? [node.data.active_model] : []),
      ...(cluster.data?.model_inventory.filter((entry) => entry.complete).map((entry) => entry.model_id) ?? []),
    ]),
    [cluster.data?.model_inventory, models.data, node.data?.active_model],
  );

  const [draft, setDraft] = useState("");
  const [attachments, setAttachments] = useState<ChatAttachment[]>([]);
  const [dragActive, setDragActive] = useState(false);
  const [telemetryCapture, setTelemetryCapture] = useState<ChatTelemetryCapture | null>(null);
  const [telemetrySnapshots, setTelemetrySnapshots] = useState<Record<string, ChatTelemetrySnapshot>>({});
  const [headChoice, setHeadChoice] = useState<{ model: string; nodeId: string | null }>({
    model: "",
    nodeId: null,
  });
  const thread = useRef<HTMLDivElement>(null);
  const autoFollow = useRef(true);
  const composer = useRef<HTMLTextAreaElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const dragDepth = useRef(0);
  const recognition = useRef<SpeechRecognitionLike | null>(null);
  const speechBaseDraft = useRef("");
  const speechTranscript = useRef("");
  const [listening, setListening] = useState(false);

  useEffect(
    () => registerBeforeChange((kind) => {
      setTelemetryCapture(null);
      if (kind === "new") {
        setDraft("");
        setAttachments([]);
      }
    }),
    [registerBeforeChange],
  );

  const telemetryNodeIds = useMemo(
    () => Array.from(new Set(telemetryCapture?.trace.hops.map((hop) => hop.node_id) ?? [])).sort(),
    [telemetryCapture],
  );
  // The per-node environment only feeds the plugin's detailed telemetry.
  const telemetryPlugin = usePluginSlot(CHAT_TELEMETRY_SLOT);
  const environmentWanted = showTelemetry && telemetryCapture != null && telemetryPlugin != null;
  const telemetryNetwork = useChatTelemetryNetwork(telemetryNodeIds, environmentWanted, telemetryCapture?.messageId);
  const runtimeQueries = usePeerRuntimes(telemetryNodeIds, environmentWanted, telemetryCapture?.messageId);
  const runtimeByNode = useStableRecord(
    Object.fromEntries(
      telemetryNodeIds.map((nodeId, index) => [nodeId, runtimeQueries[index]?.data]),
    ) as Record<string, PeerRuntime | undefined>,
  );
  const runtimesLoading = runtimeQueries.some((query) => query.isLoading);
  const telemetryEnvironment = useMemo<ChatTelemetryEnvironment | undefined>(
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
        current?.trace === telemetryCapture.trace &&
        current?.timings === telemetryCapture.timings &&
        current?.source === telemetryCapture.source &&
        current?.cluster === telemetryCapture.cluster &&
        current?.environment === telemetryEnvironment
      ) return previous;
      return {
        ...previous,
        [telemetryCapture.messageId]: {
          ...telemetryCapture,
          environment: telemetryEnvironment,
        },
      };
    });
  }, [telemetryCapture, telemetryEnvironment]);

  // Keep the response metadata by message immediately, before the environment
  // query settles. The transcript normally stores the same values, but the
  // capture is the authoritative bridge across separate React commits.
  useEffect(() => {
    if (!showTelemetry || lastStreamResult?.sessionId !== session.id) return;
    const capture: ChatTelemetryCapture = {
      messageId: lastStreamResult.messageId,
      trace: lastStreamResult.trace,
      timings: lastStreamResult.timings,
      source: lastStreamResult.source,
      cluster: lastStreamResult.cluster,
    };
    setTelemetryCapture(capture);
    setTelemetrySnapshots((previous) => {
      const current = previous[capture.messageId];
      if (
        current?.trace === capture.trace &&
        current?.timings === capture.timings &&
        current?.source === capture.source &&
        current?.cluster === capture.cluster
      ) return previous;
      return { ...previous, [capture.messageId]: { ...current, ...capture } };
    });
  }, [lastStreamResult, session.id, showTelemetry]);

  // A session with no model yet takes whatever the network is serving.
  const model = session.model && available.includes(session.model)
    ? session.model
    : available[0] ?? node.data?.active_model ?? cluster.data?.active_model ?? "";
  const chatCapabilities = useChatCapabilities(model);
  const streaming = streamingSessionId === session.id;
  const headQuery = useInferenceHeads(model, Boolean(model));
  const selectedInferenceHead = headQuery.data?.candidates.some(
    (candidate) => candidate.node_id === headChoice.nodeId && headChoice.model === model,
  )
    ? headChoice.nodeId
    : headQuery.data?.selected_node_id ?? null;

  useEffect(() => {
    const element = thread.current;
    if (element && autoFollow.current) element.scrollTop = element.scrollHeight;
  }, [session.messages, streaming]);

  useEffect(() => () => {
    recognition.current?.stop();
    recognition.current = null;
  }, []);

  const grow = useCallback(() => {
    const el = composer.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 220)}px`;
  }, []);

  const visionAvailable = chatCapabilities.data?.vision === "supported";
  const thinkingAvailable = chatCapabilities.data?.thinking === "supported";
  const thinkingEnabled = session.thinkingEnabled ?? thinkingAvailable;
  // The switch only exists while the network has a RAG server; without one
  // there is nothing to turn off.
  const ragAvailable = chatCapabilities.data?.rag === "supported";
  // Off until the person asks for it: retrieved context is prompt the
  // pipeline has to process on every question.
  const ragEnabled = session.ragEnabled ?? false;
  const ragCollections = chatCapabilities.data?.rag_collections ?? [];
  // A collection that no longer exists is not asked for.
  const ragCollection =
    session.ragCollection && ragCollections.includes(session.ragCollection) ? session.ragCollection : "";
  const servedModel = models.data?.find((candidate) => candidate.id === model);
  const contextLength = servedModel?.context_length
    ?? servedModel?.max_input_tokens
    ?? (node.data?.active_model === model ? node.data?.config?.n_ctx : undefined);

  async function addFiles(files: FileList | File[]) {
    const selectedFiles = Array.from(files);
    const currentBytes = attachments.reduce((total, attachment) => total + estimatedAttachmentBytes(attachment), 0);
    const added: ChatAttachment[] = [];
    for (const [index, file] of selectedFiles.entries()) {
      const usedBytes = currentBytes + added.reduce((total, attachment) => total + estimatedAttachmentBytes(attachment), 0);
      if (file.type.startsWith("image/")) {
        if (!visionAvailable) {
          toast.error(t("chat.imageRejected"), t("chat.imageVisionUnavailable"));
          continue;
        }
        try {
          const remainingImages = selectedFiles
            .slice(index)
            .filter((selectedFile) => selectedFile.type.startsWith("image/")).length;
          const availableBytes = contextLength == null
            ? undefined
            : Math.floor(imageBudgetBytes(contextLength, usedBytes) / Math.max(1, remainingImages));
          const image = await resizeImageForVision(file, availableBytes);
          added.push({
            id: newId(),
            name: file.name,
            kind: "image",
            data: image.data,
            mediaType: image.data.startsWith("data:image/jpeg") ? "image/jpeg" : file.type,
            size: image.size,
            resized: image.resized,
          });
        } catch (error) {
          const detail = error instanceof ImageResizeLimitError
            ? t("chat.imageContextTooSmallDetail", { context: contextLength ?? 0 })
            : error instanceof Error ? error.message : String(error);
          toast.error(t("chat.imageRejected"), detail);
        }
        continue;
      }
      if (
        contextLength != null
        && usedBytes + file.size > imageBudgetBytes(contextLength)
      ) {
        toast.error(
          t("chat.attachmentTooLarge"),
          t("chat.attachmentTooLargeDetail", {
            bytes: usedBytes + file.size,
            context: contextLength,
          }),
        );
        continue;
      }
      if (isTextFile(file)) {
        try {
          added.push({
            id: newId(),
            name: file.name,
            kind: "text",
            data: await file.text(),
            mediaType: file.type,
            size: file.size,
          });
        } catch (error) {
          toast.error(t("chat.fileRejected"), error instanceof Error ? error.message : String(error));
        }
        continue;
      }
      try {
        added.push({
          id: newId(),
          name: file.name,
          kind: "file",
          data: await file.text(),
          mediaType: file.type || "application/octet-stream",
          size: file.size,
        });
      } catch (error) {
        toast.error(t("chat.fileRejected"), error instanceof Error ? error.message : String(error));
      }
    }
    if (added.length > 0) setAttachments((current) => [...current, ...added]);
  }

  const speechSupported = speechRecognitionConstructor() != null;

  function stopDictation() {
    recognition.current?.stop();
    recognition.current = null;
    setListening(false);
  }

  function startDictation() {
    const Constructor = speechRecognitionConstructor();
    if (!Constructor) return;
    recognition.current?.stop();
    const instance = new Constructor();
    speechBaseDraft.current = draft.trim();
    speechTranscript.current = "";
    instance.lang = speechLanguage(locale);
    instance.continuous = true;
    instance.interimResults = true;
    instance.onresult = (event) => {
      let interim = "";
      for (let index = event.resultIndex; index < event.results.length; index += 1) {
        const result = event.results[index];
        const text = result?.[0]?.transcript ?? "";
        if (result?.isFinal) {
          speechTranscript.current = [speechTranscript.current, text].filter(Boolean).join(" ");
        } else {
          interim += text;
        }
      }
      const transcript = [speechTranscript.current, interim].filter(Boolean).join(" ");
      const next = [speechBaseDraft.current, transcript].filter(Boolean).join(" ");
      setDraft(next);
      window.requestAnimationFrame(grow);
    };
    instance.onerror = () => {
      recognition.current = null;
      setListening(false);
    };
    instance.onend = () => {
      recognition.current = null;
      setListening(false);
    };
    recognition.current = instance;
    setListening(true);
    instance.start();
  }

  function hasFiles(dataTransfer: DataTransfer): boolean {
    return Array.from(dataTransfer.types).includes("Files");
  }

  function handleDragEnter(event: React.DragEvent<HTMLDivElement>) {
    event.preventDefault();
    if (!hasFiles(event.dataTransfer)) return;
    dragDepth.current += 1;
    setDragActive(true);
  }

  function handleDragOver(event: React.DragEvent<HTMLDivElement>) {
    event.preventDefault();
    if (!hasFiles(event.dataTransfer)) return;
    event.dataTransfer.dropEffect = "copy";
    setDragActive(true);
  }

  function handleDragLeave(event: React.DragEvent<HTMLDivElement>) {
    event.preventDefault();
    dragDepth.current = Math.max(0, dragDepth.current - 1);
    if (dragDepth.current === 0) setDragActive(false);
  }

  function handleDrop(event: React.DragEvent<HTMLDivElement>) {
    event.preventDefault();
    dragDepth.current = 0;
    setDragActive(false);
    if (event.dataTransfer.files.length === 0) {
      toast.error(t("chat.fileRejected"), t("chat.dropRejectedDetail"));
      return;
    }
    void addFiles(event.dataTransfer.files);
  }

  async function send(options?: { visibleText?: string; requestContent?: string }) {
    const text = options?.visibleText ?? draft.trim();
    if ((!text && attachments.length === 0) || streaming || !model) return;
    if (listening) stopDictation();
    if (attachments.some((attachment) => attachment.kind === "image") && !visionAvailable) {
      toast.error(t("chat.imageRejected"), t("chat.imageVisionUnavailable"));
      return;
    }

    const requestParts: ChatRequestPart[] = [];
    if (text) requestParts.push({ type: "text", text });
    for (const attachment of attachments) {
      if (attachment.kind === "image") {
        requestParts.push({ type: "image_url", image_url: { url: attachment.data } });
      } else {
        requestParts.push({
          type: "text",
          text: t("chat.filePrompt", { name: attachment.name, content: attachment.data }),
        });
      }
    }
    const requestContent = options?.requestContent ?? (
      requestParts.length === 1 && requestParts[0].type === "text"
        ? requestParts[0].text
        : requestParts
    );
    const titleText = text || t("chat.attachments");

    const user: ChatMessage = {
      id: newId(),
      role: "user",
      content: text,
      attachments,
      requestContent,
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
          : titleFrom(titleText),
      thinkingEnabled,
      messages: [...history, reply],
    };
    setSession(live);
    setDraft("");
    setAttachments([]);
    setTelemetryCapture(null);
    autoFollow.current = true;
    window.requestAnimationFrame(grow);

    startStream({
      session: live,
      history,
      replyId: reply.id,
      model,
      // The answer's whole budget is the model's output capacity (`/v1/models`);
      // the deployment's `max_new_tokens` is only the size of one backend leg,
      // and as the total it cut a reasoning model off before it answered.
      maxTokens:
        models.data?.find((entry) => entry.id === model)?.max_tokens ?? node.data?.config?.max_new_tokens,
      inferenceHead: selectedInferenceHead ?? undefined,
      thinkingEnabled,
      ragEnabled: ragAvailable ? ragEnabled : undefined,
      ragCollection: ragAvailable && ragCollection ? ragCollection : undefined,
      emptyResponseMessage: t("chat.emptyResponse"),
      interruptedMessage: t("chat.interrupted"),
      cluster: cluster.data ?? null,
      onUnauthorized: () => toast.error(t("chat.keyRejected"), t("chat.keyRejectedDetail")),
    });
  }

  function askAboutTelemetry(question: ChatTelemetryQuestion) {
    void send(question);
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

  function deleteChatSession(id: string) {
    setTelemetrySnapshots((previous) => {
      if (!(id in previous)) return previous;
      const next = { ...previous };
      delete next[id];
      return next;
    });
    removeSession(id, model || null);
  }

  function startNewChat() {
    setDraft("");
    setAttachments([]);
    setTelemetryCapture(null);
    startNewSession(model || null);
  }

  const noModel = available.length === 0 && !node.data?.active_model && !cluster.data?.active_model;
  const canDeploy = canAccessRoute("/deploy", profile);
  const firstTraceModel = session.messages.find((message) => message.trace?.model)?.trace?.model;
  const modelAtStart = session.modelAtStart ?? firstTraceModel ?? session.model ?? model;

  /*
   * The composer is one element in two places: centred under the opening
   * screen while the conversation is empty, and at the bottom once it has
   * turns. The shared `layoutId` makes the first message carry it down.
   */
  const composerBox = (
    <motion.div layoutId="chat-composer" transition={{ duration: 0.45, ease: [0.22, 1, 0.36, 1] }} className="mx-auto w-full max-w-3xl">
            {attachments.length > 0 && (
              <div className="mb-2 flex flex-wrap gap-1.5">
                {attachments.map((attachment) => (
                  <span
                    key={attachment.id}
                    className="flex max-w-full items-center gap-2 rounded-lg border border-line bg-raised px-1.5 py-1.5 text-[0.65rem] text-muted"
                  >
                    {attachment.kind === "image" ? (
                      <img
                        src={attachment.data}
                        alt={t("chat.attachmentThumbnail", { name: attachment.name })}
                        className="h-10 w-10 shrink-0 rounded-md border border-line object-cover"
                      />
                    ) : (
                      <span
                        aria-label={t("chat.attachmentFormatAria", { format: attachmentFormat(attachment) })}
                        className="flex h-10 w-10 shrink-0 items-center justify-center rounded-md border border-aqua/30 bg-aqua/[0.08] px-1 text-center font-mono text-[0.58rem] font-semibold uppercase text-aqua"
                      >
                        {attachmentFormat(attachment)}
                      </span>
                    )}
                    <span className="max-w-[16rem] truncate" title={attachment.name}>{attachment.name}</span>
                    {attachment.resized && (
                      <span className="shrink-0 rounded border border-amber/25 bg-amber/[0.08] px-1 py-0.5 text-[0.62rem] text-amber" title={t("chat.attachmentResized")}>
                        {t("chat.attachmentResized")}
                      </span>
                    )}
                    <button
                      type="button"
                      aria-label={t("chat.removeAttachment", { name: attachment.name })}
                      onClick={() => setAttachments((current) => current.filter((item) => item.id !== attachment.id))}
                      className="rounded p-0.5 text-faint hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris"
                    >
                      <X className="h-3 w-3" />
                    </button>
                  </span>
                ))}
              </div>
            )}
            <div
              role="group"
              aria-label={t("chat.messageFieldAria")}
              onDragEnter={handleDragEnter}
              onDragOver={handleDragOver}
              onDragLeave={handleDragLeave}
              onDrop={handleDrop}
              className={clsx(
                "relative flex items-end gap-2 border bg-ink transition-colors duration-150 focus-within:border-iris/60 focus-within:ring-2 focus-within:ring-iris/20",
                // Alone on the opening screen the box is the whole page, so it is drawn larger.
                session.messages.length === 0 ? "rounded-2xl p-3 shadow-lift" : "rounded-xl p-2",
                dragActive ? "border-iris bg-iris/[0.08] ring-2 ring-iris/30" : "border-line",
              )}
            >
              {dragActive && (
                <div
                  aria-live="polite"
                  className="pointer-events-none absolute inset-1 z-10 flex items-center justify-center rounded-lg border border-dashed border-iris bg-ink/90 px-4 text-center text-sm font-medium text-iris shadow-lift"
                >
                  {t("chat.dropHint")}
                </div>
              )}
              <input
                ref={fileInput}
                type="file"
                multiple
                className="hidden"
                onChange={(event) => {
                  const files = event.currentTarget.files;
                  if (files) void addFiles(files);
                  event.currentTarget.value = "";
                }}
              />
              <div className="flex shrink-0 items-center gap-1">
                <button
                  type="button"
                  aria-label={t("chat.attach")}
                  title={t("chat.attach")}
                  onClick={() => fileInput.current?.click()}
                  className="rounded-md p-1.5 text-faint transition-colors hover:bg-raised hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris"
                >
                  <Paperclip className="h-4 w-4" />
                </button>
              </div>
              <textarea
                ref={composer}
                data-guide="chat-input"
                rows={1}
                value={draft}
                placeholder={t(promptPlaceholderKey(session.id))}
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
                className={clsx(
                  "max-h-[220px] flex-1 resize-none bg-transparent px-2 py-1.5 leading-relaxed text-fg placeholder:text-faint focus:outline-none",
                  session.messages.length === 0 ? "min-h-[5.5rem] text-base" : "min-h-[2.25rem] text-[0.9rem]",
                )}
              />
              {streaming ? (
                <Button
                  variant="outline"
                  icon={<Square className="h-3.5 w-3.5" />}
                  onClick={stopStream}
                >
                  {t("chat.stop")}
                </Button>
              ) : (
                <>
                  {thinkingAvailable && (
                    <Button
                      size="sm"
                      variant={thinkingEnabled ? "outline" : "ghost"}
                      aria-pressed={thinkingEnabled}
                      aria-label={t("chat.thinkToggle")}
                      title={t("chat.thinkToggleHint")}
                      onClick={() => {
                        const next = !thinkingEnabled;
                        const updated = { ...session, thinkingEnabled: next };
                        setSession(updated);
                        upsert(updated);
                      }}
                      className={clsx(
                        "shrink-0",
                        thinkingEnabled ? "border-iris/50 bg-iris/[0.08] text-iris" : "text-faint",
                      )}
                    >
                      {t("chat.thinkToggle")}
                    </Button>
                  )}
                  {speechSupported && (
                    <button
                      type="button"
                      aria-label={listening ? t("chat.dictationStop") : t("chat.dictationStart")}
                      title={listening ? t("chat.dictationStop") : t("chat.dictationStart")}
                      onClick={listening ? stopDictation : startDictation}
                      className={clsx(
                        "rounded-md p-1.5 transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris",
                        listening ? "bg-rose/15 text-rose" : "text-faint hover:bg-raised hover:text-fg",
                      )}
                    >
                      {listening ? <MicOff className="h-4 w-4" /> : <Mic className="h-4 w-4" />}
                    </button>
                  )}
                  <Button
                    variant="primary"
                    disabled={!draft.trim() && attachments.length === 0}
                    onClick={() => void send()}
                    aria-label={t("chat.send")}
                    className="w-9 px-0"
                  >
                    <ArrowUp className="h-4 w-4" />
                  </Button>
                </>
              )}
            </div>
            {/* Under the box, on the same baseline: what answers on the left,
                how to send on the right. */}
            <div className="mt-2 flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
              <div className="flex min-w-0 items-center gap-2">
                <ModelPicker value={model} options={available} onChange={chooseModel} />
                {ragAvailable && (
                  <RagMenu
                    enabled={ragEnabled}
                    collection={ragCollection}
                    collections={ragCollections}
                    onChange={(enabled, collection) => {
                      const updated = { ...session, ragEnabled: enabled, ragCollection: collection || undefined };
                      setSession(updated);
                      upsert(updated);
                    }}
                  />
                )}
              </div>
              <p className="text-[0.7rem] text-faint">{t("chat.composerHint")}</p>
            </div>
            {model && (
              <InferenceHeadChoice
                model={model}
                response={headQuery.data}
                loading={headQuery.isLoading}
                failed={headQuery.isError}
                selectedNodeId={selectedInferenceHead}
                onChange={(nodeId) => setHeadChoice({ model, nodeId })}
              />
            )}
    </motion.div>
  );
  const empty = session.messages.length === 0;
  const showHistory = hasConversations(sessions);
  // The composer remounts in its new place; the focus follows it there.
  const wasEmpty = useRef(empty);
  useEffect(() => {
    if (wasEmpty.current && !empty && document.activeElement === document.body) composer.current?.focus();
    wasEmpty.current = empty;
  }, [empty]);

  return (
    <div className="flex h-full min-h-0">
      {!historyLivesInMainMenu && showHistory && (
        <ChatSessionList
          sessions={sessions}
          activeId={session.id}
          onPick={selectSession}
          onNew={startNewChat}
          onDelete={deleteChatSession}
        />
      )}

      <div className="flex min-w-0 flex-1 flex-col">
        {/* An empty conversation has no title to show: the opening screen is the page. */}
        {!empty && (
        <div className="flex h-14 shrink-0 items-center justify-between gap-3 border-b border-line px-4 sm:px-6">
          <div className="flex min-w-0 flex-1 items-center gap-2">
            <TitleField
              // A conversation nobody has spoken in has no name to keep, so it
              // shows what the screen is instead of an empty title to rename.
              title={session.messages.length ? session.title : t("chat.title")}
              onRename={rename}
            />
            {chatCapabilities.data?.coverage === "incomplete" && (
              <Badge tone="amber">{t("chat.incomplete")}</Badge>
            )}
            {chatCapabilities.data?.coverage === "unavailable" && (
              <Badge tone="amber">{t("chat.coverageUnavailable")}</Badge>
            )}
          </div>
          <div className="flex shrink-0 items-center gap-2">
            {!historyLivesInMainMenu && showHistory && (
              <Button
                size="sm"
                variant="ghost"
                icon={<Plus className="h-3.5 w-3.5" />}
                onClick={startNewChat}
                className="md:hidden"
              >
                {t("chat.newShort")}
              </Button>
            )}
          </div>
        </div>
        )}

        {empty ? (
          <div className="flex min-h-0 flex-1 flex-col items-center justify-center overflow-y-auto px-4 py-8 sm:px-6" data-testid="chat-empty">
            {noModel ? (
              // Nothing to talk to: no composer, no RAG, only what is missing.
              <MissingPiece title={t("chat.noModel")} detail={t(canDeploy ? "chat.noModelDetail" : "chat.noModelAskAdmin")}>
                {canDeploy && (
                  <Link to="/deploy" className="mt-4 inline-flex items-center gap-2 rounded-full bg-iris px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-iris/85">
                    <Boxes className="h-4 w-4" />
                    {t("chat.noModelDeploy")}
                  </Link>
                )}
              </MissingPiece>
            ) : (
              <>
                <ChatHero />
                {composerBox}
              </>
            )}
          </div>
        ) : (
          <>
        <div ref={thread} onScroll={(event) => { autoFollow.current = isNearBottom(event.currentTarget); }} className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden">
          <div className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6">
              <div className="space-y-6">
                <AnimatePresence initial={false}>
                  {modelAtStart && <ModelIntroMark model={modelAtStart} />}
                  {session.messages.map((m) => {
                    const telemetry = telemetryCapture?.messageId === m.id
                      ? { ...telemetrySnapshots[m.id], ...telemetryCapture, environment: telemetryEnvironment }
                      : telemetrySnapshots[m.id];
                    return (
                      <Bubble
                        key={m.id}
                        message={m}
                        streaming={streaming && m.id === session.messages[session.messages.length - 1]?.id}
                        onAsk={askAboutTelemetry}
                        analysisDisabled={streaming || !model}
                        telemetryPlugin={showTelemetry ? telemetryPlugin : null}
                        telemetryTrace={telemetry?.trace}
                        telemetryTimings={telemetry?.timings}
                        telemetrySource={telemetry?.source}
                        telemetryEnvironment={telemetry?.environment}
                      />
                    );
                  })}
                </AnimatePresence>
                {/* A model that is thinking already shows it in its own line. */}
                {streaming &&
                  !session.messages[session.messages.length - 1]?.content &&
                  !session.messages[session.messages.length - 1]?.reasoning && (
                    <ChatWaitingLine asleep={chatCapabilities.data?.asleep === true} />
                  )}
              </div>
          </div>
        </div>

        <div className="shrink-0 border-t border-line bg-surface/80 px-4 py-3 backdrop-blur-md sm:px-6">
          {noModel ? (
            <p className="mx-auto w-full max-w-3xl text-center text-sm text-muted" data-testid="chat-no-model-bar">{t("chat.noModel")}</p>
          ) : composerBox}
        </div>
          </>
        )}
      </div>
    </div>
  );
}

/** Direct consumers such as focused route tests still get an isolated chat. */
export function Chat() {
  const controller = useOptionalChatSessionController();
  const ai = controller ? (
    <ChatScreen />
  ) : (
    <ChatSessionProvider>
      <ChatScreen />
    </ChatSessionProvider>
  );
  return (
    <div className="flex h-full min-h-0 flex-col gap-3">
      <div className="min-h-0 flex-1">{ai}</div>
    </div>
  );
}
