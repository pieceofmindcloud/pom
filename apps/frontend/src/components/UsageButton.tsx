import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import { ArrowRight, Ban, Gauge, TimerReset } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { useI18n } from "../i18n/provider";
import { useMyQuota } from "../lib/queries";
import { useProfile } from "../lib/profile";
import { USAGE_ROUTE } from "./nav";
import { Meter, periodText, scopeText, until, useNow, type QuotaStatus } from "./QuotaParts";

/**
 * Usage, beside the bell: an icon that opens this node's budget in a popover,
 * the way the bell opens the activity. Drawn only when this node has a budget
 * set for it (`configured` in `GET /api/ui/quota`): without one there is
 * nothing to count. Resetting the session or asking for more stays on the
 * Usage screen, which the popover links to.
 */
export function UsageButton() {
  const { t, locale } = useI18n();
  const { known } = useProfile();
  const quota = useMyQuota<QuotaStatus>(known);
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const now = useNow();

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", onClick);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onClick);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  const data = quota.data;
  if (data?.configured !== true) return null;
  const limits = data.limits ?? [];
  const credits = data.reset_credits ?? { available: 0, used: 0 };
  // The bar on the icon: the plan closest to its ceiling, so a glance says
  // whether the next request may be refused.
  const worst = Math.max(
    0,
    ...limits.flatMap((limit) => [
      limit.input_limit > 0 ? limit.used_input / limit.input_limit : 0,
      limit.output_limit > 0 ? limit.used_output / limit.output_limit : 0,
    ]),
  );
  const tone = data.blocked || worst >= 1 ? "rose" : worst >= 0.8 ? "amber" : null;

  return (
    <div className="relative" ref={ref}>
      <button
        onClick={() => setOpen((o) => !o)}
        data-testid="topbar-usage"
        title={t("nav.budget")}
        aria-label={t("nav.budget")}
        aria-expanded={open}
        className={clsx(
          "relative flex h-9 w-9 items-center justify-center rounded-lg border transition-colors duration-150",
          open
            ? "border-iris/40 bg-iris/12 text-iris"
            : "border-line bg-raised text-muted hover:border-line-strong hover:text-fg",
        )}
      >
        <Gauge className="h-4 w-4" />
        {tone && (
          <span className={clsx("absolute -right-0.5 -top-0.5 h-2.5 w-2.5 rounded-full", tone === "rose" ? "bg-rose" : "bg-amber")} />
        )}
      </button>

      <AnimatePresence>
        {open && (
          <motion.div
            initial={{ opacity: 0, y: -6, scale: 0.98 }}
            animate={{ opacity: 1, y: 0, scale: 1 }}
            exit={{ opacity: 0, y: -6, scale: 0.98 }}
            transition={{ duration: 0.16, ease: [0.22, 1, 0.36, 1] }}
            data-testid="usage-popover"
            className="fixed right-3 top-[3.75rem] z-50 w-[min(22rem,calc(100vw-1.5rem))] origin-top-right sm:absolute sm:right-0 sm:top-11 overflow-hidden rounded-xl border border-line bg-surface shadow-lift"
          >
            <div className="flex items-center justify-between border-b border-line px-4 py-3">
              <span className="font-display text-sm font-semibold text-fg">{t("budget.title")}</span>
              {data.blocked ? (
                <span className="flex items-center gap-1 text-[0.72rem] text-rose"><Ban className="h-3 w-3" />{t("budget.blocked")}</span>
              ) : (
                <span className="text-[0.72rem] text-aqua">{t("budget.within")}</span>
              )}
            </div>
            <div className="max-h-[26rem] divide-y divide-line overflow-y-auto">
              {limits.length === 0 && <p className="px-4 py-3 text-[0.78rem] text-muted">{t("budget.none")}</p>}
              {limits.map((limit) => (
                <div key={`${limit.assignment_id}:${limit.counter}`} className="grid gap-2.5 px-4 py-3">
                  <div className="min-w-0">
                    <p className="truncate text-[0.8rem] font-medium text-fg">{limit.plan}</p>
                    <p className="truncate text-[0.7rem] text-faint">{periodText(t, limit)} · {scopeText(t, limit)}</p>
                  </div>
                  <Meter label={t("budget.input")} used={limit.used_input} limit={limit.input_limit} extra={limit.extra_input} />
                  <Meter label={t("budget.output")} used={limit.used_output} limit={limit.output_limit} extra={limit.extra_output} />
                  <p className="flex items-center gap-1.5 text-[0.7rem] text-muted">
                    <TimerReset className="h-3.5 w-3.5 shrink-0" />
                    {limit.active
                      ? t("budget.resetsIn", {
                          time: until(limit.resets_at, now),
                          date: limit.resets_at ? new Date(limit.resets_at).toLocaleString(locale, { day: "2-digit", month: "short", hour: "2-digit", minute: "2-digit" }) : "—",
                        })
                      : t("budget.sessionIdle")}
                  </p>
                </div>
              ))}
            </div>
            <div className="flex items-center justify-between gap-3 border-t border-line px-4 py-3">
              <span className="text-[0.72rem] text-muted">{t("budget.credits", { count: credits.available, used: credits.used })}</span>
              <Link
                to={USAGE_ROUTE}
                onClick={() => setOpen(false)}
                data-testid="usage-open-screen"
                className="flex shrink-0 items-center gap-1 text-[0.74rem] font-medium text-iris hover:underline"
              >
                {t("budget.manage")}
                <ArrowRight className="h-3.5 w-3.5" />
              </Link>
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
