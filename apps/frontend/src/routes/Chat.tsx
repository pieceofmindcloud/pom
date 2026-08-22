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
  ChevronDown,
  MessagesSquare,
  Pencil,
  Plus,
  Square,
  Sparkles,
  Trash2,
  TriangleAlert,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Markdown } from "../components/Markdown";
import { Badge, Button, Empty } from "../components/ui";
import { ApiError, streamSse } from "../lib/api";
import { isNearBottom } from "../lib/chatScroll";
import { consumeChatDelta, emptyChatStream, finishChatStream } from "../lib/chatStream";
import { analyzeTelemetry, buildTelemetryAnalysisPrompt } from "../lib/telemetryAnalysis";
import {
  emptySession,
  newId,
  titleFrom,
  turnsOnly,
  useChatSessions,
  type ChatMessage,
  type ChatSession,
} from "../lib/chatStore";
import { bytes, since } from "../lib/format";
import { isPhasedHop } from "../lib/chatStream";
import type { LegacyHopTrace, PhasedHopTrace, PhaseTrace, RequestTrace } from "../lib/chatStream";
import { useCluster, useNode, useServableModels } from "../lib/queries";
import { chatModelOptions } from "../lib/chatModels";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import { localizeServerText } from "../i18n/locales";

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

function Bubble({
  message,
  streaming,
  onAnalyze,
  analysisDisabled = false,
}: {
  message: ChatMessage;
  streaming: boolean;
  onAnalyze?: (trace: RequestTrace) => void;
  analysisDisabled?: boolean;
}) {
  const { t, locale } = useI18n();
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
              <span>{message.tokensPerSecond.toFixed(1)} {t("chat.speedUnit")}{message.tokensGenerated != null && ` · ${message.tokensGenerated} ${t("chat.tokensUnit")}`}{message.durationMs != null && ` · ${(message.durationMs / 1000).toFixed(1)} ${t("chat.timeUnit")}`}</span>
              {message.trace && (
                <ExecutionTelemetry
                  trace={message.trace}
                  source={message.source}
                  onAnalyze={onAnalyze}
                  analyzeDisabled={analysisDisabled || streaming}
                />
              )}
            </div>
          )}
        </div>
      )}
    </motion.div>
  );
}

/**
 * Closed by default: it is four rows of numbers per hop, and the answer above
 * it is what the operator came for. Opening it is one click; scrolling past it
 * on every single turn was not.
 */
