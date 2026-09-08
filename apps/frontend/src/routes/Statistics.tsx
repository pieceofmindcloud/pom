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
    <div className="min-w-0">
      <header className="mb-4">
        <p className="eyebrow">{t("tokenLogs.statsTab")}</p>
        <h1 className="font-display text-xl font-semibold tracking-tight text-fg">{t("tokenLogs.statsTitle")}</h1>
        <p className="mt-1 text-sm text-muted">{t("tokenLogs.statsSubtitle")}</p>
      </header>
      <StatsView filters={filters} name={name} />
    </div>
  );
}
