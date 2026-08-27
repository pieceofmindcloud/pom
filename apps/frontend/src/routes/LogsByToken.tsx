/**
 * Persistent request audit.
 *
 * The backend writes the first row before inference starts, then completes the
 * same SQLite row. This screen is deliberately an inspector: identity,
 * prompt, request body and telemetry are available without exposing a key
 * secret.
 */
import { KeyRound, Radio, Search, Server } from "lucide-react";
import { Fragment, useState } from "react";
import {
  Badge,
  Empty,
  ErrorState,
  Field,
  Input,
  Mono,
  PageHeader,
  Panel,
  PanelHeader,
  ScrollX,
  Select,
  Skeleton,
  type Tone,
} from "../components/ui";
import { clock, ms, num, since } from "../lib/format";
import { useCluster, useRequestLog } from "../lib/queries";
import type { RequestLogEntry } from "../lib/types";
import { useI18n } from "../i18n/provider";

/** Peer names, so a log line reads as machines rather than as hex. */
function useNodeNames() {
  const cluster = useCluster();
  const map = new Map<string, string>();
  for (const p of cluster.data?.peers ?? []) {
    map.set(p.node_id, p.node?.hostname || p.short_id);
  }
  return (id: string) => map.get(id) ?? id.slice(0, 8) + "…";
}

export function HopChain({ entry, name }: { entry: RequestLogEntry; name: (id: string) => string }) {
  const { t } = useI18n();
  if (entry.hops.length === 0) {
    return (
      <span className="flex flex-wrap items-center gap-1">
        <span className="font-mono text-[0.72rem] text-fg">{name(entry.entry_node_id)}</span>
        <span className="text-[0.78rem] text-faint">· {t("tokenLogs.noPipeline")}</span>
      </span>
    );
  }
  return (
    <span className="flex flex-wrap items-center gap-1">
      {entry.hops.map((h, i) => (
        <span key={h + "-" + i} className="flex items-center gap-1">
          {i > 0 && <span className="text-faint">→</span>}
          <span className="font-mono text-[0.72rem] text-fg">{name(h)}</span>
        </span>
      ))}
    </span>
  );
}

export function statusTone(status: string): Tone {
  if (status === "ok") return "aqua";
  if (status === "partial" || status === "incomplete") return "amber";
  if (status === "in_progress") return "iris";
  return "rose";
}

export function statusLabel(status: string, t: (key: string) => string) {
  const labels: Record<string, string> = {
    ok: t("tokenLogs.statusCompleted"),
    partial: t("tokenLogs.statusPartial"),
    error: t("tokenLogs.statusError"),
    abandoned: t("tokenLogs.statusAbandoned"),
    in_progress: t("tokenLogs.statusInProgress"),
    incomplete: t("tokenLogs.statusIncomplete"),
  };
  return labels[status] ?? status;
}

function JsonBlock({ value }: { value: unknown }) {
  return (
    <pre className="max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-lg border border-line bg-ink p-3 font-mono text-[0.72rem] leading-relaxed text-muted">
      {JSON.stringify(value, null, 2)}
    </pre>
  );
}

function rateText(value: number | null | undefined, unavailable: string, unit: string) {
  return value == null ? unavailable : `${num(value, 1)} ${unit}`;
}

