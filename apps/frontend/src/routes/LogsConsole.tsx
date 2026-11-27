/**
 * The Logs screen's Cluster console tab: every node's process and engine
 * lines, the network's deployment log, from the core's live tail
 * (`/api/ui/cluster-console`). No plugin; the logs plugin's history is the
 * Deployment logs tab.
 */
import { Terminal as TerminalIcon } from "lucide-react";
import { Link } from "react-router-dom";
import { Badge, PageHeader, Panel, PanelHeader } from "../components/ui";
import { ClusterConsole } from "../components/ClusterConsole";
import { useClusterConsole, useNode } from "../lib/queries";
import { useI18n } from "../i18n/provider";

export function LogsConsole() {
  const { t } = useI18n();
  const logs = useClusterConsole();
  const node = useNode();
  const collecting = node.data?.cluster_logs_enabled !== false;
  return (
    <div className="flex h-full min-h-0 flex-col bg-ink px-4 py-5 sm:px-6 sm:py-6">
      <PageHeader title={t("logs.console")} subtitle={t("logs.consoleSubtitle")} />
      <Panel className="flex min-h-0 flex-1 flex-col overflow-hidden">
        <PanelHeader
          title={
            <span className="flex items-center gap-2 font-mono text-sm">
              <TerminalIcon className="h-4 w-4 text-aqua" />
              {t("deploymentLogs.consoleTitle")}
            </span>
          }
          actions={
            <Badge tone={logs.isError ? "rose" : collecting ? "aqua" : "amber"}>
              {logs.isError ? t("deploymentLogs.offline") : collecting ? t("deploymentLogs.live") : t("deploymentLogs.paused")}
            </Badge>
          }
        />
        <ClusterConsole
          testId="logs-console-terminal"
          entries={logs.data?.entries ?? []}
          loading={logs.isLoading}
          error={logs.isError}
          notice={
            !collecting ? (
              node.data?.cluster_logs_authority ? (
                <>
                  {t("deploymentLogs.collectionOff")}{" "}
                  <Link className="underline" to="/config/preferences/admin">{t("deploymentLogs.collectionOffLink")}</Link>
                </>
              ) : (
                t("deploymentLogs.collectionOffPrincipal")
              )
            ) : undefined
          }
          empty={{ title: t("deploymentLogs.emptyTitle"), hint: t("logs.consoleEmptyHint") }}
        />
      </Panel>
    </div>
  );
}
