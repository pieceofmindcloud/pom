/**
 * Why a machine's load failed, in the engine's own words.
 *
 * The node keeps the failure in its runtime status (`RuntimeStatus.failure`,
 * `src/inference/runtime.rs`): the error the engine returned and its last log
 * lines, or, after a native crash, what the engine wrote before dying. It is
 * the same for every engine and needs no plugin; the admin reads every
 * machine's failure because the cluster view already carries each node's status.
 */
import { AlertTriangle, Check, ChevronDown, Copy } from "lucide-react";
import { useState } from "react";
import { copyText } from "../lib/clipboard";
import type { EngineFailure as Failure } from "../lib/types";
import { useI18n } from "../i18n/provider";

export function EngineFailure({ failure, machine }: { failure: Failure; machine: string }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const text = [
    `${machine} - ${failure.engine} (${failure.at})`,
    failure.message,
    ...failure.log_tail,
  ].join("\n");

  async function copy() {
    setCopied(await copyText(text));
    window.setTimeout(() => setCopied(false), 1500);
  }

  return (
    <div role="alert" className="space-y-1.5 border-b border-line bg-rose-500/10 px-3 py-2 text-[0.72rem]">
      <div className="flex items-start gap-1.5 text-rose-300">
        <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
        <div className="min-w-0 flex-1">
          <div className="font-medium">
            {t("engineFailure.title", { engine: failure.engine, attempts: failure.attempts })}
          </div>
          <div className="break-words font-mono text-[0.68rem] text-ink">{failure.message}</div>
        </div>
        <button
          type="button"
          onClick={() => void copy()}
          className="shrink-0 rounded p-1 text-muted hover:text-ink"
          aria-label={t("engineFailure.copy")}
          title={t("engineFailure.copy")}
        >
          {copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
        </button>
      </div>
      {failure.log_tail.length > 0 && (
        <>
          <button
            type="button"
            onClick={() => setOpen(!open)}
            aria-expanded={open}
            className="flex items-center gap-1 text-muted hover:text-ink"
          >
            <ChevronDown className={open ? "h-3 w-3" : "h-3 w-3 -rotate-90"} />
            {t("engineFailure.log", { count: failure.log_tail.length })}
          </button>
          {open && (
            <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all rounded bg-black/40 p-2 font-mono text-[0.64rem] text-ink">
              {failure.log_tail.join("\n")}
            </pre>
          )}
        </>
      )}
    </div>
  );
}
