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
  Mic,
  MicOff,
  MessagesSquare,
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
import { ExecutionTelemetry, TimingSummary, type ExecutionTelemetryEnvironment } from "../components/ExecutionTelemetry";
import { InferenceHeadChoice } from "../components/InferenceHeadChoice";
import { Badge, Button, Empty } from "../components/ui";
import { isNearBottom } from "../lib/chatScroll";
import {
  buildTelemetryAnalysisPrompt,
} from "../lib/telemetryAnalysis";
import {
  ChatSessionProvider,
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
import { resizeImageForVision } from "../lib/imageResize";
import { ms, num } from "../lib/format";
import type { RequestTrace } from "../lib/chatStream";
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

export { ExecutionTelemetry } from "../components/ExecutionTelemetry";
export type { ExecutionTelemetryEnvironment } from "../components/ExecutionTelemetry";
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

export function Bubble({
  message,
  streaming,
  onAnalyze,
  analysisDisabled = false,
  telemetryEnvironment,
  telemetryTrace,
}: {
  message: ChatMessage;
  streaming: boolean;
  onAnalyze?: (trace: RequestTrace) => void;
  analysisDisabled?: boolean;
  telemetryEnvironment?: ExecutionTelemetryEnvironment;
  /** The final stream capture can arrive one render before the transcript patch. */
  telemetryTrace?: RequestTrace;
}) {
  const { t } = useI18n();
  const { profile } = useProfile();
  const showTelemetry = profile !== "infer";
  const trace = message.trace ?? telemetryTrace;
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
              <figcaption className="mt-1.5 truncate text-[0.7rem] text-faint" title={attachment.name}>
                {attachment.name}
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
            <details open={streaming} className="mb-3 rounded-xl border border-iris/25 bg-iris/[0.05] px-3 py-2.5">
              <summary className="cursor-pointer select-none text-xs font-medium text-iris">{t("chat.thinking")}</summary>
              <div className="mt-2 text-sm text-muted"><Markdown>{message.reasoning}</Markdown></div>
            </details>
          )}
          {message.content && <Markdown>{message.content}</Markdown>}
          {message.error && (
            <ChatError message={message.error} />
          )}
          {showTelemetry && message.source && (
            <p className="mt-2 font-mono text-[0.68rem] text-faint">{message.source}</p>
          )}
          {showTelemetry && message.timings ? (
            <TimingSummary timings={message.timings} />
          ) : showTelemetry && message.tokensPerSecond != null && (
            <div className="mt-2 flex flex-wrap items-center gap-2 font-mono text-[0.68rem] text-faint">
              <span>{t("chat.legacyTotalSpeed", { value: num(message.tokensPerSecond, 1) })}{message.tokensGenerated != null && ` · ${message.tokensGenerated} ${t("chat.tokensUnit")}`}{message.durationMs != null && ` · ${ms(message.durationMs)}`}</span>
            </div>
          )}
          {showTelemetry && trace && (
            <div className="mt-2">
              <ExecutionTelemetry
                trace={trace}
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
  const chatCapabilities = useChatCapabilities();
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
  const [telemetryCapture, setTelemetryCapture] = useState<{
    messageId: string;
    trace: RequestTrace;
    cluster: ClusterView | null;
  } | null>(null);
  const [telemetrySnapshots, setTelemetrySnapshots] = useState<Record<string, ExecutionTelemetryEnvironment>>({});
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
  const telemetryNetwork = useChatTelemetryNetwork(telemetryNodeIds, showTelemetry && telemetryCapture != null, telemetryCapture?.messageId);
  const runtimeQueries = usePeerRuntimes(telemetryNodeIds, showTelemetry && telemetryCapture != null, telemetryCapture?.messageId);
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
  const model = session.model && available.includes(session.model)
    ? session.model
    : available[0] ?? node.data?.active_model ?? cluster.data?.active_model ?? "";
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

  useEffect(() => {
    if (!showTelemetry || lastStreamResult?.sessionId !== session.id) return;
    setTelemetryCapture({
      messageId: lastStreamResult.messageId,
      trace: lastStreamResult.trace,
      cluster: lastStreamResult.cluster,
    });
  }, [lastStreamResult, session.id, showTelemetry]);

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
  const servedModel = models.data?.find((candidate) => candidate.id === model);
  const contextLength = servedModel?.context_length
    ?? servedModel?.max_input_tokens
    ?? (node.data?.active_model === model ? node.data?.config?.n_ctx : undefined);

  async function addFiles(files: FileList | File[]) {
    const selectedFiles = Array.from(files);
    const contextBytes = contextLength != null ? contextLength * 4 : undefined;
    const currentBytes = attachments.reduce((total, attachment) => total + estimatedAttachmentBytes(attachment), 0);
    const selectedBytes = selectedFiles.reduce((total, file) => total + file.size, 0);
    if (contextBytes != null && currentBytes + selectedBytes > contextBytes) {
      toast.error(
        t("chat.attachmentTooLarge"),
        t("chat.attachmentTooLargeDetail", {
          bytes: currentBytes + selectedBytes,
          context: contextLength ?? 0,
        }),
      );
      return;
    }

    const added: ChatAttachment[] = [];
    for (const file of selectedFiles) {
      if (file.type.startsWith("image/")) {
        if (!visionAvailable) {
          toast.error(t("chat.imageRejected"), t("chat.imageVisionUnavailable"));
          continue;
        }
        try {
          added.push({
            id: newId(),
            name: file.name,
            kind: "image",
            data: await resizeImageForVision(file),
            mediaType: file.type,
            size: file.size,
          });
        } catch (error) {
          toast.error(t("chat.imageRejected"), error instanceof Error ? error.message : String(error));
        }
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
      maxTokens: node.data?.config?.max_new_tokens,
      inferenceHead: selectedInferenceHead ?? undefined,
      thinkingEnabled,
      emptyResponseMessage: t("chat.emptyResponse"),
      interruptedMessage: t("chat.interrupted"),
      cluster: cluster.data ?? null,
      onUnauthorized: () => toast.error(t("chat.keyRejected"), t("chat.keyRejectedDetail")),
    });
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
  const firstTraceModel = session.messages.find((message) => message.trace?.model)?.trace?.model;
  const modelAtStart = session.modelAtStart ?? firstTraceModel ?? session.model ?? model;

  return (
    <div className="flex h-full min-h-0">
      {!historyLivesInMainMenu && (
        <ChatSessionList
          sessions={sessions}
          activeId={session.id}
          onPick={selectSession}
          onNew={startNewChat}
          onDelete={deleteChatSession}
        />
      )}

      <div className="flex min-w-0 flex-1 flex-col">
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
            {!historyLivesInMainMenu && (
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
                      telemetryTrace={telemetryCapture?.messageId === m.id ? telemetryCapture.trace : undefined}
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
                        aria-label={`Formato ${attachmentFormat(attachment)}`}
                        className="flex h-10 w-10 shrink-0 items-center justify-center rounded-md border border-aqua/30 bg-aqua/[0.08] px-1 text-center font-mono text-[0.58rem] font-semibold uppercase text-aqua"
                      >
                        {attachmentFormat(attachment)}
                      </span>
                    )}
                    <span className="max-w-[16rem] truncate" title={attachment.name}>{attachment.name}</span>
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
              aria-label="Campo de mensagem"
              onDragEnter={handleDragEnter}
              onDragOver={handleDragOver}
              onDragLeave={handleDragLeave}
              onDrop={handleDrop}
              className={clsx(
                "relative flex items-end gap-2 rounded-xl border bg-ink p-2 transition-colors duration-150 focus-within:border-iris/60 focus-within:ring-2 focus-within:ring-iris/20",
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
              <ModelPicker value={model} options={available} onChange={chooseModel} />
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
          </div>
        </div>
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
