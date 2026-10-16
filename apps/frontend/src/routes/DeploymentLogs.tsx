/**
 * Console of the whole cluster process. The `/logs` screen is the request
 * audit and remains a table; this screen only shows the lines that
 * the local replica received from the nodes, in the persisted order of arrival.
 */
import { ArrowDownToLine, Check, Copy, Terminal } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import {
  Badge,
  ErrorState,
  PageHeader,
  Panel,
  PanelHeader,
  Select,
  Skeleton,
} from "../components/ui";
import { isNearBottom } from "../lib/chatScroll";
import { copyText } from "../lib/clipboard";
import { clock } from "../lib/format";
import { useClusterLogs, useDeployments, useEnterpriseStatus } from "../lib/queries";
import { useI18n } from "../i18n/provider";

function logLineText(entry: {
  node_id: string;
  node_name: string;
  origin_at: string;
  level: string;
  message: string;
}) {
  return `${entry.node_name || entry.node_id} [${entry.origin_at}] ${entry.level.toLowerCase()} ${entry.message}`;
}

export function DeploymentLogs() {
  const { t } = useI18n();
  const [searchParams, setSearchParams] = useSearchParams();
  const selectedDeployId = searchParams.get("deploy_id") || undefined;
  // The lines are kept by the logs plugin; without it here there is nothing to read.
  const logsPlugin = useEnterpriseStatus();
  const pluginActive = logsPlugin.data.active;
  const logs = useClusterLogs(selectedDeployId, pluginActive);
  const deployments = useDeployments();
  const entries = logs.data?.entries ?? [];
  const terminalRef = useRef<HTMLDivElement>(null);
  const wasAtBottom = useRef(true);
  const [autoScroll, setAutoScroll] = useState(true);
  const [copyState, setCopyState] = useState<"idle" | "copied" | "failed">("idle");
  const latestSequence = entries[entries.length - 1]?.received_seq ?? null;

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal || !autoScroll || !wasAtBottom.current || entries.length === 0) return;
    terminal.scrollTo({ top: terminal.scrollHeight, behavior: "auto" });
  }, [autoScroll, entries.length, latestSequence]);

  function handleTerminalScroll() {
    const terminal = terminalRef.current;
    if (!terminal) return;
    const atBottom = isNearBottom({
      scrollTop: terminal.scrollTop,
      clientHeight: terminal.clientHeight,
      scrollHeight: terminal.scrollHeight,
    });
    wasAtBottom.current = atBottom;
    if (!atBottom) setAutoScroll(false);
  }

  function toggleAutoScroll() {
    const next = !autoScroll;
    setAutoScroll(next);
    if (next) {
      wasAtBottom.current = true;
      terminalRef.current?.scrollTo({
        top: terminalRef.current.scrollHeight,
        behavior: "auto",
      });
    }
  }

  async function copyLogs() {
    const copied = await copyText(entries.slice(-500).map(logLineText).join("\n"));
    setCopyState(copied ? "copied" : "failed");
  }

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
            <Badge tone={logs.isError || !pluginActive ? "rose" : "aqua"}>
              {logs.isError || !pluginActive ? t("deploymentLogs.offline") : t("deploymentLogs.live")}
            </Badge>
          }
        />

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

        <div className="relative min-h-0 flex-1 overflow-hidden bg-black">
          <div className="absolute right-3 top-3 z-10 flex items-center gap-2">
            <button
              type="button"
              aria-label={
                autoScroll
                  ? t("deploymentLogs.disableAutoScroll")
                  : t("deploymentLogs.enableAutoScroll")
              }
              aria-pressed={autoScroll}
              title={
                autoScroll
                  ? t("deploymentLogs.disableAutoScroll")
                  : t("deploymentLogs.enableAutoScroll")
              }
              onClick={toggleAutoScroll}
              className="inline-flex h-8 items-center gap-1.5 rounded-md border border-white/20 bg-white/10 px-2.5 text-xs font-medium text-slate-200 transition-colors hover:border-white/35 hover:bg-white/20 hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-aqua/70"
            >
              <ArrowDownToLine className="h-3.5 w-3.5" aria-hidden="true" />
              {autoScroll
                ? t("deploymentLogs.autoScrollOn")
                : t("deploymentLogs.autoScrollOff")}
            </button>
            <button
              type="button"
              aria-label={t("deploymentLogs.copyLogs")}
              title={t("deploymentLogs.copyLogs")}
              onClick={() => void copyLogs()}
              className="inline-flex h-8 items-center gap-1.5 rounded-md border border-white/20 bg-white/10 px-2.5 text-xs font-medium text-slate-200 transition-colors hover:border-white/35 hover:bg-white/20 hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-aqua/70"
            >
              {copyState === "copied" ? (
                <Check className="h-3.5 w-3.5 text-aqua" aria-hidden="true" />
              ) : (
                <Copy className="h-3.5 w-3.5" aria-hidden="true" />
              )}
              {copyState === "copied"
                ? t("deploymentLogs.logsCopied")
                : t("deploymentLogs.copyLogs")}
            </button>
          </div>

          <div
            ref={terminalRef}
            data-testid="deployment-log-terminal"
            onScroll={handleTerminalScroll}
            className="h-full overflow-auto p-3 pt-14 font-mono text-[0.76rem] leading-6 text-emerald-100 sm:p-4 sm:pt-14"
            role="log"
            aria-live="polite"
          >
            {logs.isLoading && <Skeleton className="h-24 bg-white/10" />}
            {!logs.isLoading && !logs.isError && entries.length === 0 && !logsPlugin.isLoading && (
              <div className="flex flex-col items-center justify-center px-6 py-14 text-center text-slate-200">
                <div className="mb-3.5 flex h-11 w-11 items-center justify-center rounded-xl border border-white/15 bg-white/10 text-slate-400">
                  <Terminal className="h-5 w-5" aria-hidden="true" />
                </div>
                <p className="font-display text-[0.95rem] font-semibold text-slate-100">
                  {t(pluginActive ? "deploymentLogs.emptyTitle" : "deploymentLogs.pluginRequiredTitle")}
                </p>
                <p className="mt-1.5 max-w-sm text-sm leading-relaxed text-slate-400">
                  {t(pluginActive ? "deploymentLogs.emptyHint" : "deploymentLogs.pluginRequiredHint")}
                </p>
              </div>
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
        </div>
      </Panel>
      {copyState === "failed" && (
        <p className="mt-2 text-right text-xs text-rose" role="status">
          {t("deploymentLogs.copyFailed")}
        </p>
      )}
      {copyState === "copied" && (
        <p className="sr-only" role="status">
          {t("deploymentLogs.logsCopied")}
        </p>
      )}
    </div>
  );
}
