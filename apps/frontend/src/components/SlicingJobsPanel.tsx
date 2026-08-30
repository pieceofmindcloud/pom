import { CheckCircle2, CircleDot, XCircle } from "lucide-react";
import {
  Badge,
  ErrorState,
  Panel,
  PanelHeader,
  Progress,
  Skeleton,
} from "./ui";
import { bytes, clock, duration, rate, since } from "../lib/format";
import { useJobs } from "../lib/queries";
import type { Job } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { localizeServerText } from "../i18n/locales";

// Written out rather than composed: Tailwind only emits classes it can find as
// literal strings, so `text-${tone}` would produce no CSS at all.
const STATE = {
  running: { tone: "iris", color: "text-iris", labelKey: "jobs.running", icon: CircleDot },
  done: { tone: "aqua", color: "text-aqua", labelKey: "jobs.ready", icon: CheckCircle2 },
  failed: { tone: "rose", color: "text-rose", labelKey: "jobs.failed", icon: XCircle },
} as const;

function JobCard({ job }: { job: Job }) {
  const { t, locale } = useI18n();
  const meta = STATE[job.state] ?? STATE.running;
  const Icon = meta.icon;
  const phaseProgress =
    job.bytes_processed != null && job.total_bytes && job.total_bytes > 0
      ? (job.bytes_processed / job.total_bytes) * 100
      : job.progress;
  return (
    <div className="px-5 py-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex min-w-0 gap-3">
          <Icon
            className={`mt-0.5 h-4 w-4 shrink-0 ${meta.color} ${job.state === "running" ? "animate-pulse-soft" : ""}`}
          />
          <div className="min-w-0">
            <p className="font-medium text-fg">{localizeServerText(locale, job.title || job.step)}</p>
            <p className="break-path mt-0.5 text-[0.82rem] leading-relaxed text-muted">
              {localizeServerText(locale, job.message || job.step)}
            </p>
          </div>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <Badge tone={meta.tone}>{t(meta.labelKey)}</Badge>
          {job.kind && (
            <Badge mono tone="neutral">
              {job.kind}
            </Badge>
          )}
        </div>
      </div>

      {job.state === "running" && (
        <>
          <div className="mt-3 flex flex-wrap items-center justify-between gap-2 text-[0.72rem]">
            <span className="font-medium uppercase tracking-[0.12em] text-iris">
              {localizeServerText(locale, job.step)}
            </span>
            {job.bytes_processed != null && job.total_bytes != null ? (
              <span className="font-mono text-muted">
                {bytes(job.bytes_processed)} {t("jobs.of")} {bytes(job.total_bytes)}
              </span>
            ) : job.progress != null ? (
              <span className="font-mono text-muted">{job.progress.toFixed(0)}%</span>
            ) : null}
          </div>
          <Progress value={phaseProgress} className="mt-2" tone="iris" />
          <div className="mt-2 flex flex-wrap gap-x-4 gap-y-1 font-mono text-[0.7rem] text-faint">
            {job.bytes_processed != null && job.speed_bytes_per_sec != null && (
              <span>{rate(job.speed_bytes_per_sec)}</span>
            )}
            {job.eta_seconds != null && <span>{duration(job.eta_seconds)} {t("jobs.remaining")}</span>}
            {job.current_file && <span className="break-path">{job.current_file}</span>}
            {job.phase_started_at && <span>{t("jobs.phaseElapsed")} {since(job.phase_started_at)}</span>}
          </div>
        </>
      )}

      <div className="mt-2.5 flex flex-wrap items-center gap-x-4 gap-y-1 font-mono text-[0.7rem] text-faint">
        <span>{t("jobs.started")} {since(job.started_at)}</span>
        {job.finished_at && <span>{t("jobs.finishedAt")} {clock(job.finished_at)}</span>}
        <span>#{job.id.slice(0, 8)}</span>
      </div>

      {job.state === "failed" && job.error && job.error !== job.message && (
        <p className="mt-3 border-t border-rose/20 pt-2.5 text-[0.78rem] leading-relaxed text-rose">
          {localizeServerText(locale, job.error)}
        </p>
      )}

      {job.outputs.length > 0 && (
        <ul className="mt-3 space-y-1 border-t border-line pt-2.5">
          {job.outputs.map((output) => (
            <li key={output} className="break-path font-mono text-[0.72rem] text-muted">
              {output}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** The former jobs screen, embedded below the slicing controls. */
export function SlicingJobsPanel() {
  const { t } = useI18n();
  const jobs = useJobs();
  const list = jobs.data?.jobs ?? [];
  const running = list.filter((job) => job.state === "running");

  if (jobs.isError) return <ErrorState error={jobs.error} onRetry={() => void jobs.refetch()} />;
  if (jobs.isLoading) return <Skeleton className="h-40 w-full" />;
  if (list.length === 0) return null;

  return (
    <Panel aria-label={t("jobs.title")}>
      <PanelHeader
        eyebrow={t("jobs.title")}
        title={running.length ? t("jobs.runningCount", { count: running.length }) : t("jobs.recentTitle")}
        subtitle={t("jobs.subtitle")}
      />
      <div className="divide-y divide-line">
        {list.map((job) => <JobCard key={job.id} job={job} />)}
      </div>
    </Panel>
  );
}
