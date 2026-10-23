import { useCallback, useEffect, useState } from "react";
import { rpc, ui, usePluginI18n } from "../host/runtime";
import { useScreenReady } from "../host/ready";

type EngineStatus = {
  state: "idle" | "loading" | "ready" | "stopping" | "error";
  detail: string | null;
  model: string | null;
  n_ctx: number | null;
  in_flight: number;
  runtime: { ready: boolean; version: string | null };
  seconds_in_state: number;
  /** Only for the administrator: the engine's own output, never a prompt. */
  log?: string[];
};

const TONE: Record<EngineStatus["state"], "aqua" | "amber" | "red" | undefined> = {
  idle: undefined,
  loading: "amber",
  ready: "aqua",
  stopping: "amber",
  error: "red",
};

/** How often the screen asks again while it is open. */
const REFRESH_MS = 3000;

export function Main() {
  const { t } = usePluginI18n();
  const { PageHeader, Panel, PanelHeader, Badge, KeyValue, ErrorState } = ui();
  const [status, setStatus] = useState<EngineStatus | null>(null);
  const [error, setError] = useState<unknown>(null);

  const load = useCallback(() => {
    rpc<EngineStatus>("status").then(
      (reply) => {
        setStatus(reply);
        setError(null);
      },
      setError,
    );
  }, []);

  useEffect(() => {
    load();
    const timer = window.setInterval(load, REFRESH_MS);
    return () => window.clearInterval(timer);
  }, [load]);
  useScreenReady(status !== null || error !== null);

  if (error && !status) {
    return <ErrorState title={t("main.title")} error={error} onRetry={load} />;
  }

  return (
    <div className="pb-page">
      <PageHeader title={t("main.title")} subtitle={t("main.subtitle")} />
      {status === null ? (
        <p className="pb-muted">{t("main.loading")}</p>
      ) : (
        <>
          <Panel>
            <PanelHeader title={t("main.engine")} actions={<Badge tone={TONE[status.state]}>{t(`state.${status.state}`)}</Badge>} />
            <dl className="pb-facts">
              <KeyValue label={t("main.model")}>{status.model ?? t("main.none")}</KeyValue>
              <KeyValue label={t("main.context")}>{status.n_ctx ?? t("main.none")}</KeyValue>
              <KeyValue label={t("main.inFlight")}>{status.in_flight}</KeyValue>
              <KeyValue label={t("main.runtime")}>
                {status.runtime.ready ? (status.runtime.version ?? t("main.installed")) : t("main.missing")}
              </KeyValue>
            </dl>
            {status.detail ? <p className="pb-detail">{status.detail}</p> : null}
          </Panel>
          {status.log ? (
            <Panel>
              <PanelHeader title={t("main.log")} />
              <pre className="pb-log">{status.log.join("\n") || t("main.none")}</pre>
            </Panel>
          ) : null}
        </>
      )}
    </div>
  );
}
