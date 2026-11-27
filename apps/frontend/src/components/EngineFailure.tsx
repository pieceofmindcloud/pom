/**
 * Why a machine's load failed, in the engine's own words.
 *
 * The node keeps the failure in its runtime status (`RuntimeStatus.failure`,
 * `src/inference/runtime.rs`): the error the engine returned and its last log
 * lines, or, after a native crash, what the engine wrote before dying. It is
 * the same for every engine and needs no plugin; the admin reads every
 * machine's failure because the cluster view already carries each node's status.
 *
 * `EngineFailure` is the one line a machine card shows (its foot on the Flow,
 * its top in the details), with Details opening `EngineFailureDialog`: the
 * whole message, the engine log and a copy button. `icon` is the compact
 * card's form, where only an alert icon fits.
 */
import { AlertTriangle, Check, Copy } from "lucide-react";
import clsx from "clsx";
import { useState } from "react";
import { copyText } from "../lib/clipboard";
import type { EngineFailure as Failure } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { Modal } from "./Confirm";
import { Button } from "./ui";

export function EngineFailure({
  failure,
  machine,
  variant = "line",
  className,
}: {
  failure: Failure;
  machine: string;
  variant?: "line" | "icon";
  className?: string;
}) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const title = t("engineFailure.title", { engine: failure.engine, attempts: failure.attempts });
  const dialog = <EngineFailureDialog open={open} onClose={() => setOpen(false)} failure={failure} machine={machine} />;

  if (variant === "icon") {
    return (
      <>
        <button
          type="button"
          data-testid="engine-failure-icon"
          onClick={() => setOpen(true)}
          className="inline-flex shrink-0 items-center rounded p-0.5 text-rose hover:bg-rose/15"
          aria-label={`${title}: ${t("engineFailure.details")}`}
          title={`${title}: ${failure.message}`}
        >
          <AlertTriangle className="h-3.5 w-3.5" />
        </button>
        {dialog}
      </>
    );
  }

  return (
    <>
      <div
        role="alert"
        data-testid="engine-failure"
        className={clsx("flex items-center gap-1.5 bg-rose/10 px-2 py-1 text-[0.68rem] text-rose", className)}
      >
        <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
        <span className="min-w-0 flex-1 truncate" title={failure.message}>
          <span className="font-semibold">{t("engineFailure.short")}</span> {failure.message}
        </span>
        <button
          type="button"
          onClick={() => setOpen(true)}
          className="shrink-0 rounded border border-rose/40 px-1.5 py-0.5 font-medium hover:bg-rose/15"
        >
          {t("engineFailure.details")}
        </button>
      </div>
      {dialog}
    </>
  );
}

export function EngineFailureDialog({
  open,
  onClose,
  failure,
  machine,
}: {
  open: boolean;
  onClose: () => void;
  failure: Failure;
  machine: string;
}) {
  const { t } = useI18n();
  const [copied, setCopied] = useState(false);
  const text = [`${machine} - ${failure.engine} (${failure.at})`, failure.message, ...failure.log_tail].join("\n");

  async function copy() {
    setCopied(await copyText(text));
    window.setTimeout(() => setCopied(false), 1500);
  }

  return (
    <Modal
      open={open}
      onClose={onClose}
      size="lg"
      title={t("engineFailure.dialogTitle", { machine })}
      footer={
        <>
          <Button variant="ghost" onClick={() => void copy()} icon={copied ? <Check className="h-4 w-4" /> : <Copy className="h-4 w-4" />}>
            {copied ? t("engineFailure.copied") : t("engineFailure.copy")}
          </Button>
          <Button onClick={onClose}>{t("engineFailure.close")}</Button>
        </>
      }
    >
      <div className="space-y-3 text-sm">
        <p className="text-muted">
          {t("engineFailure.title", { engine: failure.engine, attempts: failure.attempts })} · {new Date(failure.at).toLocaleString()}
        </p>
        <pre className="whitespace-pre-wrap break-words rounded border border-rose/30 bg-rose/10 p-2 font-mono text-[0.75rem] text-rose">
          {failure.message}
        </pre>
        <div>
          <p className="mb-1 text-xs font-semibold text-muted">{t("engineFailure.log", { count: failure.log_tail.length })}</p>
          {failure.log_tail.length > 0 ? (
            <pre data-testid="engine-failure-log" className="max-h-80 overflow-auto whitespace-pre-wrap break-all rounded bg-black p-2 font-mono text-[0.7rem] text-emerald-100">
              {failure.log_tail.join("\n")}
            </pre>
          ) : (
            <p className="text-xs text-faint">{t("engineFailure.noLog")}</p>
          )}
        </div>
      </div>
    </Modal>
  );
}
