import { Check, CheckCircle2, Download, Inbox, Upload, X } from "lucide-react";
import { Link } from "react-router-dom";
import { ModelDownloadsPanel } from "../components/ModelDownloadsPanel";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Footnote,
  FullBleedHeader,
  Panel,
  PanelHeader,
  Progress,
  ScrollX,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { bytes, duration, layerRange, rate } from "../lib/format";
import { useModelDownloads, useOffers, useTransfers } from "../lib/queries";
import { useToast } from "../lib/toast";
import { canCancel, isActiveTransfer, isFinishedTransfer } from "../lib/transferStatus";
import { transferHash, transferTitle } from "../lib/transferLabel";
import { useI18n } from "../i18n/provider";
import type { TransferOffer } from "../lib/types";

export function Transfers() {
  const { t } = useI18n();
  const transfers = useTransfers();
  const offers = useOffers();
  const modelDownloads = useModelDownloads();
  const toast = useToast();

  const jobs = transfers.data?.jobs ?? [];
  const active = jobs.filter((j) => isActiveTransfer(j.state));
  const finished = jobs.filter((j) => isFinishedTransfer(j.state));
  const waiting = (offers.data?.offers ?? []).filter(
    (offer): offer is TransferOffer => !offer.kind || offer.kind === "slice",
  );

  async function cancel(id: string) {
    try {
      await api.post(`/api/transfer/jobs/${id}/cancel`);
      toast.success(t("transfers.cancelled"));
      void transfers.refetch();
    } catch (e) {
      toast.fromError(t("transfers.cancelFailed"), e);
    }
  }

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
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <FullBleedHeader
        title={t("transfers.title")}
        subtitle={t("transfers.subtitle")}
        actions={
          <Link to="/transfers/send">
            <Button variant="primary" icon={<Upload className="h-3.5 w-3.5" />}>
              {t("transfers.sendFile")}
            </Button>
          </Link>
        }
      />

      <div className="min-h-0 min-w-0 flex-1 overflow-auto p-4 sm:p-5">
        {(transfers.isError || offers.isError) && (
          <div className="mb-5">
            <ErrorState
              error={transfers.error ?? offers.error}
              onRetry={() => void transfers.refetch()}
            />
          </div>
        )}

        <div className="space-y-5">
          <ModelDownloadsPanel />

          {waiting.length > 0 && (
            <Panel>
              <PanelHeader
                eyebrow={t("transfers.replyNeeded")}
                title={t("transfers.offersFromMachine", { count: waiting.length })}
                subtitle={t("transfers.offerDetail")}
              />
              <ScrollX>
                <table className="w-full min-w-[48rem] text-sm">
                  <thead>
                    <tr className="border-b border-line text-left text-xs text-faint">
                      <th className="px-5 py-2 font-medium">{t("transfers.columnItem")}</th>
                      <th className="px-5 py-2 font-medium">{t("transfers.columnDetail")}</th>
                      <th className="px-5 py-2 font-medium">{t("models.columnActions")}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {waiting.map((o) => (
                      <tr key={o.id} className="border-t border-line align-top">
                        <td className="min-w-[14rem] px-5 py-3.5">
                          <div className="flex flex-wrap items-center gap-2">
                            <span className="font-medium text-fg">
                              {o.pom?.model.label ?? o.name}
                            </span>
                            {o.start_layer != null && o.end_layer != null && (
                              <Badge mono tone="iris">
                                {layerRange(o.start_layer, o.end_layer)}
                              </Badge>
                            )}
                            {transferHash(o.name) && (
                              <span className="font-mono text-[0.7rem] text-faint" title={o.name}>
                                {transferHash(o.name)}
                              </span>
                            )}
                          </div>
                        </td>
                        <td className="break-path px-5 py-3.5 text-[0.78rem] text-muted">
                          {t("transfers.from")} <span className="text-fg">{o.from_host}</span> · {bytes(o.size_bytes)}{" "}
                          · <span className="font-mono text-faint">{o.remote_path}</span>
                        </td>
                        <td className="whitespace-nowrap px-5 py-3.5">
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
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </ScrollX>
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
              <ScrollX>
                <table className="w-full min-w-[52rem] text-sm">
                  <thead>
                    <tr className="border-b border-line text-left text-xs text-faint">
                      <th className="px-5 py-2 font-medium">{t("transfers.columnItem")}</th>
                      <th className="px-5 py-2 font-medium">{t("transfers.columnProgress")}</th>
                      <th className="px-5 py-2 font-medium">{t("models.columnActions")}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {active.map((job) => (
                      <tr key={job.id} className="border-t border-line align-top">
                        <td className="min-w-[14rem] px-5 py-4">
                          <span className="flex items-center gap-2 font-medium text-fg">
                            {job.direction === "sending" ? (
                              <Upload className="h-3.5 w-3.5 shrink-0 text-faint" aria-hidden="true" />
                            ) : (
                              <Download className="h-3.5 w-3.5 shrink-0 text-faint" aria-hidden="true" />
                            )}
                            {transferTitle(job)}
                            {transferHash(job.name) && (
                              <span className="font-mono text-[0.7rem] font-normal text-faint" title={job.name}>
                                {transferHash(job.name)}
                              </span>
                            )}
                          </span>
                          <p className="mt-1.5 text-[0.7rem] text-faint">
                            {job.direction === "sending"
                              ? t("transfers.sendingTo", { host: job.peer_host })
                              : t("transfers.receivingFrom", { host: job.peer_host })}
                          </p>
                        </td>
                        <td className="min-w-[16rem] px-5 py-4">
                          <div className="flex items-center justify-between gap-3 font-mono text-[0.78rem] text-muted">
                            <span>
                              {t("transfers.ofTotal", {
                                done: bytes(job.bytes_done),
                                total: bytes(job.bytes_total),
                              })}
                            </span>
                            <span>{rate(job.bytes_per_sec)}</span>
                          </div>
                          <Progress
                            className="mt-2"
                            tone="aqua"
                            value={job.bytes_total > 0 ? (job.bytes_done / job.bytes_total) * 100 : null}
                          />
                          <p className="mt-1.5 flex flex-wrap gap-x-3 font-mono text-[0.68rem] text-faint">
                            {job.eta_secs != null && <span>{t("transfers.remaining")} {duration(job.eta_secs)}</span>}
                            <span>{job.detail}</span>
                          </p>
                        </td>
                        <td className="whitespace-nowrap px-5 py-4">
                          {canCancel(job.state) && (
                            <Button
                              size="sm"
                              variant="ghost"
                              icon={<X className="h-3.5 w-3.5" />}
                              onClick={() => void cancel(job.id)}
                            >
                              {t("transfers.cancel")}
                            </Button>
                          )}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </ScrollX>
            )}
          </Panel>

          {finished.length > 0 && (
            <Panel>
              <PanelHeader title={t("transfers.finished")} />
              <ScrollX>
                <table className="w-full min-w-[42rem] text-sm">
                  <thead>
                    <tr className="border-b border-line text-left text-xs text-faint">
                      <th className="px-5 py-2 font-medium">{t("transfers.columnItem")}</th>
                      <th className="px-5 py-2 font-medium">{t("models.size")}</th>
                      <th className="px-5 py-2 font-medium">{t("transfers.columnState")}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {finished.map((job) => (
                      <tr key={job.id} className="border-t border-line align-top">
                        <td className="min-w-[14rem] px-5 py-3">
                          <div className="flex min-w-0 items-center gap-2.5">
                            {job.state === "done" ? (
                              <CheckCircle2 className="h-4 w-4 shrink-0 text-aqua" aria-hidden="true" />
                            ) : (
                              <X
                                className={`h-4 w-4 shrink-0 ${
                                  job.state === "cancelled" ? "text-amber" : "text-rose"
                                }`}
                                aria-hidden="true"
                              />
                            )}
                            <div className="min-w-0">
                              <span className="block truncate text-[0.88rem] text-fg">
                                {transferTitle(job)}
                              </span>
                              {job.detail && (
                                <span className="block truncate text-[0.72rem] text-muted" title={job.detail}>
                                  {job.detail}
                                </span>
                              )}
                            </div>
                          </div>
                        </td>
                        <td className="whitespace-nowrap px-5 py-3 font-mono text-[0.75rem] text-faint">
                          {bytes(job.bytes_total)}
                        </td>
                        <td className="whitespace-nowrap px-5 py-3">
                          <Badge
                            tone={
                              job.state === "done"
                                ? "aqua"
                                : job.state === "cancelled"
                                  ? "amber"
                                  : "rose"
                            }
                          >
                            {job.state === "done" ? (
                              <span className="flex items-center gap-1">
                                <Check className="h-3 w-3" aria-hidden="true" />
                                {t("transfers.completed")}
                              </span>
                            ) : job.state === "cancelled" ? (
                              t("transfers.cancelledState")
                            ) : (
                              t("transfers.failed")
                            )}
                          </Badge>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </ScrollX>
            </Panel>
          )}
        </div>
      </div>

      {waiting.length === 0 && jobs.length === 0 && (modelDownloads.data?.length ?? 0) === 0 && (
        <Footnote
          className="shrink-0 border-t border-line px-4 py-2.5"
          icon={<Inbox className="h-3.5 w-3.5" />}
        >
          {t("transfers.bellNote")}
        </Footnote>
      )}
    </div>
  );
}
