import { ArrowUpRight, Download, LockKeyhole } from "lucide-react";
import { useI18n } from "../i18n/provider";
import { bytes, num, since } from "../lib/format";
import type { RemoteModelSummary } from "../lib/types";
import { Badge, Button, Panel } from "./ui";

export function RemoteModelCard({
  model,
  onOpen,
}: {
  model: RemoteModelSummary;
  onOpen: (model: RemoteModelSummary) => void;
}) {
  const { t } = useI18n();
  return (
    <Panel className="flex min-w-0 flex-col p-4 transition-colors duration-150 hover:border-line-strong">
      <div className="flex min-w-0 items-start justify-between gap-3">
        <button
          type="button"
          className="min-w-0 text-left focus:outline-none focus-visible:ring-2 focus-visible:ring-iris/50"
          onClick={() => onOpen(model)}
        >
          <span className="break-path block font-display text-[0.98rem] font-semibold tracking-tight text-fg">
            {model.repo_id}
          </span>
          {model.author && <span className="mt-1 block text-xs text-muted">{model.author}</span>}
        </button>
        <Button
          variant="ghost"
          size="sm"
          icon={<ArrowUpRight className="h-3.5 w-3.5" />}
          onClick={() => onOpen(model)}
          aria-label={t("remoteModels.openDetails", { name: model.repo_id })}
        >
          {t("remoteModels.openDetails", { name: "" }).replace(/ $/, "")}
        </Button>
      </div>

      <p className="mt-3 line-clamp-2 min-h-[2.5rem] text-sm leading-relaxed text-muted">
        {model.description || t("remoteModels.searchHint")}
      </p>

      <div className="mt-3 flex min-h-6 flex-wrap items-center gap-1.5">
        {model.has_gguf && (
          <Badge tone="aqua" mono>
            <Download className="h-3 w-3" />
            {t("remoteModels.gguf")}
          </Badge>
        )}
        {model.gated && (
          <Badge tone="amber">
            <LockKeyhole className="h-3 w-3" />
            {t("remoteModels.gated")}
          </Badge>
        )}
        {model.private && <Badge tone="amber">{t("remoteModels.private")}</Badge>}
        {model.installed && <Badge tone="aqua">{t("remoteModels.downloaded")}</Badge>}
        {model.downloading && <Badge tone="amber">{t("remoteModels.downloading")}</Badge>}
        {model.tags.slice(0, 3).map((tag) => (
          <Badge key={tag} tone="neutral">
            {tag}
          </Badge>
        ))}
      </div>

      <dl className="mt-4 grid grid-cols-3 gap-3 border-t border-line pt-3">
        <div className="min-w-0">
          <dt className="eyebrow">{t("remoteModels.downloads")}</dt>
          <dd className="mt-1 font-mono text-xs tabular-nums text-fg">{num(model.downloads)}</dd>
        </div>
        <div className="min-w-0">
          <dt className="eyebrow">{t("remoteModels.likes")}</dt>
          <dd className="mt-1 font-mono text-xs tabular-nums text-fg">{num(model.likes)}</dd>
        </div>
        <div className="min-w-0">
          <dt className="eyebrow">{t("remoteModels.updated")}</dt>
          <dd className="mt-1 truncate text-xs text-fg" title={model.last_modified ?? undefined}>
            {since(model.last_modified)}
          </dd>
        </div>
      </dl>
    </Panel>
  );
}

export function RemoteFileMeta({ size }: { size: number | null }) {
  return <span className="font-mono text-xs tabular-nums text-fg">{bytes(size)}</span>;
}