export function ExecutionTelemetry({
  trace,
  source,
  onAnalyze,
  analyzeDisabled = false,
}: {
  trace: RequestTrace;
  source?: string;
  onAnalyze?: (trace: RequestTrace) => void;
  analyzeDisabled?: boolean;
}) {
  const { t } = useI18n();
  const analysis = analyzeTelemetry(trace);
  const tpRows = trace.hops.flatMap((hop) => {
    if (!isPhasedHop(hop)) return [];
    return [
      { hop, label: t("chat.telemetry.prefill"), phase: hop.prefill },
      { hop, label: t("chat.telemetry.decode"), phase: hop.decode },
    ].filter((row) => row.phase.tp?.enabled);
  });

  return <details className="w-full rounded-lg border border-line bg-raised px-3 py-2 text-[0.72rem] text-muted">
    <summary className="cursor-pointer font-medium text-fg">{t("chat.telemetry.title")} {source ? `· ${source}` : ""}</summary>
    <div className="mt-2 flex flex-wrap gap-x-3 gap-y-1 font-mono"><span>{t("chat.telemetry.tokensIn", { count: trace.input_tokens })}</span><span>{t("chat.telemetry.tokensOut", { count: trace.output_tokens })}</span><span>{t("chat.telemetry.prefillMs", { ms: trace.prefill_ms })}</span><span>{bytes(trace.total_bytes_sent + trace.total_bytes_received)}</span></div>
    <div className={clsx("mt-3 rounded-lg border px-3 py-2.5", analysis.severity === "critical" ? "border-rose/35 bg-rose/[0.06]" : analysis.severity === "warning" ? "border-amber/35 bg-amber/[0.06]" : "border-line/70 bg-surface/40")}>
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <p className="text-[0.68rem] font-semibold uppercase tracking-[0.12em] text-faint">{t("chat.telemetry.analysis.label")}</p>
          <p className="mt-1 font-medium text-fg">{t(analysis.titleKey)}</p>
          <p className="mt-1 max-w-prose leading-relaxed text-muted">{t(analysis.detailKey)}</p>
          <p className="mt-1 font-mono text-[0.68rem] text-faint">{analysis.evidence}</p>
          <p className="mt-1 text-[0.68rem] text-faint">{t(analysis.nextStepKey)}</p>
        </div>
        {onAnalyze && (
          <Button
            size="sm"
            variant="outline"
            icon={<Sparkles className="h-3.5 w-3.5" />}
            disabled={analyzeDisabled}
            onClick={() => onAnalyze(trace)}
          >
            {t("chat.telemetry.analyze")}
          </Button>
        )}
      </div>
    </div>
    <div className="mt-2 overflow-x-auto">
      <table className="w-full min-w-[46rem] border-collapse font-mono text-[0.66rem] leading-relaxed">
        <thead className="text-faint">
          <tr className="border-b border-line/70 text-left">
            <th className="py-1 pr-3 font-normal">{t("chat.telemetry.hop")}</th>
            <th className="py-1 pr-3 font-normal">{t("chat.telemetry.phase")}</th>
            <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.calls")}</th>
            <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.gatewayHeader")}</th>
            <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.backendHeader")}</th>
            <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.residualHeader")}</th>
            <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.samplingHeader")}</th>
            <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.transportHeader")}</th>
            <th className="py-1 text-right font-normal">{t("chat.telemetry.bytesHeader")}</th>
          </tr>
        </thead>
        <tbody>
          {trace.hops.flatMap((hop) => {
            const id = `${hop.node_id}:${hop.start_layer}`;
            if (!isPhasedHop(hop)) return [<LegacyHopRows key={id} hop={hop} />];
            return [
              <PhaseRow key={`${id}:prefill`} hop={hop} label={t("chat.telemetry.prefill")} phase={hop.prefill} first />,
              <PhaseRow key={`${id}:decode`} hop={hop} label={t("chat.telemetry.decode")} phase={hop.decode} />,
            ];
          })}
        </tbody>
      </table>
    </div>
    {tpRows.length > 0 && <TpTable rows={tpRows} />}
    <p className="mt-2 text-faint">{t("chat.telemetry.footnote")}</p>
  </details>;
}

function TpTable({ rows }: { rows: Array<{ hop: PhasedHopTrace; label: string; phase: PhaseTrace }> }) {
  const { t } = useI18n();
  return <div className="mt-3 overflow-x-auto rounded-lg border border-line/60 bg-surface/30 px-2.5 py-2">
    <p className="mb-1.5 font-medium text-fg">{t("chat.telemetry.tpTitle")}</p>
    <table className="w-full min-w-[54rem] border-collapse font-mono text-[0.66rem] leading-relaxed">
      <thead className="text-faint"><tr className="border-b border-line/70 text-left">
        <th className="py-1 pr-3 font-normal">{t("chat.telemetry.hop")}</th>
        <th className="py-1 pr-3 font-normal">{t("chat.telemetry.phase")}</th>
        <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.tpReduces")}</th>
        <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.tpWait")}</th>
        <th className="py-1 pr-3 text-right font-normal">p50 / p95</th>
        <th className="py-1 pr-3 text-right font-normal">{t("chat.telemetry.tpBytes")}</th>
        <th className="py-1 text-right font-normal">{t("chat.telemetry.tpChannel")}</th>
      </tr></thead>
      <tbody>{rows.map(({ hop, label, phase }) => {
        const tp = phase.tp;
        if (!tp) return null;
        const channel = tp.tunnel_calls > 0
          ? t("chat.telemetry.tpTunnel", { calls: tp.tunnel_calls, fallback: tp.fallback_calls })
          : t("chat.telemetry.tpDirect", { calls: tp.direct_calls });
        return <tr key={`${hop.node_id}:${hop.start_layer}:${label}`} className="border-t border-line/40">
          <td className="py-1 pr-3 text-fg">L{hop.start_layer}-L{hop.end_layer}</td>
          <td className="py-1 pr-3">{label}</td>
          <td className="py-1 pr-3 text-right">{tp.reduces} / {tp.expected_reduces}</td>
          <td className="py-1 pr-3 text-right">{tp.wait_ms} ms</td>
          <td className="py-1 pr-3 text-right">{tp.reduce_p50_ms} / {tp.reduce_p95_ms} ms</td>
          <td className="py-1 pr-3 text-right">{bytes(tp.request_bytes + tp.response_bytes)} + {bytes(tp.broadcast_bytes)} {t("chat.telemetry.tpBroadcastShort")}</td>
          <td className="py-1 text-right">{channel}</td>
        </tr>;
      })}</tbody>
    </table>
  </div>;
}

