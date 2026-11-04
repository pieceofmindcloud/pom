import { CheckCircle2, LayoutDashboard, Loader2, XCircle } from "lucide-react";
import { Modal } from "./Confirm";
import { Button } from "./ui";
import { useI18n } from "../i18n/provider";
import type { Job } from "../lib/types";

/**
 * The deploy the operator just started, followed until it ends.
 *
 * The Deploy button used to answer only with a toast pointing at the bell, and
 * the bell dropped the job the moment it finished: the operator saw nothing
 * happen and deployed again. This window stays over the screen with the job's
 * steps and its outcome. Closing it never stops the deploy; the bell keeps
 * the result (`Notifications`).
 */
export function DeployProgressModal({
  open,
  job,
  onClose,
  onDashboard,
}: {
  open: boolean;
  /** `undefined` until the first poll of `/api/ui/jobs` sees the new job. */
  job: Job | undefined;
  onClose: () => void;
  onDashboard: () => void;
}) {
  const { t } = useI18n();
  const state = job?.state ?? "running";
  const progress = job?.progress ?? null;
  return (
    <Modal
      open={open}
      title={t(state === "done" ? "deployProgress.doneTitle" : state === "failed" ? "deployProgress.failedTitle" : "deployProgress.runningTitle")}
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>{t("common.close")}</Button>
          <Button variant="primary" icon={<LayoutDashboard className="h-3.5 w-3.5" />} onClick={onDashboard}>
            {t("deployProgress.dashboard")}
          </Button>
        </>
      }
    >
      <div className="space-y-3" data-testid="deploy-progress">
        <div className="flex items-start gap-2.5">
          {state === "done" ? (
            <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0 text-aqua" aria-hidden />
          ) : state === "failed" ? (
            <XCircle className="mt-0.5 h-4 w-4 shrink-0 text-rose" aria-hidden />
          ) : (
            <Loader2 className="mt-0.5 h-4 w-4 shrink-0 animate-spin text-iris" aria-hidden />
          )}
          <p className="min-w-0 text-sm text-fg">
            {state === "done"
              ? t("deployProgress.doneDetail")
              : state === "failed"
                ? t("deployProgress.failedDetail")
                : job?.step || job?.message || t("deployProgress.starting")}
          </p>
        </div>
        {state === "running" && (
          <div
            role="progressbar"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={progress == null ? undefined : Math.round(progress)}
            className="h-1.5 overflow-hidden rounded-full bg-line"
          >
            <div
              className="h-full rounded-full bg-iris transition-[width]"
              style={{ width: `${Math.max(4, Math.min(100, progress ?? 4))}%` }}
            />
          </div>
        )}
        {state === "failed" && job?.message && (
          <p role="alert" className="break-words text-xs leading-relaxed text-rose">{job.message}</p>
        )}
        {state === "running" && <p className="text-xs text-muted">{t("deployProgress.closeHint")}</p>}
      </div>
    </Modal>
  );
}
