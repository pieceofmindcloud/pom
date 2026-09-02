/**
 * Request audit: a dense, full-viewport table plus server-aggregated stats.
 * The list remains paged so opening Logs is fast even when the SQLite trail is
 * large. Selecting a row moves its complete, redacted object into the side
 * inspector.
 */
import {
  Bar,
  BarChart,
  CartesianGrid,
  Cell,
  Legend,
  Pie,
  PieChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import { Activity, BarChart3, ClipboardList, Loader2, Search, Timer } from "lucide-react";
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Input,
  Mono,
  ScrollX,
  Select,
  Skeleton,
  type Tone,
} from "../components/ui";
import { RequestInspector } from "../components/RequestInspector";
import { clock, ms, num, since } from "../lib/format";
import { useCluster, useRequestLog, useRequestStats } from "../lib/queries";
import type { RequestLogEntry } from "../lib/types";
import { useI18n } from "../i18n/provider";

const PAGE_SIZE = 50;
const CHART_COLORS = ["#8b7dff", "#58d5c5", "#f3b85f", "#e783a4", "#7eb4ff"];

type Filters = {
  node_id?: string;
  api_key?: string;
  prompt?: string;
  from?: string;
  to?: string;
};

function toFilters(values: {
  node: string;
  apiKey: string;
  prompt: string;
  from: string;
  to: string;
}): Filters {
  return {
    node_id: values.node || undefined,
    api_key: values.apiKey.trim() || undefined,
    prompt: values.prompt.trim() || undefined,
    from: values.from ? new Date(values.from).toISOString() : undefined,
    to: values.to ? new Date(values.to).toISOString() : undefined,
  };
}

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
        <span className="font-mono text-[0.66rem] text-fg">{name(entry.entry_node_id)}</span>
        <span className="text-[0.7rem] text-faint">· {t("tokenLogs.noPipeline")}</span>
      </span>
    );
  }
  return (
    <span className="flex flex-wrap items-center gap-1">
      {entry.hops.map((h, i) => (
        <span key={h + "-" + i} className="flex items-center gap-1">
          {i > 0 && <span className="text-faint">→</span>}
          <span className="font-mono text-[0.66rem] text-fg">{name(h)}</span>
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

function rateText(value: number | null | undefined, unavailable: string, unit: string) {
  return value == null ? unavailable : `${num(value, 1)} ${unit}`;
}

function isInteractiveRowTarget(target: EventTarget | null) {
  return target instanceof Element && target.closest("button, a, input, select, textarea, [role='button']") !== null;
}

function StatCard({ label, value, detail, icon }: { label: string; value: string; detail?: string; icon: ReactNode }) {
  return (
    <div className="min-w-0 border border-line bg-surface/65 px-3 py-3">
      <div className="flex items-center gap-1.5 text-[0.62rem] font-semibold uppercase tracking-[0.12em] text-faint">
        {icon}
        <span className="truncate">{label}</span>
      </div>
      <p className="mt-2 truncate font-display text-xl font-semibold tracking-tight text-fg">{value}</p>
      {detail && <p className="mt-1 truncate text-[0.68rem] text-muted">{detail}</p>}
    </div>
  );
}

function ChartFrame({ title, subtitle, children }: { title: string; subtitle?: string; children: ReactNode }) {
  return (
    <section className="min-w-0 border border-line bg-surface/45 p-3">
      <div className="mb-3 min-w-0">
        <h3 className="truncate text-[0.78rem] font-semibold text-fg">{title}</h3>
        {subtitle && <p className="mt-0.5 truncate text-[0.68rem] text-faint">{subtitle}</p>}
      </div>
      <div className="min-w-0">{children}</div>
    </section>
  );
}

function chartTooltipStyle() {
  return {
    backgroundColor: "rgb(var(--raised))",
    border: "1px solid rgb(var(--line))",
    borderRadius: "8px",
    color: "rgb(var(--fg))",
    fontSize: "11px",
  };
}

function bytesText(value: number) {
  if (value < 1024) return `${num(value)} B`;
  if (value < 1024 * 1024) return `${num(value / 1024, 1)} KB`;
  return `${num(value / (1024 * 1024), 1)} MB`;
}

function StatsView({ filters, name }: { filters: Filters; name: (id: string) => string }) {
  const { t } = useI18n();
  const stats = useRequestStats(filters, true);
  const data = stats.data;
  if (stats.isLoading) return <Skeleton className="m-4 h-[28rem]" />;
  if (stats.isError) return <ErrorState error={stats.error} onRetry={() => void stats.refetch()} />;
  if (!data) return null;

  const tokenRows = Object.entries(data.by_token)
    .sort((a, b) => b[1] - a[1])
    .slice(0, 12)
    .map(([token, requests]) => ({ token, requests }));
  const nodeRows = Object.entries(data.by_node)
    .sort((a, b) => b[1] - a[1])
    .slice(0, 12)
    .map(([node, requests]) => ({ node: name(node), requests }));
  const dayRows = Object.entries(data.by_day)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([day, usage]) => ({ day: day.slice(5), input: usage.input_tokens, output: usage.output_tokens }));
  const modelRows = Object.entries(data.by_model)
    .sort((a, b) => (b[1].average_tokens_per_sec ?? 0) - (a[1].average_tokens_per_sec ?? 0))
    .map(([model, modelStats]) => ({
      model,
      tokPerSec: modelStats.average_tokens_per_sec ?? 0,
      inference: modelStats.average_inference_ms ?? 0,
      requests: modelStats.requests,
    }));
  const peerRows = Object.entries(data.peer_consumption)
    .sort((a, b) => b[1].bytes_sent + b[1].bytes_received - (a[1].bytes_sent + a[1].bytes_received))
    .slice(0, 12)
    .map(([peer, peerStats]) => ({
      peer: name(peer),
      bytesSent: peerStats.bytes_sent,
      bytesReceived: peerStats.bytes_received,
      networkMs: peerStats.network_ms,
    }));
  const stateRows = Object.entries(data.terminal_states).map(([state, requests]) => ({
    state: statusLabel(state, t),
    requests,
  }));
  const cacheRate = data.cache.input_tokens > 0
    ? `${num((data.cache.cached_tokens / data.cache.input_tokens) * 100, 1)}%`
    : t("tokenLogs.unavailable");

  return (
    <div className="min-h-0 flex-1 overflow-auto p-3 sm:p-4">
      <div className="grid min-w-0 gap-2 sm:grid-cols-2 xl:grid-cols-5">
        <StatCard label={t("tokenLogs.totalInputTokens")} value={num(data.input_tokens)} detail={t("tokenLogs.inputTokensDetail")} icon={<Activity className="h-3 w-3" />} />
        <StatCard label={t("tokenLogs.totalOutputTokens")} value={num(data.output_tokens)} detail={t("tokenLogs.outputTokensDetail")} icon={<Activity className="h-3 w-3" />} />
        <StatCard label={t("tokenLogs.averageInference")} value={data.average_inference_ms == null ? t("tokenLogs.unavailable") : ms(data.average_inference_ms)} icon={<Timer className="h-3 w-3" />} />
        <StatCard label={t("tokenLogs.averageTtft")} value={data.average_ttft_ms == null ? t("tokenLogs.unavailable") : ms(data.average_ttft_ms)} icon={<Timer className="h-3 w-3" />} />
        <StatCard label={t("tokenLogs.prefixCache") } value={cacheRate} detail={t("tokenLogs.prefixCacheDetail", { hits: num(data.cache.hit_requests), eligible: num(data.cache.eligible_requests) })} icon={<Activity className="h-3 w-3" />} />
      </div>

      {data.total === 0 ? (
        <Empty icon={<BarChart3 className="h-5 w-5" />} title={t("tokenLogs.statsEmpty")}>
          {t("tokenLogs.statsEmptyDetail")}
        </Empty>
      ) : (
        <div className="mt-3 grid min-w-0 gap-3 xl:grid-cols-2">
          <ChartFrame title={t("tokenLogs.statsByDay")} subtitle={t("tokenLogs.statsTokensSubtitle")}>
            <ResponsiveContainer width="100%" height={240} minWidth={0}>
              <BarChart data={dayRows} margin={{ top: 4, right: 8, left: -18, bottom: 0 }}>
                <CartesianGrid stroke="rgb(var(--line))" strokeDasharray="3 3" vertical={false} />
                <XAxis dataKey="day" tick={{ fill: "rgb(var(--muted))", fontSize: 10 }} axisLine={false} tickLine={false} />
                <YAxis tick={{ fill: "rgb(var(--muted))", fontSize: 10 }} axisLine={false} tickLine={false} width={32} />
                <Tooltip contentStyle={chartTooltipStyle()} />
                <Legend wrapperStyle={{ fontSize: "10px" }} />
                <Bar dataKey="input" name={t("tokenLogs.inputTokens")} fill={CHART_COLORS[0]} radius={[3, 3, 0, 0]} />
                <Bar dataKey="output" name={t("tokenLogs.outputTokens")} fill={CHART_COLORS[1]} radius={[3, 3, 0, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </ChartFrame>
          <ChartFrame title={t("tokenLogs.statsByToken")} subtitle={t("tokenLogs.requestsSubtitle")}>
            <ResponsiveContainer width="100%" height={240} minWidth={0}>
              <BarChart data={tokenRows} layout="vertical" margin={{ top: 4, right: 10, left: 8, bottom: 0 }}>
                <CartesianGrid stroke="rgb(var(--line))" strokeDasharray="3 3" horizontal={false} />
                <XAxis type="number" tick={{ fill: "rgb(var(--muted))", fontSize: 10 }} axisLine={false} tickLine={false} />
                <YAxis dataKey="token" type="category" width={84} tick={{ fill: "rgb(var(--muted))", fontSize: 10 }} axisLine={false} tickLine={false} tickFormatter={(value) => String(value).slice(0, 14)} />
                <Tooltip contentStyle={chartTooltipStyle()} />
                <Bar dataKey="requests" name={t("tokenLogs.requests")} fill={CHART_COLORS[0]} radius={[0, 3, 3, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </ChartFrame>
          <ChartFrame title={t("tokenLogs.statsByNode")} subtitle={t("tokenLogs.requestsSubtitle")}>
            <ResponsiveContainer width="100%" height={240} minWidth={0}>
              <BarChart data={nodeRows} layout="vertical" margin={{ top: 4, right: 10, left: 8, bottom: 0 }}>
                <CartesianGrid stroke="rgb(var(--line))" strokeDasharray="3 3" horizontal={false} />
                <XAxis type="number" tick={{ fill: "rgb(var(--muted))", fontSize: 10 }} axisLine={false} tickLine={false} />
                <YAxis dataKey="node" type="category" width={84} tick={{ fill: "rgb(var(--muted))", fontSize: 10 }} axisLine={false} tickLine={false} />
                <Tooltip contentStyle={chartTooltipStyle()} />
                <Bar dataKey="requests" name={t("tokenLogs.requests")} fill={CHART_COLORS[1]} radius={[0, 3, 3, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </ChartFrame>
          <ChartFrame title={t("tokenLogs.statsByModel")} subtitle={t("tokenLogs.modelSpeedSubtitle")}>
            <ResponsiveContainer width="100%" height={240} minWidth={0}>
              <BarChart data={modelRows} margin={{ top: 4, right: 8, left: -18, bottom: 0 }}>
                <CartesianGrid stroke="rgb(var(--line))" strokeDasharray="3 3" vertical={false} />
                <XAxis dataKey="model" tick={{ fill: "rgb(var(--muted))", fontSize: 10 }} axisLine={false} tickLine={false} tickFormatter={(value) => String(value).slice(0, 12)} />
                <YAxis tick={{ fill: "rgb(var(--muted))", fontSize: 10 }} axisLine={false} tickLine={false} width={32} />
                <Tooltip contentStyle={chartTooltipStyle()} formatter={(value) => `${num(Number(value), 1)} ${t("tokenLogs.tokensPerSecondUnit")}`} />
                <Bar dataKey="tokPerSec" name={t("tokenLogs.averageTokPerSec")} fill={CHART_COLORS[2]} radius={[3, 3, 0, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </ChartFrame>
          <ChartFrame title={t("tokenLogs.statsPeerConsumption")} subtitle={t("tokenLogs.peerConsumptionSubtitle")}>
            <ResponsiveContainer width="100%" height={240} minWidth={0}>
              <BarChart data={peerRows} margin={{ top: 4, right: 8, left: -18, bottom: 0 }}>
                <CartesianGrid stroke="rgb(var(--line))" strokeDasharray="3 3" vertical={false} />
                <XAxis dataKey="peer" tick={{ fill: "rgb(var(--muted))", fontSize: 10 }} axisLine={false} tickLine={false} />
                <YAxis tick={{ fill: "rgb(var(--muted))", fontSize: 10 }} axisLine={false} tickLine={false} width={42} tickFormatter={bytesText} />
                <Tooltip contentStyle={chartTooltipStyle()} formatter={(value) => bytesText(Number(value))} />
                <Legend wrapperStyle={{ fontSize: "10px" }} />
                <Bar dataKey="bytesSent" name={t("tokenLogs.bytesSent")} fill={CHART_COLORS[3]} radius={[3, 3, 0, 0]} />
                <Bar dataKey="bytesReceived" name={t("tokenLogs.bytesReceived")} fill={CHART_COLORS[4]} radius={[3, 3, 0, 0]} />
              </BarChart>
            </ResponsiveContainer>
            <div className="mt-2 grid gap-1 border-t border-line pt-2 text-[0.66rem] text-muted">
              {peerRows.slice(0, 5).map((peer) => (
                <div key={peer.peer} className="flex min-w-0 justify-between gap-3">
                  <span className="truncate">{peer.peer}</span>
                  <Mono className="shrink-0 text-faint">{num(peer.networkMs, 1)} ms</Mono>
                </div>
              ))}
            </div>
          </ChartFrame>
          <ChartFrame title={t("tokenLogs.statsTerminalStates")} subtitle={t("tokenLogs.terminalStatesSubtitle")}>
            <div className="flex min-w-0 items-center justify-center gap-3">
              <ResponsiveContainer width="55%" height={220} minWidth={0}>
                <PieChart>
                  <Pie data={stateRows} dataKey="requests" nameKey="state" innerRadius="54%" outerRadius="78%" paddingAngle={3}>
                    {stateRows.map((entry, index) => <Cell key={entry.state} fill={CHART_COLORS[index % CHART_COLORS.length]} />)}
                  </Pie>
                  <Tooltip contentStyle={chartTooltipStyle()} />
                </PieChart>
              </ResponsiveContainer>
              <div className="min-w-0 space-y-1 text-[0.68rem] text-muted">
                {stateRows.map((entry, index) => (
                  <div key={entry.state} className="flex items-center gap-2">
                    <span className="h-2 w-2 shrink-0 rounded-full" style={{ backgroundColor: CHART_COLORS[index % CHART_COLORS.length] }} />
                    <span className="truncate">{entry.state}</span>
                    <Mono className="text-faint">{num(entry.requests)}</Mono>
                  </div>
                ))}
              </div>
            </div>
          </ChartFrame>
        </div>
      )}
    </div>
  );
}

function RequestsView({ filters, name, selected, setSelected }: {
  filters: Filters;
  name: (id: string) => string;
  selected: RequestLogEntry | null;
  setSelected: (entry: RequestLogEntry | null) => void;
}) {
  const { t } = useI18n();
  const [page, setPage] = useState(0);
  const [entries, setEntries] = useState<RequestLogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const sentinelRef = useRef<HTMLDivElement>(null);
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const loadingMore = useRef(false);
  const query = useRequestLog({ ...filters, limit: PAGE_SIZE, offset: page * PAGE_SIZE });
  const filterKey = JSON.stringify(filters);

  useEffect(() => {
    setPage(0);
    setEntries([]);
    setTotal(0);
    setSelected(null);
    loadingMore.current = false;
  }, [filterKey, setSelected]);

  useEffect(() => {
    if (!query.data) return;
    setTotal(query.data.total);
    setEntries((current) => {
      const next = page === 0
        ? query.data.entries
        : (() => {
            const seen = new Set(current.map((entry) => entry.id));
            return [...current, ...query.data.entries.filter((entry) => !seen.has(entry.id))];
          })();
      if (current.length === next.length && current.every((entry, index) => entry.id === next[index]?.id)) {
        return current;
      }
      return next;
    });
    loadingMore.current = false;
  }, [page, query.data]);

  // An empty response is authoritative even if a concurrent count was taken
  // before the last row landed. It also prevents a visible sentinel from
  // repeatedly asking for empty pages while it remains in the viewport.
  const hasMore = entries.length < total && (page === 0 || (query.data?.entries.length ?? 0) > 0);
  const loadMore = useCallback(() => {
    if (!hasMore || loadingMore.current || query.isFetching) return;
    loadingMore.current = true;
    setPage((current) => current + 1);
  }, [hasMore, query.isFetching]);

  useEffect(() => {
    const sentinel = sentinelRef.current;
    if (!sentinel || typeof IntersectionObserver === "undefined") return;
    const observer = new IntersectionObserver((observations) => {
      if (observations.some((observation) => observation.isIntersecting)) loadMore();
    }, { root: scrollContainerRef.current, rootMargin: "240px 0px" });
    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [loadMore]);

  if (query.isLoading && entries.length === 0) return <Skeleton className="m-4 h-[26rem]" />;
  if (query.isError && entries.length === 0) return <ErrorState error={query.error} onRetry={() => void query.refetch()} />;

  return (
    <div className={`flex min-h-0 min-w-0 flex-1 flex-col ${selected ? "lg:grid lg:grid-cols-[minmax(0,1fr)_minmax(19rem,30rem)]" : ""}`}>
      <div ref={scrollContainerRef} className="min-h-0 min-w-0 flex-1 overflow-auto">
        <div className="flex items-center justify-between gap-3 border-b border-line px-3 py-2 text-[0.68rem] text-muted sm:px-4">
          <span>{t("tokenLogs.count", { count: num(total) })}</span>
          <div className="flex min-w-[15rem] shrink-0 items-center justify-end gap-2">
            <span className="flex min-w-[9rem] items-center justify-end gap-1 text-faint" aria-live="polite">
              {query.isFetching && entries.length > 0 ? <><Loader2 className="h-3 w-3 animate-spin" />{t("tokenLogs.loadingMore")}</> : null}
            </span>
            {query.data?.errors ? <span data-testid="logs-errors-badge"><Badge tone="rose">{t("tokenLogs.errors", { count: num(query.data.errors) })}</Badge></span> : null}
          </div>
        </div>
        {entries.length === 0 ? (
          <Empty icon={<ClipboardList className="h-5 w-5" />} title={t("tokenLogs.empty")}>
            {t("tokenLogs.emptyDetail")}
          </Empty>
        ) : (
          <ScrollX>
            <table className="w-full min-w-[60rem] border-collapse text-[0.7rem]">
              <thead>
                <tr className="border-b border-line text-left">
                  {[t("tokenLogs.time"), t("tokenLogs.token"), t("tokenLogs.model"), t("tokenLogs.path"), t("tokenLogs.tokens"), t("tokenLogs.duration"), t("tokenLogs.performance"), t("tokenLogs.status"), ""].map((heading) => (
                    <th key={heading} className="whitespace-nowrap px-3 py-2 font-medium text-muted">{heading}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {entries.map((entry) => (
                  <tr
                    key={entry.id}
                    data-testid={`logs-row-${entry.id}`}
                    aria-selected={selected?.id === entry.id}
                    tabIndex={0}
                    onClick={(event) => {
                      if (!isInteractiveRowTarget(event.target)) setSelected(entry);
                    }}
                    onKeyDown={(event) => {
                      if ((event.key === "Enter" || event.key === " ") && event.target === event.currentTarget) {
                        event.preventDefault();
                        setSelected(entry);
                      }
                    }}
                    className={`cursor-pointer border-b border-line transition-colors ${selected?.id === entry.id ? "bg-iris/20 ring-1 ring-inset ring-iris/60 hover:bg-iris/25" : "hover:bg-raised"}`}
                  >
                    <td className="whitespace-nowrap px-3 py-1.5">
                      <Mono className="text-fg">{clock(entry.received_at)}</Mono>
                      <span className="ml-1.5 text-[0.62rem] text-faint">{since(entry.received_at)}</span>
                    </td>
                    <td className="max-w-[13rem] px-3 py-1.5">
                      <span className="block truncate text-fg">{entry.api_key_label}</span>
                      <Mono className="text-[0.62rem] text-faint">{entry.api_key_id}</Mono>
                    </td>
                    <td className="max-w-[10rem] truncate px-3 py-1.5"><Mono className="text-muted">{entry.model}</Mono></td>
                    <td className="max-w-[14rem] px-3 py-1.5"><HopChain entry={entry} name={name} /></td>
                    <td className="whitespace-nowrap px-3 py-1.5"><Mono className="text-muted">{entry.prompt_tokens ?? "-"} → {entry.completion_tokens ?? "-"}</Mono></td>
                    <td className="whitespace-nowrap px-3 py-1.5"><Mono className="text-muted">{ms(entry.duration_ms)}</Mono></td>
                    <td className="px-3 py-1.5">
                      {entry.timings ? (
                        <div className="space-y-0.5 font-mono text-[0.62rem] text-muted">
                          <div>{t("tokenLogs.ttft", { value: ms(entry.timings.ttft_ms) })}</div>
                          <div>{t("tokenLogs.generation", { value: rateText(entry.timings.generation_tokens_per_sec, t("tokenLogs.rateUnavailable"), t("tokenLogs.tokensPerSecondUnit")) })}</div>
                        </div>
                      ) : <span className="text-faint">-</span>}
                    </td>
                    <td className="whitespace-nowrap px-3 py-1.5"><Badge tone={statusTone(entry.status)}>{statusLabel(entry.status, t)}</Badge></td>
                    <td className="px-3 py-1.5">
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-6 px-1.5 text-[0.65rem]"
                        aria-label={t("tokenLogs.inspect")}
                        onClick={() => setSelected(entry)}
                      >
                        <Search className="h-3.5 w-3.5" />
                      </Button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </ScrollX>
        )}
        <div ref={sentinelRef} data-testid="logs-load-more" data-page={page} className="flex min-h-12 items-center justify-center px-3 py-3 text-[0.68rem] text-faint">
          {query.isFetching && entries.length > 0 ? null : hasMore ? t("tokenLogs.scrollForMore") : entries.length > 0 ? t("tokenLogs.allLoaded") : null}
        </div>
      </div>
      {selected && <RequestInspector entry={selected} name={name} onClose={() => setSelected(null)} />}
    </div>
  );
}

export function Logs() {
  const { t } = useI18n();
  const cluster = useCluster();
  const name = useNodeNames();
  const [tab, setTab] = useState<"requests" | "stats">("requests");
  const [selected, setSelected] = useState<RequestLogEntry | null>(null);
  const [draft, setDraft] = useState({ node: "", apiKey: "", prompt: "", from: "", to: "" });
  const [filters, setFilters] = useState<Filters>({});

  const search = () => {
    setSelected(null);
    setFilters(toFilters(draft));
  };

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <header className="shrink-0 border-b border-line bg-surface/75">
        <div className="flex min-w-0 flex-wrap items-center justify-between gap-3 px-3 py-3 sm:px-4">
          <div className="min-w-0">
            <h1 className="font-display text-lg font-semibold tracking-tight text-fg">{t("tokenLogs.title")}</h1>
            <p className="mt-0.5 truncate text-[0.7rem] text-muted">{t("tokenLogs.subtitle")}</p>
          </div>
          <div role="tablist" aria-label={t("tokenLogs.screenTabs")} className="flex shrink-0 gap-1 rounded-lg bg-ink p-1">
            <button type="button" role="tab" aria-selected={tab === "requests"} className={tab === "requests" ? "rounded-md bg-raised px-2.5 py-1.5 text-[0.68rem] font-semibold text-fg" : "rounded-md px-2.5 py-1.5 text-[0.68rem] font-semibold text-faint hover:text-fg"} onClick={() => { setTab("requests"); setSelected(null); }}>
              {t("tokenLogs.requestsTab")}
            </button>
            <button type="button" role="tab" aria-selected={tab === "stats"} className={tab === "stats" ? "rounded-md bg-raised px-2.5 py-1.5 text-[0.68rem] font-semibold text-fg" : "rounded-md px-2.5 py-1.5 text-[0.68rem] font-semibold text-faint hover:text-fg"} onClick={() => { setTab("stats"); setSelected(null); }}>
              {t("tokenLogs.statsTab")}
            </button>
          </div>
        </div>
        <div className="flex min-w-0 flex-wrap items-center gap-2 border-t border-line px-3 py-2 sm:px-4">
          <div className="min-w-[10rem] flex-1 sm:max-w-[15rem]">
            <label className="sr-only" htmlFor="logs-api-key">{t("tokenLogs.apiKey")}</label>
            <Input id="logs-api-key" compact value={draft.apiKey} onChange={(event) => setDraft((current) => ({ ...current, apiKey: event.target.value }))} placeholder={t("tokenLogs.apiKeyPlaceholder")} />
          </div>
          <div className="min-w-[10rem] flex-1 sm:max-w-[15rem]">
            <label className="sr-only" htmlFor="logs-prompt">{t("tokenLogs.promptSearch")}</label>
            <Input id="logs-prompt" compact value={draft.prompt} onChange={(event) => setDraft((current) => ({ ...current, prompt: event.target.value }))} placeholder={t("tokenLogs.promptPlaceholder")} />
          </div>
          <div className="min-w-[8rem] flex-1 sm:max-w-[13rem]">
            <label className="sr-only" htmlFor="logs-node">{t("tokenLogs.filter")}</label>
            <Select id="logs-node" compact value={draft.node} onChange={(event) => setDraft((current) => ({ ...current, node: event.target.value }))}>
              <option value="">{t("tokenLogs.allNodes")}</option>
              {(cluster.data?.peers ?? []).map((peer) => <option key={peer.node_id} value={peer.node_id}>{peer.node?.hostname || peer.short_id}</option>)}
            </Select>
          </div>
          <div className="w-[10.5rem] flex-1 sm:flex-none">
            <label className="sr-only" htmlFor="logs-from">{t("tokenLogs.from")}</label>
            <Input id="logs-from" compact type="datetime-local" value={draft.from} onChange={(event) => setDraft((current) => ({ ...current, from: event.target.value }))} />
          </div>
          <div className="w-[10.5rem] flex-1 sm:flex-none">
            <label className="sr-only" htmlFor="logs-to">{t("tokenLogs.to")}</label>
            <Input id="logs-to" compact type="datetime-local" value={draft.to} onChange={(event) => setDraft((current) => ({ ...current, to: event.target.value }))} />
          </div>
          <Button size="sm" variant="primary" icon={<Search className="h-3.5 w-3.5" />} onClick={search}>{t("tokenLogs.search")}</Button>
        </div>
      </header>
      {tab === "stats" ? <StatsView filters={filters} name={name} /> : <RequestsView filters={filters} name={name} selected={selected} setSelected={setSelected} />}
    </div>
  );
}
