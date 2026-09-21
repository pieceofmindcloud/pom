import { Search } from "lucide-react";
import { useState } from "react";
import { useI18n } from "../i18n/provider";
import { useCluster } from "../lib/queries";
import { Button, Input, Select } from "../components/ui";
import { StatsView, toFilters, type FilterDraft, type Filters } from "./Logs";

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
  const cluster = useCluster();
  const name = useNodeNames();
  const [draft, setDraft] = useState<FilterDraft>({ node: "", user: "", apiKey: "", model: "", prompt: "", from: "", to: "" });
  const [filters, setFilters] = useState<Filters>({});

  function update(field: keyof FilterDraft, value: string) {
    setDraft((current) => ({ ...current, [field]: value }));
  }

  function search(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setFilters(toFilters(draft));
  }

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <header className="shrink-0 border-b border-line bg-surface/75">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-3 px-3 py-3 sm:px-4">
          <div className="min-w-0">
          <h1 className="font-display text-xl font-semibold tracking-tight text-fg">{t("tokenLogs.statsTitle")}</h1>
            <p className="mt-0.5 truncate text-[0.7rem] text-muted">{t("tokenLogs.statsSubtitle")}</p>
          </div>
        </div>
        <form className="flex min-w-0 flex-wrap items-end gap-2 border-t border-line px-3 py-2 sm:px-4" onSubmit={search}>
          <label className="flex min-w-[10rem] flex-1 flex-col gap-1 sm:max-w-[14rem]">
            <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("tokenLogs.user")}</span>
            <Input id="stats-user" compact value={draft.user} onChange={(event) => update("user", event.target.value)} placeholder={t("tokenLogs.userPlaceholder")} />
          </label>
          <label className="flex min-w-[10rem] flex-1 flex-col gap-1 sm:max-w-[14rem]">
            <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("tokenLogs.apiKey")}</span>
            <Input id="stats-api-key" compact value={draft.apiKey} onChange={(event) => update("apiKey", event.target.value)} placeholder={t("tokenLogs.apiKeyPlaceholder")} />
          </label>
          <label className="flex min-w-[10rem] flex-1 flex-col gap-1 sm:max-w-[14rem]">
            <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("tokenLogs.model")}</span>
            <Input id="stats-model" compact value={draft.model} onChange={(event) => update("model", event.target.value)} placeholder={t("tokenLogs.modelPlaceholder")} />
          </label>
          <label className="flex min-w-[9rem] flex-1 flex-col gap-1 sm:max-w-[12rem]">
            <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("tokenLogs.filter")}</span>
            <Select id="stats-node" compact aria-label={t("tokenLogs.filter")} value={draft.node} onChange={(event) => update("node", event.target.value)}>
              <option value="">{t("tokenLogs.allNodes")}</option>
              {(cluster.data?.peers ?? []).map((peer) => <option key={peer.node_id} value={peer.node_id}>{peer.node?.hostname || peer.short_id}</option>)}
            </Select>
          </label>
          <label className="flex min-w-[10.5rem] flex-1 flex-col gap-1 sm:flex-none">
            <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("tokenLogs.from")}</span>
            <Input id="stats-from" aria-label={t("tokenLogs.from")} compact type="datetime-local" value={draft.from} onChange={(event) => update("from", event.target.value)} />
          </label>
          <label className="flex min-w-[10.5rem] flex-1 flex-col gap-1 sm:flex-none">
            <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("tokenLogs.to")}</span>
            <Input id="stats-to" aria-label={t("tokenLogs.to")} compact type="datetime-local" value={draft.to} onChange={(event) => update("to", event.target.value)} />
          </label>
          <label className="flex min-w-[10rem] flex-1 flex-col gap-1 sm:max-w-[14rem]">
            <span className="text-[0.62rem] font-medium uppercase tracking-[0.1em] text-faint">{t("tokenLogs.promptSearch")}</span>
            <Input id="stats-prompt" compact value={draft.prompt} onChange={(event) => update("prompt", event.target.value)} placeholder={t("tokenLogs.promptPlaceholder")} />
          </label>
          <Button type="submit" size="sm" variant="primary" icon={<Search className="h-3.5 w-3.5" />}>{t("tokenLogs.applyFilters")}</Button>
        </form>
      </header>
      <div className="min-h-0 flex-1 overflow-auto">
        <StatsView filters={filters} name={name} />
      </div>
    </div>
  );
}
