import { Check, Download, Inbox, Upload, X } from "lucide-react";
import { Link } from "react-router-dom";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Footnote,
  PageHeader,
  Panel,
  PanelHeader,
  Progress,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { bytes, duration, layerRange, rate } from "../lib/format";
import { useOffers, useTransfers } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";

export function Transfers() {
  const { t } = useI18n();
  const transfers = useTransfers();
  const offers = useOffers();
  const toast = useToast();

  const jobs = transfers.data?.jobs ?? [];
  const active = jobs.filter((j) => j.state !== "done" && j.state !== "failed");
  const finished = jobs.filter((j) => j.state === "done" || j.state === "failed");
  const waiting = offers.data?.offers ?? [];

  async function answer(id: string, accept: boolean) {
    try {
      await api.post(`/api/transfer/offers/${id}/${accept ? "accept" : "reject"}`);
      toast.success(accept ? t("transfers.offerAccepted") : t("transfers.offerRejected"));
      void offers.refetch();
      void transfers.refetch();
    } catch (e) {
      toast.fromError(t("transfers.offerFailed"), e);
    }
  }

  return (
    <>
      <PageHeader
        title={t("transfers.title")}
        subtitle={t("transfers.subtitle")}
        actions={
          <Link to="/transferencias/enviar">
            <Button variant="primary" icon={<Upload className="h-3.5 w-3.5" />}>
              {t("transfers.sendFile")}
            </Button>
          </Link>
        }
      />

      {(transfers.isError || offers.isError) && (
        <ErrorState
          error={transfers.error ?? offers.error}
          onRetry={() => void transfers.refetch()}
        />
      )}

      <div className="space-y-5">
        {waiting.length > 0 && (
          <Panel>
            <PanelHeader
              eyebrow={t("transfers.replyNeeded")}
              title={t("transfers.offersFromMachine", { count: waiting.length })}
              subtitle={t("transfers.offerDetail")}
            />
            <div className="divide-y divide-line">
              {waiting.map((o) => (
                <div
                  key={o.id}
                  className="flex flex-wrap items-center gap-x-4 gap-y-3 px-5 py-3.5"
                >
                  <div className="min-w-[14rem] flex-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-medium text-fg">{o.name}</span>
                      {o.start_layer != null && o.end_layer != null && (
                        <Badge mono tone="iris">
                          {layerRange(o.start_layer, o.end_layer)}
                        </Badge>
                      )}
                    </div>
                    <p className="break-path mt-0.5 text-[0.78rem] text-muted">
                      {t("transfers.from")} <span className="text-fg">{o.from_host}</span> · {bytes(o.size_bytes)}{" "}
                      · <span className="font-mono text-faint">{o.remote_path}</span>
                    </p>
                  </div>
                  <div className="flex shrink-0 gap-2">
                    <Button
                      variant="primary"
                      size="sm"
                      icon={<Check className="h-3.5 w-3.5" />}
                      onClick={() => void answer(o.id, true)}
                    >
                      {t("transfers.accept")}
                    </Button>
                    <Button
                      variant="ghost"
                      size="sm"
                      icon={<X className="h-3.5 w-3.5" />}
                      onClick={() => void answer(o.id, false)}
                    >
                      {t("transfers.reject")}
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          </Panel>
        )}

        <Panel>
          {/* Only worth a header when there is something under it; otherwise the
              empty state below already carries the whole message. */}
          {(transfers.isLoading || active.length > 0) && (
            <PanelHeader
              title={
                active.length
                  ? t("transfers.activeTitle", { count: active.length })
                  : t("transfers.nothingRunning")
              }
              subtitle={t("transfers.progressDetail")}
            />
          )}
          {transfers.isLoading ? (
            <div className="space-y-3 p-5">
              <Skeleton className="h-12 w-full" />
            </div>
          ) : active.length === 0 ? (
            <Empty icon={<Download className="h-5 w-5" />} title={t("transfers.nothingRunning")}>
              {t("transfers.receivedDetail")}
            </Empty>
          ) : (
            <div className="divide-y divide-line">
              {active.map((job) => (
                <div key={job.id} className="px-5 py-4">
                  <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
                    <span className="font-medium text-fg">{job.name}</span>
                    <span className="font-mono text-[0.78rem] text-muted">
                      {bytes(job.bytes_done)} de {bytes(job.bytes_total)}
                    </span>
                  </div>
                  <Progress
                    className="mt-2.5"
                    tone="aqua"
                    value={job.bytes_total > 0 ? (job.bytes_done / job.bytes_total) * 100 : null}
                  />
                  <p className="mt-2 flex flex-wrap gap-x-4 font-mono text-[0.7rem] text-faint">
                    <span>{t("transfers.from")} {job.from_host}</span>
                    <span>{rate(job.bytes_per_sec)}</span>
                    {job.eta_secs != null && <span>{t("transfers.remaining")} {duration(job.eta_secs)}</span>}
                    <span>{job.detail}</span>
                  </p>
                </div>
              ))}
            </div>
          )}
        </Panel>

        {finished.length > 0 && (
          <Panel>
            <PanelHeader title={t("transfers.finished")} />
            <div className="divide-y divide-line">
              {finished.map((job) => (
                <div
                  key={job.id}
                  className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 px-5 py-3"
                >
                  <span className="min-w-0 truncate text-[0.88rem] text-fg">{job.name}</span>
                  <div className="flex shrink-0 items-center gap-3">
                    <span className="font-mono text-[0.75rem] text-faint">
                      {bytes(job.bytes_total)}
                    </span>
                    <Badge tone={job.state === "done" ? "aqua" : "rose"}>
                      {job.state === "done" ? t("transfers.completed") : job.detail || t("transfers.failed")}
                    </Badge>
                  </div>
                </div>
              ))}
            </div>
          </Panel>
        )}
      </div>

      {waiting.length === 0 && jobs.length === 0 && (
        <Footnote
          className="mt-4"
          icon={<Inbox className="h-3.5 w-3.5" />}
        >
          {t("transfers.bellNote")}
        </Footnote>
      )}
    </>
  );
}
