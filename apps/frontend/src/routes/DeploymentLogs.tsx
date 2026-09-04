/**
 * Console do processo inteiro do cluster. A tela de `/logs` é a auditoria de
 * requisições e continua sendo uma tabela; esta tela só mostra as linhas que o
 * principal recebeu dos nós, na ordem persistida de chegada.
 */
import { Terminal } from "lucide-react";
import { useSearchParams } from "react-router-dom";
import {
  Badge,
  Empty,
  ErrorState,
  PageHeader,
  Panel,
  PanelHeader,
  Select,
  Skeleton,
} from "../components/ui";
import { clock } from "../lib/format";
import { useClusterLogs, useDeployments } from "../lib/queries";
import { useI18n } from "../i18n/provider";

export function DeploymentLogs() {
  const { t } = useI18n();
  const [searchParams, setSearchParams] = useSearchParams();
  const selectedDeployId = searchParams.get("deploy_id") || undefined;
  const logs = useClusterLogs(selectedDeployId);
  const deployments = useDeployments();
  const entries = logs.data?.entries ?? [];

  function selectDeployment(value: string) {
    const next = new URLSearchParams(searchParams);
    if (value) next.set("deploy_id", value);
    else next.delete("deploy_id");
    setSearchParams(next);
  }

  return (
    <div className="flex h-full min-h-0 flex-col bg-[#050505] px-4 py-5 sm:px-6 sm:py-6">
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

      <Panel className="flex min-h-0 flex-1 flex-col overflow-hidden border-white/10 bg-black shadow-[0_18px_60px_rgb(0_0_0/0.35)]">
        <PanelHeader
          className="border-white/10 bg-[#090909]"
          title={
            <span className="flex items-center gap-2 font-mono text-sm">
              <Terminal className="h-4 w-4 text-aqua" />
              {t("deploymentLogs.consoleTitle")}
            </span>
          }
          subtitle={t("deploymentLogs.consoleSubtitle")}
          actions={
            <Badge tone={logs.isError ? "rose" : "aqua"}>
              {logs.isError ? t("deploymentLogs.offline") : t("deploymentLogs.live")}
            </Badge>
          }
        />

        {logs.data && logs.data.dropped > 0 && (
          <div className="border-b border-amber/20 bg-amber/[0.08] px-5 py-2 text-xs text-amber">
            {t("deploymentLogs.dropped", { count: logs.data.dropped })}
          </div>
        )}

        <div
          data-testid="deployment-log-terminal"
          className="min-h-0 flex-1 overflow-auto bg-black p-3 font-mono text-[0.76rem] leading-6 text-emerald-100 sm:p-4"
          role="log"
          aria-live="polite"
        >
          {logs.isLoading && <Skeleton className="h-24 bg-white/10" />}
          {logs.isError && <ErrorState error={logs.error} onRetry={() => void logs.refetch()} />}
          {!logs.isLoading && !logs.isError && entries.length === 0 && (
            <Empty
              icon={<Terminal className="h-5 w-5" />}
              title={t("deploymentLogs.emptyTitle")}
            >
              {t("deploymentLogs.emptyHint")}
            </Empty>
          )}
          {entries.map((entry) => (
            <div
              key={entry.received_seq}
              data-testid="deployment-log-line"
              className="group whitespace-pre-wrap break-words rounded px-2 py-0.5 hover:bg-white/[0.045]"
              title={entry.origin_at}
            >
              <span className="mr-2 text-cyan-300">{entry.node_name || entry.node_id}</span>
              <span className="mr-2 text-slate-500">[{clock(entry.origin_at)}]</span>
              <span className="mr-2 text-slate-600">{entry.level.toLowerCase()}</span>
              <span className="text-emerald-100">{entry.message}</span>
            </div>
          ))}
        </div>
      </Panel>
    </div>
  );
}
