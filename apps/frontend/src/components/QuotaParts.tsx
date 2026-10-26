/**
 * The pieces of this node's token budget shared by the Usage screen
 * (`routes/MyBudget.tsx`) and the Usage popover beside the bell
 * (`components/UsageButton.tsx`): the shape `GET /api/ui/quota` answers and
 * how one plan's spend is drawn.
 */
import { useEffect, useState } from "react";
import { useI18n } from "../i18n/provider";

export interface QuotaLimit {
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

export interface QuotaStatus {
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

export type T = ReturnType<typeof useI18n>["t"];

export function compact(value: number, locale: string) {
  return new Intl.NumberFormat(locale, { notation: "compact", maximumFractionDigits: 1 }).format(value);
}

export function useNow() {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(timer);
  }, []);
  return now;
}

export function until(iso: string | null, now: number) {
  if (!iso) return "—";
  const minutes = Math.max(0, Math.round((Date.parse(iso) - now) / 60_000));
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor((minutes % 1440) / 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes % 60}m`;
  return `${minutes % 60}m`;
}

export function Meter({ label, used, limit, extra }: { label: string; used: number; limit: number; extra: number }) {
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

export function periodText(t: T, limit: QuotaLimit) {
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

export function scopeText(t: T, limit: QuotaLimit) {
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