/**
 * A linha que descreve o canal deste salto.
 *
 * Ela existe para dizer duas coisas que um número sozinho não diz. A primeira é
 * se o salto foi cifrado - e, quando só parte das chamadas foi, ela mostra a
 * fração, porque é assim que uma virada de política pela metade aparece em vez
 * de passar despercebida.
 *
 * A segunda é o que `tls_handshake_ms` mede. O handshake acontece **uma vez por
 * conexão**, e as conexões ficam no pool: quase todo salto reporta zero. Sem
 * dizer isso com todas as letras, quem olhasse um salto zerado concluiria que
 * TLS é grátis, e quem olhasse o salto que abriu conexão concluiria que é caro
 * por token - duas leituras erradas do mesmo número. O custo por requisição
 * depois do handshake é a cifra do record layer, que está diluída dentro de
 * `response_body_ms` e não é separável sem instrumentar o socket; a tela diz
 * isso em vez de inventar uma coluna.
 */
function tlsLine(
  t: ReturnType<typeof useI18n>["t"],
  phase: { calls: number; tls_calls: number; tls_handshake_ms: number },
) {
  if (phase.calls === 0) return t("chat.telemetry.tlsIdle");
  if (phase.tls_calls === 0) return t("chat.telemetry.tlsOff");
  if (phase.tls_calls < phase.calls) {
    return t("chat.telemetry.tlsPartial", {
      cifradas: phase.tls_calls,
      total: phase.calls,
      handshake: phase.tls_handshake_ms,
    });
  }
  return t("chat.telemetry.tlsOn", { handshake: phase.tls_handshake_ms });
}

function PhaseRow({
  hop,
  label,
  phase,
  first,
}: {
  hop: PhasedHopTrace;
  label: string;
  phase: PhaseTrace;
  first?: boolean;
}) {
  const { t } = useI18n();
  const gatewayMs = phase.client_encode_ms + phase.client_decode_ms;
  const named =
    phase.node_frame_decode_ms + phase.node_input_prepare_ms + phase.node_queue_ms +
    phase.node_lock_wait_ms +
    phase.node_setup_ms + phase.node_decode_ms + phase.node_residual_ms +
    phase.node_sampling_ms + phase.node_encode_ms;
  const unattributed = Math.max(0, phase.processing_ms - named);
  const idle = phase.calls === 0;

  return <>
    <tr className={clsx("border-t border-line/40", first && "border-t-line/70", idle && "text-faint")}>
      {first
        ? <td className="py-1 pr-3 align-top text-fg" rowSpan={4}>L{hop.start_layer}-L{hop.end_layer}<br /><span className="text-faint">{hop.node_id.slice(0, 10)}</span></td>
        : null}
      <td className="py-1 pr-3">{label}</td>
      <td className="py-1 pr-3 text-right">{phase.calls}</td>
      <td className="py-1 pr-3 text-right">{gatewayMs} ms</td>
      <td className="py-1 pr-3 text-right">{phase.node_decode_ms} ms</td>
      <td className="py-1 pr-3 text-right">{phase.node_residual_ms} ms</td>
      <td className="py-1 pr-3 text-right">{phase.node_sampling_ms} ms</td>
      <td className="py-1 pr-3 text-right">{phase.network_ms} ms</td>
      <td className="py-1 text-right">{bytes(phase.bytes_sent + phase.bytes_received)}</td>
    </tr>
    <tr className="text-faint">
      <td className="pb-1.5 pr-3 align-top" colSpan={8}>
        <span className="block">{t("chat.telemetry.nodeRow", { frame: phase.node_frame_decode_ms, prepare: phase.node_input_prepare_ms, queue: phase.node_queue_ms, engineWait: phase.node_lock_wait_ms, setup: phase.node_setup_ms, frameOut: phase.node_encode_ms })}{unattributed > 0 ? ` · ${t("chat.telemetry.unattributed", { ms: unattributed })}` : ""}</span>
        <span className="block">{t("chat.telemetry.backendRow", { enqueue: phase.node_backend_enqueue_ms, completion: phase.node_backend_completion_ms, d2hCapture: phase.node_residual_capture_d2h_sync_ms, p50: phase.node_p50_ms, p95: phase.node_p95_ms })}{phase.node_backend_completion_ms === 0 ? ` ${t("chat.telemetry.syncOff")}` : ""}</span>
        <span className="block">{t("chat.telemetry.samplingRow", { access: phase.node_logits_access_ms, clone: phase.node_logits_clone_ms, penalty: phase.node_repetition_penalty_ms, softmax: phase.node_softmax_ms, topK: phase.node_top_k_ms, topP: phase.node_top_p_ms, selection: phase.node_selection_ms, text: phase.node_token_text_ms })}</span>
        <span className="block">{t("chat.telemetry.gatewayRow", { encode: phase.client_encode_ms, headers: phase.response_headers_ms, body: phase.response_body_ms, decode: phase.client_decode_ms })}</span>
        <span className="block">{tlsLine(t, phase)}</span>
      </td>
    </tr>
  </>;
}

