/**
 * One log source of the Logs screen: llama.cpp, or a plugin service that
 * publishes a log (`lib/logsHub.ts`). An admin picks the machine (`?node=`);
 * another machine's log is read through this node, which forwards it signed.
 * The lines are drawn on the shared `Terminal`.
 */
import { ScrollText } from "lucide-react";
import { useLocation, useSearchParams } from "react-router-dom";
import { Badge, PageHeader, Panel, PanelHeader, Select } from "../components/ui";
import { Terminal } from "../components/Terminal";
import { useLogSources } from "../components/LogsTabs";
import { LLAMA_CPP_SOURCE, sourceOfPath } from "../lib/logsHub";
import { useLogSource } from "../lib/queries";
import { useI18n } from "../i18n/provider";

export function LogSource() {
  const { t } = useI18n();
  const { pathname } = useLocation();
  const [params, setParams] = useSearchParams();
  const code = sourceOfPath(pathname) ?? LLAMA_CPP_SOURCE;
  const { sources, selfId } = useLogSources();
  const source = sources.find((entry) => entry.code === code);
  const machines = source?.machines ?? [];
  const asked = params.get("node");
  const nodeId =
    (asked && machines.some((m) => m.nodeId === asked) && asked) ||
    (machines.some((m) => m.nodeId === selfId) ? selfId : machines[0]?.nodeId);
  const remote = nodeId && nodeId !== selfId ? nodeId : undefined;
  const log = useLogSource(code, remote, Boolean(nodeId));
  const name = source?.name ?? code;
  const machineName = machines.find((m) => m.nodeId === nodeId)?.name;

  function pick(next: string) {
    const copy = new URLSearchParams(params);
    copy.set("node", next);
    setParams(copy, { replace: true });
  }

  return (
    <div className="flex h-full min-h-0 flex-col bg-ink px-4 py-5 sm:px-6 sm:py-6">
      <PageHeader
        title={t("logs.sourceTitle", { name })}
        subtitle={t("logs.sourceSubtitle")}
        actions={
          machines.length > 1 ? (
            <label className="flex min-w-[13rem] items-center gap-2 text-xs text-faint">
              <span className="sr-only">{t("logs.machine")}</span>
              <Select aria-label={t("logs.machine")} value={nodeId ?? ""} onChange={(event) => pick(event.target.value)}>
                {machines.map((m) => (
                  <option key={m.nodeId} value={m.nodeId}>
                    {m.name}
                    {m.nodeId === selfId ? ` (${t("logs.thisMachine")})` : ""}
                  </option>
                ))}
              </Select>
            </label>
          ) : null
        }
      />
      <Panel className="flex min-h-0 flex-1 flex-col overflow-hidden">
        <PanelHeader
          title={
            <span className="flex items-center gap-2 font-mono text-sm">
              <ScrollText className="h-4 w-4 text-aqua" />
              {machineName ? `${name} · ${machineName}` : name}
            </span>
          }
          actions={
            log.data ? (
              <Badge tone={log.data.running ? "aqua" : "neutral"}>
                {log.data.running ? t("logs.running") : t("logs.notRunning")}
              </Badge>
            ) : null
          }
        />
        <Terminal
          testId="log-source-terminal"
          lines={log.data?.lines ?? []}
          loading={log.isLoading}
          error={log.isError}
          notice={log.isError ? String((log.error as Error)?.message ?? log.error) : log.data?.previous_run ? t("logs.previousRun") : undefined}
          empty={{ title: t("logs.emptyTitle", { name }), hint: t("logs.emptyHint") }}
        />
      </Panel>
    </div>
  );
}
