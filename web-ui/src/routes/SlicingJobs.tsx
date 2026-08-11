import { CheckCircle2, CircleDot, ListTree, Scissors, XCircle } from "lucide-react";
import { Link } from "react-router-dom";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  PageHeader,
  Panel,
  PanelHeader,
  Progress,
  Skeleton,
} from "../components/ui";
import { clock, since } from "../lib/format";
import { useJobs } from "../lib/queries";
import type { Job } from "../lib/types";
import { useI18n } from "../i18n/provider";

// Written out rather than composed: Tailwind only emits classes it can find as
// literal strings, so `text-${tone}` would produce no CSS at all.
const STATE = {
  running: { tone: "iris", color: "text-iris", labelKey: "jobs.running", icon: CircleDot },
  done: { tone: "aqua", color: "text-aqua", labelKey: "jobs.ready", icon: CheckCircle2 },
  failed: { tone: "rose", color: "text-rose", labelKey: "jobs.failed", icon: XCircle },
} as const;

function JobCard({ job }: { job: Job }) {
  const { t } = useI18n();
  const meta = STATE[job.state] ?? STATE.running;
  const Icon = meta.icon;
  return (
    <div className="px-5 py-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex min-w-0 gap-3">
          <Icon
            className={`mt-0.5 h-4 w-4 shrink-0 ${meta.color} ${job.state === "running" ? "animate-pulse-soft" : ""}`}
          />
          <div className="min-w-0">
            <p className="font-medium text-fg">{job.title || job.step}</p>
            <p className="break-path mt-0.5 text-[0.82rem] leading-relaxed text-muted">
              {job.message || job.step}
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
        <Progress value={job.progress} className="mt-3" tone="iris" />
      )}

      <div className="mt-2.5 flex flex-wrap items-center gap-x-4 gap-y-1 font-mono text-[0.7rem] text-faint">
        <span>{t("jobs.started")} {since(job.started_at)}</span>
        {job.finished_at && <span>{t("jobs.finishedAt")} {clock(job.finished_at)}</span>}
        {job.progress != null && job.state === "running" && (
          <span>{job.progress.toFixed(0)}%</span>
        )}
        <span>#{job.id.slice(0, 8)}</span>
      </div>

      {job.outputs.length > 0 && (
        <ul className="mt-3 space-y-1 border-t border-line pt-2.5">
          {job.outputs.map((o) => (
            <li key={o} className="break-path font-mono text-[0.72rem] text-muted">
              {o}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

export function SlicingJobs() {
  const { t } = useI18n();
  const jobs = useJobs();
  const list = jobs.data?.jobs ?? [];
  const running = list.filter((j) => j.state === "running");

  return (
    <>
      <PageHeader
        title={t("jobs.title")}
        subtitle={t("jobs.subtitle")}
        actions={
          <Link to="/modelos/fatiar">
            <Button variant="primary" icon={<Scissors className="h-3.5 w-3.5" />}>
              {t("jobs.sliceModel")}
            </Button>
          </Link>
        }
      />

      {jobs.isError && <ErrorState error={jobs.error} onRetry={() => void jobs.refetch()} />}
      {jobs.isLoading && <Skeleton className="h-40 w-full" />}

      {jobs.data && (
        <Panel>
          {/* An empty panel says its piece once: the header would only repeat
              what the empty state already spells out. */}
          {list.length > 0 && (
            <PanelHeader
              title={
                running.length
                  ? t("jobs.runningCount", { count: running.length })
                  : t("jobs.nothingRunning")
              }
              subtitle={t("jobs.finishedDetail")}
            />
          )}
          {list.length === 0 ? (
            <Empty icon={<ListTree className="h-5 w-5" />} title={t("jobs.nothingRunning")}>
              {t("jobs.emptyDetail")}
            </Empty>
          ) : (
            <div className="divide-y divide-line">
              {list.map((j) => (
                <JobCard key={j.id} job={j} />
              ))}
            </div>
          )}
        </Panel>
      )}
    </>
  );
}
