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

// Written out rather than composed: Tailwind only emits classes it can find as
// literal strings, so `text-${tone}` would produce no CSS at all.
const STATE = {
  running: { tone: "iris", color: "text-iris", label: "rodando", icon: CircleDot },
  done: { tone: "aqua", color: "text-aqua", label: "pronto", icon: CheckCircle2 },
  failed: { tone: "rose", color: "text-rose", label: "falhou", icon: XCircle },
} as const;

function JobCard({ job }: { job: Job }) {
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
          <Badge tone={meta.tone}>{meta.label}</Badge>
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
        <span>começou {since(job.started_at)}</span>
        {job.finished_at && <span>terminou {clock(job.finished_at)}</span>}
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
  const jobs = useJobs();
  const list = jobs.data?.jobs ?? [];
  const running = list.filter((j) => j.state === "running");

  return (
    <>
      <PageHeader
        title="Fatiamentos em andamento"
        subtitle="O progresso é do nó, não do navegador: recarregar a página não perde nada, e fechar a aba não interrompe nada."
        actions={
          <Link to="/modelos/fatiar">
            <Button variant="primary" icon={<Scissors className="h-3.5 w-3.5" />}>
              Fatiar um modelo
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
                  ? `${running.length} ${running.length === 1 ? "trabalho rodando" : "trabalhos rodando"}`
                  : "Nada rodando agora"
              }
              subtitle="Trabalhos terminados ficam listados por dez minutos."
            />
          )}
          {list.length === 0 ? (
            <Empty icon={<ListTree className="h-5 w-5" />} title="Nada rodando agora">
              Fatiamentos e atribuições de modelo aparecem aqui enquanto rodam, e por dez
              minutos depois de terminarem.
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
