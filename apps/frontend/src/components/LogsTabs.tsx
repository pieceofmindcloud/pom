import type { ReactNode } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { useI18n } from "../i18n/provider";
import { resolvePluginRoute } from "../lib/pluginUi";
import { isDemoUser } from "../lib/session";
import { Tabs } from "./ui";

export const DEPLOYMENT_LOGS_ROUTE = "/deployments/logs";

/**
 * The Logs menu entry opens the request logs (the logs plugin's `/logs`), and the
 * cluster's deployment (process) logs are a tab beside them. Without the plugin
 * serving `/logs`, or for a demonstration visitor (who never sees deployment logs),
 * there is one view and no tab row.
 */
export function LogsTabs({ children }: { children: ReactNode }) {
  const { t } = useI18n();
  const { pathname } = useLocation();
  const navigate = useNavigate();
  if (!resolvePluginRoute("/logs") || isDemoUser()) return <>{children}</>;
  const value = pathname === DEPLOYMENT_LOGS_ROUTE ? DEPLOYMENT_LOGS_ROUTE : "/logs";
  return (
    <div className="flex h-full min-h-0 flex-col">
      <Tabs
        items={[
          { value: "/logs", label: t("nav.requestLogs") },
          { value: DEPLOYMENT_LOGS_ROUTE, label: t("nav.deploymentLogs") },
        ]}
        value={value}
        onChange={(next) => navigate(next)}
        label={t("nav.logs")}
        className="shrink-0 px-4 pt-3 sm:px-6"
      />
      <div className="min-h-0 flex-1">{children}</div>
    </div>
  );
}
