/**
 * The Logs screen's Cluster console tab (`/logs/console`, admins): every
 * node's process and engine lines, filterable by deployment. One screen for
 * both sources: with the logs plugin active it reads the plugin's history
 * (`/api/ui/cluster-logs`, persisted, in arrival order); without it, the
 * core's live tail (`/api/ui/cluster-console`, the last lines in memory).
 * `/deployments/logs`, the old plugin-only screen, redirects here.
 */
import { Terminal } from "lucide-react";
import { Link, useSearchParams } from "react-router-dom";
import { Badge, ErrorState, PageHeader, Panel, PanelHeader, Select } from "../components/ui";
import { ClusterConsole } from "../components/ClusterConsole";
import { useClusterConsole, useClusterLogs, useDeployments, useEnterpriseStatus, useNode } from "../lib/queries";
import { useI18n } from "../i18n/provider";

export function LogsConsole() {
  const { t } = useI18n();
  const [searchParams, setSearchParams] = useSearchParams();
  const deployId = searchParams.get("deploy_id") || undefined;
  const logsPlugin = useEnterpriseStatus();
  const history = logsPlugin.data.active;
  const pluginLogs = useClusterLogs(deployId, history);
  const coreLogs = useClusterConsole(deployId, !history && !logsPlugin.isLoading);
  const logs = history ? pluginLogs : coreLogs;
  const deployments = useDeployments();
  // The nodes only send their lines while the principal's collection is on;
  // with it off the console keeps showing the last lines it received.
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
        title={t("logs.console")}
        subtitle={t(history ? "logs.consoleHistorySubtitle" : "logs.consoleSubtitle")}
        actions={
          <label className="flex min-w-[13rem] items-center gap-2 text-xs text-faint">
            <span className="sr-only">{t("deploymentLogs.filterLabel")}</span>
            <Select
              aria-label={t("deploymentLogs.filterLabel")}
              value={deployId ?? ""}
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
          subtitle={t(history ? "logs.sourceHistory" : "logs.sourceMemory")}
          actions={
            <Badge tone={logs.isError ? "rose" : collecting ? "aqua" : "amber"}>
              {logs.isError ? t("deploymentLogs.offline") : collecting ? t("deploymentLogs.live") : t("deploymentLogs.paused")}
            </Badge>
          }
        />
        {!collecting && (
          <div role="status" className="border-b border-amber/20 bg-amber/[0.08] px-5 py-2 text-xs text-amber">
            {node.data?.cluster_logs_authority ? (
              <>
                {t("deploymentLogs.collectionOff")}{" "}
                <Link className="underline" to="/config/preferences/admin">{t("deploymentLogs.collectionOffLink")}</Link>
              </>
            ) : (
              t("deploymentLogs.collectionOffPrincipal")
            )}
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
          testId="logs-console-terminal"
          entries={entries}
          loading={logs.isLoading || logsPlugin.isLoading}
          error={logs.isError}
          empty={{
            title: t("deploymentLogs.emptyTitle"),
            hint: t(history ? "deploymentLogs.emptyHint" : "logs.consoleEmptyHint"),
          }}
        />
      </Panel>
    </div>
  );
}
