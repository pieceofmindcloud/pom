import type { ReactNode } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { useI18n } from "../i18n/provider";
import { logNodes, logSources, logsTabs, type LogSource } from "../lib/logsHub";
import { useProfile } from "../lib/profile";
import { useCluster, useNode } from "../lib/queries";
import { isDemoUser } from "../lib/session";
import { Tabs } from "./ui";

export { DEPLOYMENT_LOGS_ROUTE } from "../lib/logsHub";

/**
 * The tab row of the Logs screen (`lib/logsHub.ts`): the cluster console and
 * this machine's engines are the POM's, for every admin and worker; the logs
 * plugin adds Request logs and Deployment logs. One tab, or a demonstration
 * visitor, draws no row.
 */
/** The log sources this profile reads: this machine's, or every machine's for an admin. */
export function useLogSources(): { sources: LogSource[]; selfId: string | undefined; loading: boolean } {
  const { profile } = useProfile();
  const node = useNode();
  const cluster = useCluster();
  const nodes = logNodes(profile, node.data, cluster.data?.peers);
  return { sources: logSources(nodes), selfId: node.data?.node_id, loading: node.isLoading };
}

export function LogsTabs({ children }: { children: ReactNode }) {
  const { t } = useI18n();
  const { pathname } = useLocation();
  const navigate = useNavigate();
  const { profile } = useProfile();
  const { sources } = useLogSources();
  const tabs = logsTabs(profile, sources, t);
  if (tabs.length < 2 || isDemoUser()) return <>{children}</>;
  const value = tabs.find((tab) => tab.value === pathname)?.value ?? tabs[0].value;
  return (
    <div className="flex h-full min-h-0 flex-col">
      <Tabs
        items={tabs.map(({ value, label }) => ({ value, label }))}
        value={value}
        onChange={(next) => navigate(next)}
        label={t("nav.logs")}
        className="shrink-0 px-4 pt-3 sm:px-6"
      />
      <div className="min-h-0 flex-1">{children}</div>
    </div>
  );
}
