/**
 * This node's token budget, as the admins set it in the logs plugin's Quota
 * screen: each plan that binds this machine with what it spent of the current
 * window, when the window turns over, the session-reset credits the admins
 * granted (spending one ends the running session windows) and a request for
 * more budget. The node asks `/api/ui/quota`, which reaches the plugin of an
 * admin; see `crates/node/src/backend/quota.rs`.
 */
import { Ban, CheckCircle2, Gauge, RotateCcw, Send, TimerReset } from "lucide-react";
import { useEffect, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Badge, Button, Empty, ErrorState, Input, PageHeader, Panel, PanelHeader, Skeleton } from "../components/ui";
import { api } from "../lib/api";
import { keys, useMyQuota } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";

interface QuotaLimit {
  assignment_id: string;
  plan: string;
  period: "daily" | "weekly" | "monthly" | "session";
  session_minutes: number;
  scope: "node" | "team" | "project" | "everyone" | "default";
  subject: string;
  mode: "shared" | "member";
  counter: string;
  active: boolean;
  resets_at: string | null;
  used_input: number;
  used_output: number;
  input_limit: number;
  output_limit: number;
  extra_input: number;
  extra_output: number;
  exhausted: boolean;
}

interface QuotaStatus {
  available: boolean;
  configured: boolean;
  /** A machine that runs the logs plugin did not answer yet. */
  pending?: boolean;
  blocked?: boolean;
  limits?: QuotaLimit[];
  reset_credits?: { available: number; used: number };
  resettable?: boolean;
  grants?: Array<{ id: string; extra_input: number; extra_output: number; expires_at: string; note: string }>;
  pending_request?: { id: string; at: string; note: string } | null;
}

const KEY = keys.myQuota;

type T = ReturnType<typeof useI18n>["t"];

function compact(value: number, locale: string) {
  return new Intl.NumberFormat(locale, { notation: "compact", maximumFractionDigits: 1 }).format(value);
}

function useNow() {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(timer);
  }, []);
  return now;
}

