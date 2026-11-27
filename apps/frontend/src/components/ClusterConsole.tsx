/**
 * The cluster's process and engine lines as a terminal. One component for
 * the two places that show them: the Dashboard's console (the core's live
 * tail, `/api/ui/cluster-console`, no plugin) and the Deployment Logs screen
 * (the logs plugin's history). The caller brings the lines; this draws them,
 * follows the bottom and copies them.
 */
import { ArrowDownToLine, Check, Copy, Terminal } from "lucide-react";
import clsx from "clsx";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { isNearBottom } from "../lib/chatScroll";
import { copyText } from "../lib/clipboard";
import { clock } from "../lib/format";
import type { ClusterLogEntry } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { Skeleton } from "./ui";

export function logLineText(entry: ClusterLogEntry) {
  return `${entry.node_name || entry.node_id} [${entry.origin_at}] ${entry.level.toLowerCase()} ${entry.message}`;
}

const levelTone: Record<string, string> = {
  error: "text-rose-300",
  warn: "text-amber-300",
};

export function ClusterConsole({
  entries,
  loading = false,
  error = false,
  notice,
  empty,
  compact = false,
  testId = "cluster-console-terminal",
}: {
  entries: ClusterLogEntry[];
  loading?: boolean;
  error?: boolean;
  /** A line above the log (collection off, lines dropped). */
  notice?: ReactNode;
  /** What an empty log says; the caller knows why it is empty. */
  empty: { title: string; hint?: string };
  compact?: boolean;
  testId?: string;
}) {
  const { t } = useI18n();
  const terminalRef = useRef<HTMLDivElement>(null);
  const wasAtBottom = useRef(true);
  const [autoScroll, setAutoScroll] = useState(true);
  const [copyState, setCopyState] = useState<"idle" | "copied" | "failed">("idle");
  const latestSequence = entries[entries.length - 1]?.received_seq ?? null;

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal || !autoScroll || !wasAtBottom.current || entries.length === 0) return;
    terminal.scrollTo({ top: terminal.scrollHeight, behavior: "auto" });
  }, [autoScroll, entries.length, latestSequence]);

  function onScroll() {
    const terminal = terminalRef.current;
    if (!terminal) return;
    const atBottom = isNearBottom({
      scrollTop: terminal.scrollTop,
      clientHeight: terminal.clientHeight,
      scrollHeight: terminal.scrollHeight,
    });
    wasAtBottom.current = atBottom;
    if (!atBottom) setAutoScroll(false);
  }

  function toggleAutoScroll() {
    const next = !autoScroll;
    setAutoScroll(next);
    if (next) {
      wasAtBottom.current = true;
      terminalRef.current?.scrollTo({ top: terminalRef.current.scrollHeight, behavior: "auto" });
    }
  }

  async function copyLogs() {
    const copied = await copyText(entries.slice(-500).map(logLineText).join("\n"));
    setCopyState(copied ? "copied" : "failed");
  }

  const button =
    "inline-flex items-center gap-1.5 rounded-md border border-white/20 bg-white/10 font-medium text-slate-200 transition-colors hover:border-white/35 hover:bg-white/20 hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-aqua/70";
  const size = compact ? "h-7 px-2 text-[0.68rem]" : "h-8 px-2.5 text-xs";

  return (
    <div className="relative min-h-0 flex-1 overflow-hidden bg-black">
      <div className={clsx("absolute right-3 z-10 flex items-center gap-2", compact ? "top-2" : "top-3")}>
        <button
          type="button"
          aria-label={autoScroll ? t("deploymentLogs.disableAutoScroll") : t("deploymentLogs.enableAutoScroll")}
          aria-pressed={autoScroll}
          title={autoScroll ? t("deploymentLogs.disableAutoScroll") : t("deploymentLogs.enableAutoScroll")}
          onClick={toggleAutoScroll}
          className={clsx(button, size)}
        >
          <ArrowDownToLine className="h-3.5 w-3.5" aria-hidden="true" />
          {autoScroll ? t("deploymentLogs.autoScrollOn") : t("deploymentLogs.autoScrollOff")}
        </button>
        <button
          type="button"
          aria-label={t("deploymentLogs.copyLogs")}
          title={t("deploymentLogs.copyLogs")}
          onClick={() => void copyLogs()}
          className={clsx(button, size)}
        >
          {copyState === "copied" ? (
            <Check className="h-3.5 w-3.5 text-aqua" aria-hidden="true" />
          ) : (
            <Copy className="h-3.5 w-3.5" aria-hidden="true" />
          )}
          {copyState === "copied" ? t("deploymentLogs.logsCopied") : t("deploymentLogs.copyLogs")}
        </button>
      </div>
      <div
        ref={terminalRef}
        data-testid={testId}
        onScroll={onScroll}
        className={clsx(
          "h-full overflow-auto font-mono text-emerald-100",
          compact ? "p-3 pt-11 text-[0.7rem] leading-5" : "p-3 pt-14 text-[0.76rem] leading-6 sm:p-4 sm:pt-14",
        )}
        role="log"
        aria-live="polite"
      >
        {notice && <div className="mb-2 text-amber-300">{notice}</div>}
        {loading && <Skeleton className="h-12 bg-white/10" />}
        {!loading && !error && entries.length === 0 && (
          <div className="flex flex-col items-center justify-center px-6 py-10 text-center text-slate-200">
            <div className="mb-3 flex h-10 w-10 items-center justify-center rounded-xl border border-white/15 bg-white/10 text-slate-400">
              <Terminal className="h-5 w-5" aria-hidden="true" />
            </div>
            <p className="font-display text-[0.95rem] font-semibold text-slate-100">{empty.title}</p>
            {empty.hint && <p className="mt-1.5 max-w-sm text-sm leading-relaxed text-slate-400">{empty.hint}</p>}
          </div>
        )}
        {entries.map((entry) => (
          <div
            key={entry.received_seq}
            data-testid="deployment-log-line"
            className="whitespace-pre-wrap break-words rounded px-2 py-0.5 hover:bg-white/[0.045]"
            title={entry.origin_at}
          >
            <span className="mr-2 text-cyan-300">{entry.node_name || entry.node_id}</span>
            <span className="mr-2 text-slate-500">[{clock(entry.origin_at)}]</span>
            <span className={clsx("mr-2", levelTone[entry.level.toLowerCase()] ?? "text-slate-600")}>
              {entry.level.toLowerCase()}
            </span>
            <span>{entry.message}</span>
          </div>
        ))}
      </div>
      {copyState === "failed" && (
        <p className="absolute bottom-2 right-3 text-xs text-rose" role="status">
          {t("deploymentLogs.copyFailed")}
        </p>
      )}
      {copyState === "copied" && (
        <p className="sr-only" role="status">
          {t("deploymentLogs.logsCopied")}
        </p>
      )}
    </div>
  );
}