function AuditDetails({ entry, name }: { entry: RequestLogEntry; name: (id: string) => string }) {
  const { t } = useI18n();
  return (
    <details className="group">
      <summary className="cursor-pointer list-none text-[0.75rem] text-iris group-open:mb-3">
        {t("tokenLogs.inspect")}
      </summary>
      <div className="mb-4 flex flex-wrap gap-x-5 gap-y-1.5 text-[0.72rem] text-muted">
        <span>
          {t("tokenLogs.route")}: <Mono>{entry.route}</Mono>
        </span>
        <span>
          {t("tokenLogs.entryNode")}: <Mono>{name(entry.entry_node_id)}</Mono>
        </span>
        <span>
          {t("tokenLogs.source")}: <Mono>{entry.source}</Mono>
        </span>
        {entry.completed_at && (
          <span>
            {t("tokenLogs.completedAt")}: <Mono>{clock(entry.completed_at)}</Mono>
          </span>
        )}
      </div>
      <div className="grid min-w-0 gap-4 lg:grid-cols-2">
        <div className="min-w-0 space-y-3">
          <div>
            <p className="mb-1.5 text-[0.72rem] font-medium uppercase tracking-[0.12em] text-faint">
              {t("tokenLogs.prompt")}
            </p>
            <pre className="max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-lg border border-line bg-ink p-3 font-mono text-[0.78rem] leading-relaxed text-fg">
              {entry.prompt}
            </pre>
          </div>
          <div>
            <p className="mb-1.5 text-[0.72rem] font-medium uppercase tracking-[0.12em] text-faint">
              {t("tokenLogs.requestDetails")}
            </p>
            <JsonBlock value={entry.request_details} />
          </div>
        </div>
        <div className="min-w-0 space-y-3">
          <div>
            <p className="mb-1.5 text-[0.72rem] font-medium uppercase tracking-[0.12em] text-faint">
              {t("tokenLogs.result")}
            </p>
            <pre className="max-h-56 overflow-auto whitespace-pre-wrap break-words rounded-lg border border-line bg-ink p-3 font-mono text-[0.78rem] leading-relaxed text-fg">
              {entry.result ?? t("tokenLogs.noResult")}
            </pre>
          </div>
          <div>
            <p className="mb-1.5 text-[0.72rem] font-medium uppercase tracking-[0.12em] text-faint">
              {t("tokenLogs.telemetry")}
            </p>
            <JsonBlock value={entry.telemetry ?? t("tokenLogs.noTelemetry")} />
          </div>
          {entry.timings && (
            <div className="rounded-lg border border-line/70 bg-surface/30 p-3 text-[0.72rem] text-muted">
              <p className="mb-1.5 font-medium uppercase tracking-[0.12em] text-faint">
                {t("tokenLogs.performance")}
              </p>
              <div className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1 font-mono">
                <span>{t("chat.ttft")}</span>
                <span>{ms(entry.timings.ttft_ms)}</span>
                <span>{t("chat.prefillSpeed")}</span>
                <span>{rateText(entry.timings.prefill_tokens_per_sec, t("tokenLogs.rateUnavailable"), t("tokenLogs.tokensPerSecondUnit"))}</span>
                <span>{t("chat.promptTokensUnit")}</span>
                <span>{entry.timings.prefill_tokens}</span>
                <span>{t("chat.generationSpeed")}</span>
                <span>{rateText(entry.timings.generation_tokens_per_sec, t("tokenLogs.rateUnavailable"), t("tokenLogs.tokensPerSecondUnit"))}</span>
                <span>{t("chat.tokensUnit")}</span>
                <span>{entry.timings.generation_tokens}</span>
                <span>{t("chat.totalTime")}</span>
                <span>{ms(entry.timings.total_ms)} · {num(entry.timings.total_tokens_per_sec, 1)} {t("tokenLogs.tokensPerSecondUnit")}</span>
              </div>
            </div>
          )}
        </div>
      </div>
      {entry.error && (
        <p className="mt-4 break-words rounded-lg border border-rose/25 bg-rose/8 p-3 text-[0.78rem] text-rose">
          {entry.error}
        </p>
      )}
    </details>
  );
}

