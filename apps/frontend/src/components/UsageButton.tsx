import clsx from "clsx";
import { Gauge } from "lucide-react";
import { Link, useLocation } from "react-router-dom";
import { useI18n } from "../i18n/provider";
import { useMyQuota } from "../lib/queries";
import { useProfile } from "../lib/profile";
import { USAGE_ROUTE } from "./nav";

/**
 * Usage, beside the bell. Drawn only when this node has a budget set for it
 * (`configured` in `GET /api/ui/quota`): without one the screen would only say
 * there is nothing to count, so the button would be a dead end.
 */
export function UsageButton() {
  const { t } = useI18n();
  const { known } = useProfile();
  const quota = useMyQuota<{ available: boolean; pending?: boolean; configured?: boolean }>(known);
  const { pathname } = useLocation();
  if (quota.data?.configured !== true) return null;
  const active = pathname === USAGE_ROUTE;
  return (
    <Link
      to={USAGE_ROUTE}
      data-testid="topbar-usage"
      title={t("nav.budget")}
      aria-label={t("nav.budget")}
      aria-current={active ? "page" : undefined}
      className={clsx(
        "flex h-9 items-center gap-1.5 rounded-lg border px-2.5 text-sm transition-colors duration-150",
        active
          ? "border-iris/40 bg-iris/12 text-iris"
          : "border-line bg-raised text-muted hover:border-line-strong hover:text-fg",
      )}
    >
      <Gauge className="h-4 w-4" />
      <span className="hidden sm:inline">{t("nav.budget")}</span>
    </Link>
  );
}