function until(iso: string | null, now: number) {
  if (!iso) return "—";
  const minutes = Math.max(0, Math.round((Date.parse(iso) - now) / 60_000));
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor((minutes % 1440) / 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes % 60}m`;
  return `${minutes % 60}m`;
}

function Meter({ label, used, limit, extra }: { label: string; used: number; limit: number; extra: number }) {
  const { t, locale } = useI18n();
  const share = limit > 0 ? Math.min(1, used / limit) : 0;
  const over = limit > 0 && used >= limit;
  return (
    <div className="min-w-0">
      <div className="flex items-baseline justify-between gap-2 text-[0.74rem]">
        <span className="text-muted">{label}</span>
        <span className="font-mono tabular-nums text-fg">
          {compact(used, locale)}
          <span className="text-faint"> / {limit > 0 ? compact(limit, locale) : t("budget.unlimited")}</span>
          {limit > 0 && <span className={over ? "ml-2 text-rose" : "ml-2 text-muted"}>{Math.round((used / limit) * 100)}%</span>}
        </span>
      </div>
      <div className="mt-1.5 h-2 overflow-hidden rounded-full bg-line/60" role="progressbar" aria-label={label} aria-valuemin={0} aria-valuemax={limit || 1} aria-valuenow={Math.min(used, limit || used)}>
        <span
          className={`block h-full rounded-full transition-[width] duration-700 ${over ? "bg-rose" : share >= 0.8 ? "bg-amber" : "bg-iris"}`}
          style={{ width: `${share * 100}%` }}
        />
      </div>
      {extra > 0 && <p className="mt-1 text-[0.68rem] text-aqua">{t("budget.includesGrant", { amount: compact(extra, locale) })}</p>}
    </div>
  );
}

function periodText(t: T, limit: QuotaLimit) {
  switch (limit.period) {
    case "session":
      return t("budget.period.session", { hours: Math.round((limit.session_minutes / 60) * 10) / 10 });
    case "daily":
      return t("budget.period.daily");
    case "weekly":
      return t("budget.period.weekly");
    default:
      return t("budget.period.monthly");
  }
}

function scopeText(t: T, limit: QuotaLimit) {
  switch (limit.scope) {
    case "team":
      return limit.mode === "member" ? t("budget.scope.teamMember", { team: limit.subject }) : t("budget.scope.team", { team: limit.subject });
    case "everyone":
      return limit.mode === "member" ? t("budget.scope.everyoneMember") : t("budget.scope.everyone");
    case "default":
      return t("budget.scope.default");
    default:
      return t("budget.scope.node");
  }
}

export function MyBudget() {
  const { t, locale } = useI18n();
  const toast = useToast();
  const client = useQueryClient();
  const now = useNow();
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState<"reset" | "request" | null>(null);
  const quota = useMyQuota<QuotaStatus>();
  const data = quota.data;

  async function act(kind: "reset" | "request") {
    setBusy(kind);
    try {
      const reply = kind === "reset"
        ? await api.post<QuotaStatus>("/api/ui/quota/reset")
        : await api.post<QuotaStatus>("/api/ui/quota/request", { note: note.trim() });
      client.setQueryData(KEY, reply);
      toast.success(kind === "reset" ? t("budget.resetDone") : t("budget.requestSent"));
      if (kind === "request") setNote("");
    } catch (error) {
      toast.fromError(kind === "reset" ? t("budget.resetFailed") : t("budget.requestFailed"), error);
    } finally {
      setBusy(null);
    }
  }

  const limits = data?.limits ?? [];
  const credits = data?.reset_credits ?? { available: 0, used: 0 };
  return (
    <div className="grid gap-4">
      <PageHeader
        title={t("budget.title")}
        subtitle={t("budget.subtitle")}
        actions={
          data?.configured && limits.length > 0 ? (
            data.blocked ? (
              <Badge tone="rose"><Ban className="mr-1 inline h-3 w-3" />{t("budget.blocked")}</Badge>
            ) : (
              <Badge tone="aqua"><CheckCircle2 className="mr-1 inline h-3 w-3" />{t("budget.within")}</Badge>
            )
          ) : undefined
        }
      />
      {quota.isLoading || (data?.pending && !data.configured) ? (
        <Skeleton className="h-40" />
      ) : quota.isError ? (
        <ErrorState title={t("budget.loadFailed")} error={quota.error} onRetry={() => void quota.refetch()} />
      ) : !data?.available ? (
        <Empty icon={<Gauge className="h-5 w-5" />} title={t("budget.unavailable")}>{t("budget.unavailableHint")}</Empty>
      ) : !data.configured || limits.length === 0 ? (
        <Empty icon={<Gauge className="h-5 w-5" />} title={t("budget.none")}>{t("budget.noneHint")}</Empty>
      ) : (
        <>
          <div className="grid gap-3 md:grid-cols-2">
            {limits.map((limit) => (
              <Panel key={`${limit.assignment_id}:${limit.counter}`} className={limit.exhausted ? "border-rose/40" : undefined}>
                <PanelHeader title={limit.plan} subtitle={`${periodText(t, limit)} · ${scopeText(t, limit)}`} />
                <div className="grid gap-3 px-4 pb-4">
                  <Meter label={t("budget.input")} used={limit.used_input} limit={limit.input_limit} extra={limit.extra_input} />
                  <Meter label={t("budget.output")} used={limit.used_output} limit={limit.output_limit} extra={limit.extra_output} />
                  <p className="flex items-center gap-1.5 text-[0.74rem] text-muted">
                    <TimerReset className="h-3.5 w-3.5" />
                    {limit.active
                      ? t("budget.resetsIn", {
                          time: until(limit.resets_at, now),
                          date: limit.resets_at ? new Date(limit.resets_at).toLocaleString(locale, { day: "2-digit", month: "short", hour: "2-digit", minute: "2-digit" }) : "—",
                        })
                      : t("budget.sessionIdle")}
                  </p>
                </div>
              </Panel>
            ))}
          </div>
          <div className="grid gap-3 md:grid-cols-2">
            <Panel>
              <PanelHeader title={t("budget.resetTitle")} subtitle={t("budget.resetHint")} />
              <div className="flex flex-wrap items-center justify-between gap-3 px-4 pb-4">
                <p className="text-sm text-fg">{t("budget.credits", { count: credits.available, used: credits.used })}</p>
                <Button
                  variant="primary"
                  icon={<RotateCcw className="h-4 w-4" />}
                  loading={busy === "reset"}
                  disabled={busy !== null || credits.available <= 0 || !data.resettable}
                  onClick={() => void act("reset")}
                >
                  {t("budget.resetAction")}
                </Button>
              </div>
            </Panel>
            <Panel>
              <PanelHeader title={t("budget.requestTitle")} subtitle={t("budget.requestHint")} />
              <div className="grid gap-2 px-4 pb-4">
                {data.pending_request && (
                  <p className="text-[0.74rem] text-iris">
                    {t("budget.pending", { date: new Date(data.pending_request.at).toLocaleString(locale, { day: "2-digit", month: "short", hour: "2-digit", minute: "2-digit" }) })}
                  </p>
                )}
                <div className="flex flex-wrap gap-2">
                  <Input className="min-w-0 flex-1" value={note} maxLength={500} placeholder={t("budget.requestPlaceholder")} onChange={(event) => setNote(event.target.value)} aria-label={t("budget.requestTitle")} />
                  <Button icon={<Send className="h-4 w-4" />} loading={busy === "request"} disabled={busy !== null} onClick={() => void act("request")}>
                    {data.pending_request ? t("budget.requestUpdate") : t("budget.requestAction")}
                  </Button>
                </div>
              </div>
            </Panel>
          </div>
          {(data.grants ?? []).length > 0 && (
            <Panel>
              <PanelHeader title={t("budget.grantsTitle")} />
              <ul className="grid gap-1.5 px-4 pb-4 text-[0.78rem]">
                {(data.grants ?? []).map((grant) => (
                  <li key={grant.id} className="text-fg">
                    {grant.extra_input > 0 && `+${compact(grant.extra_input, locale)} ${t("budget.input")} `}
                    {grant.extra_output > 0 && `+${compact(grant.extra_output, locale)} ${t("budget.output")} `}
                    <span className="text-muted">· {t("budget.grantUntil", { time: until(grant.expires_at, now) })}</span>
                    {grant.note && <span className="text-faint"> · {grant.note}</span>}
                  </li>
                ))}
              </ul>
            </Panel>
          )}
        </>
      )}
    </div>
  );
}
