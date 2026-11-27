import { ChevronUp, Terminal } from "lucide-react";
import { useState } from "react";
import { useClusterConsole, useNode } from "../lib/queries";
import { useI18n } from "../i18n/provider";
import { ClusterConsole } from "./ClusterConsole";
import { Badge } from "./ui";

/**
 * The Dashboard's cluster console: the core's live tail of every node's
 * process and engine lines (`/api/ui/cluster-console`). It needs no plugin;
 * the logs plugin's history is the Deployment Logs screen.
 */
export function DeploymentLogDock() {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const logs = useClusterConsole(undefined, open);
  // With the network's collection off no node sends lines: say so instead of "Live".
  const node = useNode();
  const collecting = node.data?.cluster_logs_enabled !== false;
  const entries = logs.data?.entries ?? [];

  return (
    <section
      className={`flex min-h-8 shrink-0 flex-col border-t border-line bg-surface transition-[height] duration-200 ${open ? "h-1/2 max-h-1/2" : "h-8"}`}
      aria-label={t("deploymentLogs.consoleTitle")}
    >
      <button
        type="button"
        className="flex h-8 shrink-0 items-center gap-2 px-3 text-left text-[0.68rem] text-muted hover:bg-raised hover:text-fg"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        <Terminal className="h-3.5 w-3.5 text-aqua" />
        <span className="font-mono">{t("deploymentLogs.consoleTitle")}</span>
        <Badge tone={logs.isError ? "rose" : collecting ? "aqua" : "amber"}>
          {logs.isError ? t("deploymentLogs.offline") : collecting ? t("deploymentLogs.live") : t("deploymentLogs.paused")}
        </Badge>
        <ChevronUp className={`ml-auto h-3.5 w-3.5 transition-transform ${open ? "rotate-180" : ""}`} />
      </button>
      {open && (
        <ClusterConsole
          compact
          testId="deployment-log-dock-terminal"
          entries={entries}
          loading={logs.isLoading}
          error={logs.isError}
          notice={
            !collecting
              ? node.data?.cluster_logs_authority
                ? t("deploymentLogs.collectionOff")
                : t("deploymentLogs.collectionOffPrincipal")
              : undefined
          }
          empty={{ title: t("deploymentLogs.emptyTitle") }}
        />
      )}
    </section>
  );
}
