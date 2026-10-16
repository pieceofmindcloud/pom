import { ArrowDownToLine, ChevronUp, Terminal } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { isNearBottom } from "../lib/chatScroll";
import { clock } from "../lib/format";
import { useClusterLogs } from "../lib/queries";
import { useI18n } from "../i18n/provider";
import { Badge, Skeleton } from "./ui";

/** Compact console of the Dashboard: the same live endpoint as the logs screen. */
export function DeploymentLogDock() {
  const { t } = useI18n();
  const logs = useClusterLogs();
  const entries = logs.data?.entries ?? [];
  const [open, setOpen] = useState(false);
  const [autoScroll, setAutoScroll] = useState(true);
  const terminalRef = useRef<HTMLDivElement>(null);
  const wasAtBottom = useRef(true);
  const latestSequence = entries[entries.length - 1]?.received_seq ?? null;

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal || !open || !autoScroll || !wasAtBottom.current || entries.length === 0) return;
    terminal.scrollTo({ top: terminal.scrollHeight, behavior: "auto" });
  }, [autoScroll, entries.length, latestSequence, open]);

  function onScroll() {
    const terminal = terminalRef.current;
    if (!terminal) return;
    const atBottom = isNearBottom({ scrollTop: terminal.scrollTop, clientHeight: terminal.clientHeight, scrollHeight: terminal.scrollHeight });
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

  return <section className={`flex min-h-8 shrink-0 flex-col border-t border-line bg-surface transition-[height] duration-200 ${open ? "h-1/2 max-h-1/2" : "h-8"}`} aria-label={t("deploymentLogs.consoleTitle")}><button type="button" className="flex h-8 shrink-0 items-center gap-2 px-3 text-left text-[0.68rem] text-muted hover:bg-raised hover:text-fg" aria-expanded={open} onClick={() => setOpen((value) => !value)}><Terminal className="h-3.5 w-3.5 text-aqua" /><span className="font-mono">{t("deploymentLogs.consoleTitle")}</span><Badge tone={logs.isError ? "rose" : "aqua"}>{logs.isError ? t("deploymentLogs.offline") : t("deploymentLogs.live")}</Badge><ChevronUp className={`ml-auto h-3.5 w-3.5 transition-transform ${open ? "rotate-180" : ""}`} /></button>{open && <div className="relative min-h-0 flex-1 overflow-hidden bg-black"><button type="button" className="absolute right-3 top-2 z-10 inline-flex h-7 items-center gap-1 rounded border border-white/20 bg-white/10 px-2 text-[0.68rem] text-slate-200 hover:bg-white/20" aria-pressed={autoScroll} aria-label={autoScroll ? t("deploymentLogs.disableAutoScroll") : t("deploymentLogs.enableAutoScroll")} onClick={toggleAutoScroll}><ArrowDownToLine className="h-3 w-3" />{autoScroll ? t("deploymentLogs.autoScrollOn") : t("deploymentLogs.autoScrollOff")}</button><div ref={terminalRef} data-testid="deployment-log-dock-terminal" onScroll={onScroll} className="h-full overflow-auto p-3 pt-11 font-mono text-[0.7rem] leading-5 text-emerald-100" role="log" aria-live="polite">{logs.isLoading && <Skeleton className="h-12 bg-white/10" />}{!logs.isLoading && !logs.isError && entries.length === 0 && <p className="text-slate-500">{t("deploymentLogs.emptyTitle")}</p>}{entries.map((entry) => <div key={entry.received_seq} className="whitespace-pre-wrap break-words rounded px-1.5 py-0.5 hover:bg-white/[0.045]"><span className="mr-2 text-cyan-300">{entry.node_name || entry.node_id}</span><span className="mr-2 text-slate-500">[{clock(entry.origin_at)}]</span><span className="mr-2 text-slate-600">{entry.level.toLowerCase()}</span><span>{entry.message}</span></div>)}</div></div>}</section>;
}