/**
 * A hop recorded before prefill and decode were split apart.
 *
 * Its timings and bytes were already summed across both phases when they
 * reached us, so there is nothing to split. Printing the same total on a
 * "prefill" row and again on a "decode" row would put numbers under labels they
 * do not belong to - the row says the split is missing instead.
 */
function LegacyHopRows({ hop }: { hop: LegacyHopTrace }) {
  const { t } = useI18n();
  const gatewayMs = hop.client_encode_ms + hop.client_decode_ms;
  const named =
    hop.node_frame_decode_ms + hop.node_input_prepare_ms + hop.node_queue_ms +
    hop.node_setup_ms + hop.node_decode_ms + hop.node_residual_ms +
    hop.node_sampling_ms + hop.node_encode_ms;
  const unattributed = Math.max(0, hop.prefill_ms + hop.decode_ms - named);

  return <>
    <tr className="border-t border-line/70">
      <td className="py-1 pr-3 align-top text-fg" rowSpan={2}>L{hop.start_layer}-L{hop.end_layer}<br /><span className="text-faint">{hop.node_id.slice(0, 10)}</span></td>
      <td className="py-1 pr-3">{t("chat.telemetry.legacyBoth")}</td>
      <td className="py-1 pr-3 text-right">{hop.calls}</td>
      <td className="py-1 pr-3 text-right">{gatewayMs} ms</td>
      <td className="py-1 pr-3 text-right">{hop.node_decode_ms} ms</td>
      <td className="py-1 pr-3 text-right">{hop.node_residual_ms} ms</td>
      <td className="py-1 pr-3 text-right">{hop.node_sampling_ms} ms</td>
      <td className="py-1 pr-3 text-right">{hop.network_ms} ms</td>
      <td className="py-1 text-right">{bytes(hop.bytes_sent + hop.bytes_received)}</td>
    </tr>
    <tr className="text-faint">
      <td className="pb-1.5 pr-3 align-top" colSpan={8}>
        <span className="block">{t("chat.telemetry.legacyNote", { prefillCalls: hop.prefill_calls, decodeCalls: hop.decode_calls })}</span>
        <span className="block">{t("chat.telemetry.legacyNodeRow", { frame: hop.node_frame_decode_ms, prepare: hop.node_input_prepare_ms, queue: hop.node_queue_ms, setup: hop.node_setup_ms, frameOut: hop.node_encode_ms, unattributed })}</span>
        <span className="block">{t("chat.telemetry.legacyDecodeRow", { decodeP50: hop.node_decode_p50_ms, decodeP95: hop.node_decode_p95_ms, encode: hop.client_encode_ms, headers: hop.response_headers_ms, body: hop.response_body_ms, decode: hop.client_decode_ms })}</span>
      </td>
    </tr>
  </>;
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
  }

  function deleteSession(id: string) {
    remove(id);
    if (id === session.id) setSession(emptySession(model || null));
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
