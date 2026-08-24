import { CheckCircle2, Download, RotateCcw, X, XCircle } from "lucide-react";
import { useEffect, useRef } from "react";
import { useI18n } from "../i18n/provider";
import { cancelRemoteModelDownload, retryRemoteModelDownload } from "../lib/api";
import { bytes, duration, rate } from "../lib/format";
import { isModelDownloadActive, percentOfDownload } from "../lib/modelProvider";
import { useModelDownloads } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { Job, RemoteDownloadStatus } from "../lib/types";
import { Badge, Button, ErrorState, Panel, PanelHeader, Progress, Skeleton } from "./ui";

const STATUS_TONE: Record<RemoteDownloadStatus, "iris" | "aqua" | "amber" | "rose" | "sky"> = {
  queued: "amber",
  starting: "amber",
  downloading: "iris",
  verifying: "sky",
  completed: "aqua",
  cancelled: "amber",
  failed: "rose",
};

function statusLabel(status: RemoteDownloadStatus | undefined, t: (key: string, values?: Record<string, string | number>) => string) {
  return t(`remoteModels.${status ?? "failed"}`);
}

function DownloadRow({ job, onRefresh }: { job: Job; onRefresh: () => void }) {
  const { t } = useI18n();
  const toast = useToast();
  const status = job.download_status;
  const active = isModelDownloadActive(job);
  const progress = percentOfDownload(job) ?? job.progress;
  const name = job.quantization || job.filename || job.title || job.repo_id || job.id;

  async function cancel() {
    try {
      await cancelRemoteModelDownload(job.id);
      toast.info(t("remoteModels.cancel"));
      onRefresh();
    } catch (error) {
      toast.error(t("remoteModels.downloadFailed"));
    }
  }

  async function retry() {
    try {
      await retryRemoteModelDownload(job.id);
      toast.info(t("remoteModels.starting"));
      onRefresh();
    } catch (error) {
      toast.error(t("remoteModels.downloadFailed"));
    }
  }

  return (
    <div className="flex gap-3 border-t border-line px-5 py-4 first:border-t-0">
      <div className="mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-line bg-raised text-muted">
        {status === "completed" ? <CheckCircle2 className="h-4 w-4 text-aqua" /> : status === "failed" ? <XCircle className="h-4 w-4 text-rose" /> : <Download className="h-4 w-4 text-iris" />}
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-start justify-between gap-2">
          <div className="min-w-0">
            <p className="truncate text-sm font-medium text-fg" title={job.repo_id}>{job.repo_id || job.title}</p>
            <p className="break-path mt-0.5 font-mono text-xs text-muted" title={job.filename}>{name}</p>
          </div>
          <Badge tone={STATUS_TONE[status ?? "failed"]}>{statusLabel(status, t)}</Badge>
        </div>
        <Progress value={progress} tone={STATUS_TONE[status ?? "downloading"]} className="mt-3" />
        <div className="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1 font-mono text-[0.7rem] text-faint">
          <span>{progress == null ? "—" : `${Math.round(progress)}%`}</span>
          <span>{bytes(job.bytes_downloaded)} / {bytes(job.total_bytes)}</span>
          {job.speed_bytes_per_sec != null && <span>{t("remoteModels.speed")} {rate(job.speed_bytes_per_sec)}</span>}
          {job.eta_seconds != null && <span>{t("remoteModels.remaining", { time: duration(job.eta_seconds) })}</span>}
        </div>
        {job.error && <p className="break-path mt-2 text-xs text-rose">{job.error}</p>}
        <div className="mt-3 flex flex-wrap gap-2">
          {active && (
            <Button size="sm" variant="ghost" icon={<X className="h-3.5 w-3.5" />} onClick={() => void cancel()}>
              {t("remoteModels.cancel")}
            </Button>
          )}
          {(status === "failed" || status === "cancelled") && (
            <Button size="sm" variant="outline" icon={<RotateCcw className="h-3.5 w-3.5" />} onClick={() => void retry()}>
              {t("remoteModels.retry")}
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}

export function ModelDownloadsPanel() {
  const { t } = useI18n();
  const downloads = useModelDownloads();
  const toast = useToast();
  const previous = useRef(new Map<string, RemoteDownloadStatus | undefined>());
  const list = downloads.data ?? [];

  useEffect(() => {
    for (const job of list) {
      const before = previous.current.get(job.id);
      if (before && before !== job.download_status && job.download_status === "completed") {
        toast.success(t("remoteModels.completed"), job.filename);
      }
      if (before && before !== job.download_status && job.download_status === "failed") {
        toast.error(t("remoteModels.downloadFailed"), job.error ?? undefined);
      }
      previous.current.set(job.id, job.download_status);
    }
  }, [list, t, toast]);

  if (downloads.isError) return <ErrorState error={downloads.error} onRetry={() => void downloads.refetch()} />;
  if (downloads.isLoading) return <Panel><Skeleton className="m-5 h-28" /></Panel>;
  if (list.length === 0) return null;

  return (
    <Panel aria-label={t("remoteModels.downloadsTitle")}>
      <PanelHeader
        eyebrow={t("remoteModels.downloadsTitle")}
        title={t("remoteModels.downloadsCount", { count: list.filter((job) => job.state === "running").length })}
        subtitle={t("remoteModels.subtitle")}
      />
      <div>{list.map((job) => <DownloadRow key={job.id} job={job} onRefresh={() => void downloads.refetch()} />)}</div>
    </Panel>
  );
}