function Tally({
  title,
  icon,
  rows,
  emptyLabel,
}: {
  title: string;
  icon: React.ReactNode;
  rows: Array<[string, number]>;
  emptyLabel: string;
}) {
  const max = Math.max(1, ...rows.map(([, n]) => n));
  return (
    <Panel>
      <PanelHeader title={title} eyebrow={<span className="flex items-center gap-1.5">{icon}</span>} />
      {rows.length === 0 ? (
        <p className="px-5 py-6 text-center text-[0.82rem] text-faint">{emptyLabel}</p>
      ) : (
        <ul className="space-y-2.5 px-5 py-4">
          {rows.map(([label, count]) => (
            <li key={label} className="min-w-0">
              <div className="mb-1 flex items-baseline justify-between gap-3">
                <span className="truncate text-[0.82rem] text-fg">{label}</span>
                <Mono className="shrink-0 text-muted">{num(count)}</Mono>
              </div>
              <div className="h-1.5 w-full overflow-hidden rounded-full bg-line">
                <div
                  className="h-full rounded-full bg-iris transition-[width] duration-500 ease-swift"
                  style={{ width: String((count / max) * 100) + "%" }}
                />
              </div>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

export function LogsByToken() {
  const { t } = useI18n();
  const [nodeFilter, setNodeFilter] = useState("");
  const [apiKeyFilter, setApiKeyFilter] = useState("");
  const [promptFilter, setPromptFilter] = useState("");
  const [fromFilter, setFromFilter] = useState("");
  const [toFilter, setToFilter] = useState("");
  const cluster = useCluster();
  const log = useRequestLog({
    node_id: nodeFilter || undefined,
    api_key: apiKeyFilter.trim() || undefined,
    prompt: promptFilter.trim() || undefined,
    from: fromFilter ? new Date(fromFilter).toISOString() : undefined,
    to: toFilter ? new Date(toFilter).toISOString() : undefined,
  });
  const name = useNodeNames();
  const data = log.data;

  const byToken = Object.entries(data?.by_token ?? {}).sort((a, b) => b[1] - a[1]);
  const byNode = Object.entries(data?.by_node ?? {})
    .sort((a, b) => b[1] - a[1])
    .map(([id, n]) => [name(id), n] as [string, number]);

  return (
    <>
      <PageHeader title={t("tokenLogs.title")} subtitle={t("tokenLogs.subtitle")} />

      <Panel>
        <PanelHeader
          title={t("tokenLogs.filters")}
          eyebrow={<Search className="h-3.5 w-3.5" />}
          subtitle={t("tokenLogs.filtersHint")}
        />
        <div className="grid gap-4 px-5 py-4 md:grid-cols-2 xl:grid-cols-5">
          <Field label={t("tokenLogs.apiKey")}>
            <Input
              value={apiKeyFilter}
              onChange={(e) => setApiKeyFilter(e.target.value)}
              placeholder={t("tokenLogs.apiKeyPlaceholder")}
              aria-label={t("tokenLogs.apiKey")}
            />
          </Field>
          <Field label={t("tokenLogs.promptSearch")}>
            <Input
              value={promptFilter}
              onChange={(e) => setPromptFilter(e.target.value)}
              placeholder={t("tokenLogs.promptPlaceholder")}
              aria-label={t("tokenLogs.promptSearch")}
            />
          </Field>
          <Field label={t("tokenLogs.filter")}>
            <Select
              value={nodeFilter}
              onChange={(e) => setNodeFilter(e.target.value)}
              aria-label={t("tokenLogs.filter")}
            >
              <option value="">{t("tokenLogs.allNodes")}</option>
              {(cluster.data?.peers ?? []).map((p) => (
                <option key={p.node_id} value={p.node_id}>
                  {p.node?.hostname || p.short_id}
                </option>
              ))}
            </Select>
          </Field>
          <Field label={t("tokenLogs.from")}>
            <Input type="datetime-local" value={fromFilter} onChange={(e) => setFromFilter(e.target.value)} />
          </Field>
          <Field label={t("tokenLogs.to")}>
            <Input type="datetime-local" value={toFilter} onChange={(e) => setToFilter(e.target.value)} />
          </Field>
        </div>
      </Panel>

      {log.isError && <ErrorState error={log.error} onRetry={() => void log.refetch()} />}
      {log.isLoading && <Skeleton className="h-64 w-full" />}

      {data && (
        <div className="space-y-5">
          <div className="grid gap-5 lg:grid-cols-2">
            <Tally
              title={t("tokenLogs.byToken")}
              icon={<KeyRound className="h-3 w-3" />}
              rows={byToken}
              emptyLabel={t("tokenLogs.noRequests")}
            />
            <Tally
              title={t("tokenLogs.byNode")}
              icon={<Server className="h-3 w-3" />}
              rows={byNode}
              emptyLabel={t("tokenLogs.noHops")}
            />
          </div>

          <Panel>
            <PanelHeader
              title={t("tokenLogs.count", { count: num(data.total) })}
              subtitle={t("tokenLogs.sqliteDetail")}
              actions={
                data.errors ? (
                  <Badge tone="rose">
                    {t("tokenLogs.errors", { count: num(data.errors) })}
                  </Badge>
                ) : undefined
              }
            />
            {data.entries.length === 0 ? (
              <Empty icon={<Radio className="h-5 w-5" />} title={t("tokenLogs.empty")}>
                {t("tokenLogs.emptyDetail")}
              </Empty>
            ) : (
              <ScrollX>
                <table className="w-full min-w-[64rem] border-collapse text-[0.82rem]">
                  <thead>
                    <tr className="border-b border-line text-left">
                      {[
                        t("tokenLogs.time"),
                        t("tokenLogs.token"),
                        t("tokenLogs.model"),
                        t("tokenLogs.path"),
                        t("tokenLogs.tokens"),
                        t("tokenLogs.duration"),
                        t("tokenLogs.performance"),
                        t("tokenLogs.status"),
                        "",
                      ].map((h) => (
                        <th key={h} className="px-4 py-2.5 font-medium text-muted">
                          {h}
                        </th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {data.entries.map((e) => (
                      <Fragment key={e.id}>
                        <tr className="border-b border-line hover:bg-raised">
                          <td className="whitespace-nowrap px-4 py-2.5">
                            <Mono className="text-fg">{clock(e.received_at)}</Mono>
                            <span className="ml-2 text-[0.7rem] text-faint">{since(e.received_at)}</span>
                          </td>
                          <td className="px-4 py-2.5">
                            <span className="text-fg">{e.api_key_label}</span>
                            <Mono className="ml-1.5 text-faint">{e.api_key_id}</Mono>
                            {e.api_key_node_id && (
                              <span className="ml-1.5 text-[0.7rem] text-faint">· {name(e.api_key_node_id)}</span>
                            )}
                          </td>
                          <td className="px-4 py-2.5">
                            <Mono className="text-muted">{e.model}</Mono>
                          </td>
                          <td className="px-4 py-2.5">
                            <HopChain entry={e} name={name} />
                          </td>
                          <td className="whitespace-nowrap px-4 py-2.5">
                            <Mono className="text-muted">
                              {e.prompt_tokens ?? "-"} → {e.completion_tokens ?? "-"}
                            </Mono>
                          </td>
                          <td className="whitespace-nowrap px-4 py-2.5">
                            <Mono className="text-muted">{ms(e.duration_ms)}</Mono>
                          </td>
                          <td className="px-4 py-2.5">
                            {e.timings ? (
                              <div className="space-y-0.5 font-mono text-[0.68rem] text-muted">
                                <div>{t("tokenLogs.ttft", { value: ms(e.timings.ttft_ms) })}</div>
                                <div>{t("tokenLogs.prefill", { value: rateText(e.timings.prefill_tokens_per_sec, t("tokenLogs.rateUnavailable"), t("tokenLogs.tokensPerSecondUnit")) })}</div>
                                <div>{t("tokenLogs.generation", { value: rateText(e.timings.generation_tokens_per_sec, t("tokenLogs.rateUnavailable"), t("tokenLogs.tokensPerSecondUnit")) })}</div>
                              </div>
                            ) : <span className="text-faint">-</span>}
                          </td>
                          <td className="px-4 py-2.5">
                            <Badge tone={statusTone(e.status)}>{statusLabel(e.status, t)}</Badge>
                          </td>
                          <td className="px-4 py-2.5">
                            <span className="text-[0.72rem] text-faint">+</span>
                          </td>
                        </tr>
                        <tr className="border-b border-line last:border-0">
                          <td colSpan={9} className="px-4 py-2.5">
                            <AuditDetails entry={e} name={name} />
                          </td>
                        </tr>
                      </Fragment>
                    ))}
                  </tbody>
                </table>
              </ScrollX>
            )}
          </Panel>
        </div>
      )}
    </>
  );
}
