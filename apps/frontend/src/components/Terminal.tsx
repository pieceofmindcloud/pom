/**
 * The panel's terminal: lines of a log on a black ground, following the
 * bottom while the reader stays there, with a follow switch and a copy
 * button. One component for every log the panel or a plugin shows: the
 * cluster console, the engines' logs on the Logs screen, and plugin screens,
 * which get it from the host as `components.Terminal` (host version 9,
 * `lib/pluginHost.ts`) instead of drawing their own.
 *
 * A line is a plain string or a structured record (source, time, level,
 * text). The caller brings the lines and says what an empty log means.
 */
import { ArrowDownToLine, Check, Copy, Terminal as TerminalIcon } from "lucide-react";
import clsx from "clsx";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { isNearBottom } from "../lib/chatScroll";
import { copyText } from "../lib/clipboard";
import { clock } from "../lib/format";
import { useI18n } from "../i18n/provider";
import { Skeleton } from "./ui";

export type TerminalRecord = {
  /** Stable key for the row; the index is used when absent. */
  key?: string | number;
  /** Who wrote it (a machine name), drawn first in cyan. */
  source?: string;
  /** ISO time; drawn as a clock and kept whole in the row's title. */
  time?: string;
  /** `error` and `warn` are toned; any other word is drawn faint. */
  level?: string;
  text: string;
};

export type TerminalLine = string | TerminalRecord;

const LEVEL_TONE: Record<string, string> = {
  error: "text-rose-300",
  warn: "text-amber-300",
  warning: "text-amber-300",
};

function record(line: TerminalLine): TerminalRecord {
  return typeof line === "string" ? { text: line } : line;
}

/** The line as copied text: `source [time] level text`. */
export function terminalLineText(line: TerminalLine): string {
  const r = record(line);
  return [r.source, r.time ? `[${r.time}]` : null, r.level?.toLowerCase(), r.text].filter(Boolean).join(" ");
}

export type TerminalProps = {
  lines: TerminalLine[];
  loading?: boolean;
  error?: boolean;
  /** A line above the log (collection off, lines dropped). */
  notice?: ReactNode;
  /** What an empty log says; the caller knows why it is empty. */
  empty?: { title: string; hint?: string };
  /** Smaller type and buttons, for a dock or a card. */
  compact?: boolean;
  /** How many of the last lines Copy takes (default 500). */
  copyLimit?: number;
  className?: string;
  testId?: string;
};

export function Terminal({
  lines,
  loading = false,
  error = false,
  notice,
  empty,
  compact = false,
  copyLimit = 500,
  className,
  testId = "terminal",
}: TerminalProps) {
  const { t } = useI18n();
  const box = useRef<HTMLDivElement>(null);
  const wasAtBottom = useRef(true);
  const [follow, setFollow] = useState(true);
  const [copyState, setCopyState] = useState<"idle" | "copied" | "failed">("idle");
  const last = lines[lines.length - 1];
  const lastKey = last == null ? null : typeof last === "string" ? last : (last.key ?? last.text);

  useEffect(() => {
    const el = box.current;
    if (!el || !follow || !wasAtBottom.current || lines.length === 0) return;
    el.scrollTop = el.scrollHeight;
  }, [follow, lines.length, lastKey]);

  function onScroll() {
    const el = box.current;
    if (!el) return;
    const atBottom = isNearBottom({ scrollTop: el.scrollTop, clientHeight: el.clientHeight, scrollHeight: el.scrollHeight });
    wasAtBottom.current = atBottom;
    if (!atBottom) setFollow(false);
  }

  function toggleFollow() {
    const next = !follow;
    setFollow(next);
    if (next) {
      wasAtBottom.current = true;
      if (box.current) box.current.scrollTop = box.current.scrollHeight;
    }
  }

  async function copy() {
    const copied = await copyText(lines.slice(-copyLimit).map(terminalLineText).join("\n"));
    setCopyState(copied ? "copied" : "failed");
    window.setTimeout(() => setCopyState("idle"), 1500);
  }

  const button =
    "inline-flex items-center gap-1.5 rounded-md border border-white/20 bg-white/10 font-medium text-slate-200 transition-colors hover:border-white/35 hover:bg-white/20 hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-aqua/70";
  const size = compact ? "h-7 px-2 text-[0.68rem]" : "h-8 px-2.5 text-xs";
  const followLabel = follow ? t("deploymentLogs.disableAutoScroll") : t("deploymentLogs.enableAutoScroll");

  return (
    <div className={clsx("relative min-h-0 flex-1 overflow-hidden bg-black", className)}>
      <div className={clsx("absolute right-3 z-10 flex items-center gap-2", compact ? "top-2" : "top-3")}>
        <button type="button" aria-label={followLabel} aria-pressed={follow} title={followLabel} onClick={toggleFollow} className={clsx(button, size)}>
          <ArrowDownToLine className="h-3.5 w-3.5" aria-hidden="true" />
          {follow ? t("deploymentLogs.autoScrollOn") : t("deploymentLogs.autoScrollOff")}
        </button>
        <button
          type="button"
          aria-label={t("deploymentLogs.copyLogs")}
          title={t("deploymentLogs.copyLogs")}
          onClick={() => void copy()}
          className={clsx(button, size)}
        >
          {copyState === "copied" ? <Check className="h-3.5 w-3.5 text-aqua" aria-hidden="true" /> : <Copy className="h-3.5 w-3.5" aria-hidden="true" />}
          {copyState === "copied" ? t("deploymentLogs.logsCopied") : t("deploymentLogs.copyLogs")}
        </button>
      </div>
      <div
        ref={box}
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
        {!loading && !error && lines.length === 0 && empty && (
          <div className="flex flex-col items-center justify-center px-6 py-10 text-center text-slate-200">
            <div className="mb-3 flex h-10 w-10 items-center justify-center rounded-xl border border-white/15 bg-white/10 text-slate-400">
              <TerminalIcon className="h-5 w-5" aria-hidden="true" />
            </div>
            <p className="font-display text-[0.95rem] font-semibold text-slate-100">{empty.title}</p>
            {empty.hint && <p className="mt-1.5 max-w-sm text-sm leading-relaxed text-slate-400">{empty.hint}</p>}
          </div>
        )}
        {lines.map((line, index) => {
          const r = record(line);
          return (
            <div
              key={r.key ?? index}
              data-testid="terminal-line"
              className="whitespace-pre-wrap break-words rounded px-2 py-0.5 hover:bg-white/[0.045]"
              title={r.time}
            >
              {r.source && <span className="mr-2 text-cyan-300">{r.source}</span>}
              {r.time && <span className="mr-2 text-slate-500">[{clock(r.time)}]</span>}
              {r.level && <span className={clsx("mr-2", LEVEL_TONE[r.level.toLowerCase()] ?? "text-slate-600")}>{r.level.toLowerCase()}</span>}
              <span>{r.text}</span>
            </div>
          );
        })}
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
