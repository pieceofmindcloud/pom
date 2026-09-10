import { useI18n } from "../i18n/provider";
import { useCluster } from "../lib/queries";
import { StatsView, type Filters } from "./Logs";

function useNodeNames() {
  const cluster = useCluster();
  const map = new Map<string, string>();
  for (const peer of cluster.data?.peers ?? []) {
    map.set(peer.node_id, peer.node?.hostname || peer.short_id);
  }
  return (id: string) => map.get(id) ?? id.slice(0, 8) + "…";
}

export function Statistics() {
  const { t } = useI18n();
  const name = useNodeNames();
  const filters: Filters = {};

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <header className="flex shrink-0 flex-wrap items-end justify-between gap-2 border-b border-line bg-surface/70 px-4 py-4 sm:px-6">
        <div>
          <h1 className="font-display text-xl font-semibold tracking-tight text-fg">{t("tokenLogs.statsTitle")}</h1>
          <p className="mt-1 text-sm text-muted">{t("tokenLogs.statsSubtitle")}</p>
        </div>
      </header>
      <div className="min-h-0 flex-1 overflow-auto">
        <StatsView filters={filters} name={name} />
      </div>
    </div>
  );
}
