/**
 * The logs plugin's history of the cluster's process lines, filterable by
 * deployment. The `/logs` screen is the request audit and remains a table.
 * The terminal is `ClusterConsole`, the same the Dashboard's core console
 * (no plugin, live tail) draws.
 */
import { Terminal } from "lucide-react";
import { Link, useSearchParams } from "react-router-dom";
import {
  Badge,
  ErrorState,
  PageHeader,
  Panel,
  PanelHeader,
  Select,
} from "../components/ui";
import { ClusterConsole } from "../components/ClusterConsole";
import { useClusterLogs, useDeployments, useEnterpriseStatus, useNode } from "../lib/queries";
import { useI18n } from "../i18n/provider";

export function DeploymentLogs() {
  const { t } = useI18n();
  const [searchParams, setSearchParams] = useSearchParams();
  const selectedDeployId = searchParams.get("deploy_id") || undefined;
  // The lines are kept by the logs plugin; without it here there is nothing to read.
  const logsPlugin = useEnterpriseStatus();
  const pluginActive = logsPlugin.data.active;
  const logs = useClusterLogs(selectedDeployId, pluginActive);
  const deployments = useDeployments();
  // The nodes only send their lines while the principal's collection is on;
  // with it off the console keeps showing the last lines it ever received.
  const node = useNode();
  const collecting = node.data?.cluster_logs_enabled !== false;
  const entries = logs.data?.entries ?? [];
  function selectDeployment(value: string) {
    const next = new URLSearchParams(searchParams);
    if (value) next.set("deploy_id", value);
    else next.delete("deploy_id");
    setSearchParams(next);
  }

  return (
    <div className="flex h-full min-h-0 flex-col bg-ink px-4 py-5 sm:px-6 sm:py-6">
      <PageHeader
        title={t("deploymentLogs.title")}
        subtitle={t("deploymentLogs.subtitle")}
        actions={
          <label className="flex min-w-[13rem] items-center gap-2 text-xs text-faint">
            <span className="sr-only">{t("deploymentLogs.filterLabel")}</span>
            <Select
              aria-label={t("deploymentLogs.filterLabel")}
              value={selectedDeployId ?? ""}
              onChange={(event) => selectDeployment(event.target.value)}
            >
              <option value="">{t("deploymentLogs.allDeployments")}</option>
              {(deployments.data?.deployments ?? []).map((row) => (
                <option key={row.deployment.deploy_id} value={row.deployment.deploy_id}>
                  {row.deployment.model.label} · {row.deployment.deploy_id.slice(0, 8)}
                </option>
              ))}
            </Select>
          </label>
        }
      />

      <Panel className="flex min-h-0 flex-1 flex-col overflow-hidden shadow-[0_18px_60px_rgb(0_0_0/0.12)]">
        <PanelHeader
          title={
            <span className="flex items-center gap-2 font-mono text-sm">
              <Terminal className="h-4 w-4 text-aqua" />
              {t("deploymentLogs.consoleTitle")}
            </span>
          }
          subtitle={t("deploymentLogs.consoleSubtitle")}
          actions={
            <Badge tone={logs.isError || !pluginActive ? "rose" : collecting ? "aqua" : "amber"}>
              {logs.isError || !pluginActive
                ? t("deploymentLogs.offline")
                : collecting ? t("deploymentLogs.live") : t("deploymentLogs.paused")}
            </Badge>
          }
        />

        {pluginActive && !collecting && (
          <div role="status" className="border-b border-amber/20 bg-amber/[0.08] px-5 py-2 text-xs text-amber">
            {node.data?.cluster_logs_authority
              ? <>{t("deploymentLogs.collectionOff")} <Link className="underline" to="/config/preferences/admin">{t("deploymentLogs.collectionOffLink")}</Link></>
              : t("deploymentLogs.collectionOffPrincipal")}
          </div>
        )}

        {logs.data && logs.data.dropped > 0 && (
          <div className="border-b border-amber/20 bg-amber/[0.08] px-5 py-2 text-xs text-amber">
            {t("deploymentLogs.dropped", { count: logs.data.dropped })}
          </div>
        )}

        {logs.isError && (
          <div className="border-b border-line p-4">
            <ErrorState error={logs.error} onRetry={() => void logs.refetch()} />
          </div>
        )}

        <ClusterConsole
          entries={entries}
          loading={logs.isLoading}
          error={logs.isError}
          testId="deployment-log-terminal"
          empty={
            logsPlugin.isLoading
              ? { title: "" }
              : {
                  title: t(pluginActive ? "deploymentLogs.emptyTitle" : "deploymentLogs.pluginRequiredTitle"),
                  hint: t(pluginActive ? "deploymentLogs.emptyHint" : "deploymentLogs.pluginRequiredHint"),
                }
          }
        />
      </Panel>
    </div>
  );
}
