/**
 * What this node is busy with right now — and what it has just finished.
 *
 * Everything listed here is read back from the node — slicing runs from
 * `GET /api/ui/jobs`, transfers from `GET /api/transfer/jobs`, pending offers
 * from `GET /api/transfer/offers`. None of it is browser state, so reloading
 * the page in the middle of a 17-minute transfer shows the same bar at the same
 * percentage instead of losing it.
 *
 * O sino lista os **dois sentidos** e também o desfecho recente
 * (`isWorthNotifying`). Antes ele mostrava só o que estava em andamento, e o
 * resultado — concluída, falhada, recusada — sumia da tela no instante em que
 * acontecia: quem recebia via a barra andar e depois nada, quem enviava não
 * via aviso nenhum de que a outra ponta tinha aceitado ou recusado.
 */
import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import {
  Bell,
  Blocks,
  Check,
  CheckCircle2,
  Download,
  Inbox,
  Info,
  Scissors,
  ShieldAlert,
  Upload,
  UserPlus,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "../lib/api";
import { bytes, duration, rate, since } from "../lib/format";
import { useAdminAccess, useJobs, useOffers, useTransfers } from "../lib/queries";
import {
  dismissDefaultPasswordNotice,
  markDefaultPasswordNoticeRead,
  useDefaultPasswordNoticeState,
} from "../lib/defaultPasswordNotice";
import { admissionsApi, retryWaitMinutes, useAdmissionRequests, useAdmissions, type AdmissionRecord } from "../lib/admissions";
import { useProfile } from "../lib/profile";
import { useToast } from "../lib/toast";
import { isFinishedTransfer, isWorthNotifying } from "../lib/transferStatus";
import { transferHash, transferTitle } from "../lib/transferLabel";
import type {
  AdminTransferOffer,
  NetworkOffer,
  TransferJob,
  TransferOffer,
} from "../lib/types";
import { Badge, Button, Empty, Progress } from "./ui";
import {
  answerPendingPluginConfirm,
  dismissPluginNotice,
  markPluginNoticesRead,
  usePendingPluginConfirms,
  usePluginNotices,
  type PluginNotice,
} from "../lib/pluginInbox";
import { pluginDisplayName } from "../lib/pluginUi";
import { useI18n } from "../i18n/provider";
import { localizeServerText } from "../i18n/locales";

/** What an admin needs to decide on a candidate, in one line. */
function admissionDetail(record: AdmissionRecord, t: (key: string, values?: Record<string, string | number>) => string): string {
  const { card } = record;
  const gigabytes = (mb: number) => `${(mb / 1024).toFixed(mb >= 10 * 1024 ? 0 : 1)} GB`;
  const vram = card.gpus.reduce((sum, gpu) => sum + (gpu.vram_total_mb ?? 0), 0);
  return [
    t(`network.enrollment.${record.role}`),
    card.relayed == null ? null : t(card.relayed ? "lobby.connectionRelayed" : "lobby.connectionDirect"),
    card.rtt_ms == null ? null : t("lobby.latency", { value: card.rtt_ms }),
    card.cpu_count > 0 ? t("lobby.cpuCount", { count: card.cpu_count }) : null,
    card.ram_total_mb > 0 ? t("lobby.ramTotal", { value: gigabytes(card.ram_total_mb) }) : null,
    card.gpus.length === 0
      ? t("lobby.noGpu")
      : `${card.gpus.map((gpu) => gpu.name).join(", ")}${vram > 0 ? ` (${gigabytes(vram)} VRAM)` : ""}`,
  ].filter(Boolean).join(" · ");
}

/** Ícone, tom e chave de estado de uma transferência, num lugar só. */
function transferLook(job: TransferJob) {
  if (job.state === "done") {
    return { icon: <CheckCircle2 className="h-3.5 w-3.5" />, tone: "aqua" as const };
  }
  if (job.state === "failed") {
    return { icon: <X className="h-3.5 w-3.5" />, tone: "rose" as const };
  }
  if (job.state === "cancelled") {
    return { icon: <X className="h-3.5 w-3.5" />, tone: "amber" as const };
  }
  return {
    icon:
      job.direction === "sending" ? (
        <Upload className="h-3.5 w-3.5" />
      ) : (
        <Download className="h-3.5 w-3.5" />
      ),
    tone: "aqua" as const,
  };
}

function Row({
  icon,
  title,
  aside,
  detail,
  progress,
  tone,
  footer,
}: {
  icon: React.ReactNode;
  title: string;
  /** Identificação secundária — o hash curto de uma fatia. */
  aside?: string | null;
  detail?: string;
  progress?: number | null;
  tone?: "iris" | "aqua" | "amber" | "rose";
  footer?: React.ReactNode;
}) {
  return (
    <div className="flex gap-3 px-4 py-3">
      <div className="mt-0.5 flex h-7 w-7 shrink-0 items-center justify-center rounded-lg border border-line bg-raised text-muted">
        {icon}
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-2">
          <p className="truncate text-[0.83rem] font-medium text-fg" title={title}>
            {title}
          </p>
          {aside && (
            <span className="shrink-0 font-mono text-[0.68rem] text-faint" title={aside}>
              {aside}
            </span>
          )}
        </div>
        {detail && (
          <p className="break-path mt-0.5 text-xs leading-relaxed text-muted">{detail}</p>
        )}
        {progress !== undefined && (
          <Progress value={progress} tone={tone ?? "iris"} className="mt-2" />
        )}
        {footer && <div className="mt-2 flex items-center gap-2">{footer}</div>}
      </div>
    </div>
  );
}

type AuthorityOffer = AdminTransferOffer;

function isAuthorityOffer(offer: NetworkOffer): offer is AuthorityOffer {
  return offer.kind === "principal_transfer";
}

function isSliceOffer(offer: NetworkOffer): offer is TransferOffer {
  return !offer.kind || offer.kind === "slice";
}

function noticeIcon(notice: PluginNotice) {
  switch (notice.level) {
    case "success":
      return <CheckCircle2 className="h-3.5 w-3.5 text-aqua" />;
    case "warning":
      return <Info className="h-3.5 w-3.5 text-amber" />;
    case "error":
      return <X className="h-3.5 w-3.5 text-rose" />;
    default:
      return <Info className="h-3.5 w-3.5" />;
  }
}

function isPendingOffer(offer: NetworkOffer): boolean {
  return !offer.status || offer.status === "pending";
}

export function Notifications() {
  const { t, locale } = useI18n();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const navigate = useNavigate();
  const toast = useToast();

  const jobs = useJobs();
  const transfers = useTransfers();
  const offers = useOffers();
  // Machines asking to join this network (for its admin), and the answers to
  // what this machine asked of other networks.
  const { profile } = useProfile();
  const admissions = useAdmissions(profile === "admin");
  const admissionRequests = useAdmissionRequests();
  const pendingAdmissions = (admissions.data ?? []).filter((record) => record.status === "pending");
  // An answer to something this machine asked. A standing approval (it was
  // admitted before) answers nothing and is shown in the lobby instead.
  const answeredRequests = (admissionRequests.data ?? []).filter((request) => request.status !== "pending" && !request.standing);
  const [joinKey, setJoinKey] = useState<string | null>(null);
  const [deciding, setDeciding] = useState<string | null>(null);

  async function decideAdmission(record: AdmissionRecord, decision: "approve" | "reject") {
    setDeciding(record.node_id);
    try {
      await admissionsApi.decide(record.node_id, decision);
      toast.success(t(decision === "approve" ? "notifications.admissionApproved" : "notifications.admissionRejected"));
      void admissions.refetch();
    } catch (e) {
      toast.fromError(t("notifications.offerFailed"), e);
    } finally {
      setDeciding(null);
    }
  }

  async function joinApproved(key: string) {
    setJoinKey(key);
    try {
      await admissionsApi.join(key);
      window.setTimeout(() => window.location.assign("/admin-ui/"), 1500);
    } catch (e) {
      toast.fromError(t("lobby.joinFailed"), e);
      setJoinKey(null);
    }
  }

  const running = (jobs.data?.jobs ?? []).filter((j) => j.state === "running");
  // O que anda **e** o que acabou de terminar: o desfecho é a metade da
  // notificação que faltava, nos dois sentidos.
  const moving = (transfers.data?.jobs ?? []).filter((transfer) => isWorthNotifying(transfer));
  const waiting = offers.data?.offers ?? [];
  const sliceOffers = waiting.filter(isSliceOffer);
  const authorityOffers = waiting.filter(isAuthorityOffer);
  // Perguntas de plugins (`notification.confirm`): esperam aqui como uma
  // oferta, e a resposta volta à janela do plugin que perguntou.
  const pluginConfirms = usePendingPluginConfirms();
  // Avisos de plugins (`notification.notify`): só mensagem. Contam enquanto
  // não lidos e ficam lidos quando o sino é aberto; o destaque de "novo" vale
  // para esta abertura.
  const pluginNotices = usePluginNotices();
  const unreadNotices = pluginNotices.filter((notice) => !notice.read).length;
  const [freshNotices, setFreshNotices] = useState<ReadonlySet<string>>(new Set());
  // The default administrative password: one notice per start of the node
  // (`boot_id`). Read when the bell opens, gone once dismissed; ignoring it is
  // allowed, so it never blocks anything.
  const access = useAdminAccess();
  const bootId = access.data?.boot_id ?? "";
  const passwordNoticeState = useDefaultPasswordNoticeState(bootId);
  const passwordNotice = Boolean(access.data?.default_password) && passwordNoticeState !== "dismissed";
  const passwordNoticeUnread = passwordNotice && passwordNoticeState === "unread";
  const [passwordNoticeFresh, setPasswordNoticeFresh] = useState(false);
  const count = running.length + moving.length + waiting.length + pluginConfirms.length + unreadNotices
    + pendingAdmissions.length + answeredRequests.length
    + (passwordNoticeUnread ? 1 : 0);
  const hasItems = passwordNotice
    || running.length + moving.length + waiting.length + pluginConfirms.length + pluginNotices.length
      + pendingAdmissions.length + answeredRequests.length > 0;

  useEffect(() => {
    if (!open) {
      setPasswordNoticeFresh(false);
      return;
    }
    if (!passwordNoticeUnread) return;
    setPasswordNoticeFresh(true);
    markDefaultPasswordNoticeRead(bootId);
  }, [open, passwordNoticeUnread, bootId]);

  useEffect(() => {
    if (!open || unreadNotices === 0) return;
    setFreshNotices((current) => new Set([
      ...current,
      ...pluginNotices.filter((notice) => !notice.read).map((notice) => notice.id),
    ]));
    markPluginNoticesRead();
  }, [open, unreadNotices, pluginNotices]);

  useEffect(() => {
    if (!open) setFreshNotices(new Set());
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", onClick);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onClick);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  async function answerOffer(offer: NetworkOffer, accept: boolean) {
    try {
      await api.post(`/api/transfer/offers/${offer.id}/${accept ? "accept" : "reject"}`);
      const kind = offer.kind ?? "slice";
      toast.success(
        accept
          ? kind === "principal_transfer"
            ? t("notifications.principalTransferAccepted")
            : t("notifications.transferStarted")
          : kind === "principal_transfer"
            ? t("notifications.principalTransferRejected")
            : t("notifications.offerRejected"),
      );
      void offers.refetch();
      void transfers.refetch();
    } catch (e) {
      toast.fromError(t("notifications.offerFailed"), e);
    }
  }

  return (
    <div className="relative" ref={ref}>
      <button
        onClick={() => setOpen((o) => !o)}
        aria-label={t("notifications.activityAria", { count })}
        aria-expanded={open}
        className={clsx(
          "relative flex h-9 w-9 items-center justify-center rounded-lg border transition-colors duration-150",
          open
            ? "border-iris/40 bg-iris/12 text-iris"
            : "border-line bg-raised text-muted hover:border-line-strong hover:text-fg",
        )}
      >
        <Bell className="h-4 w-4" />
        {count > 0 && (
          <span className="absolute -right-1 -top-1 flex h-4 min-w-4 items-center justify-center rounded-full bg-iris px-1 font-mono text-[0.6rem] font-bold text-white">
            {count}
          </span>
        )}
      </button>

      <AnimatePresence>
        {open && (
          <motion.div
            initial={{ opacity: 0, y: -6, scale: 0.98 }}
            animate={{ opacity: 1, y: 0, scale: 1 }}
            exit={{ opacity: 0, y: -6, scale: 0.98 }}
            transition={{ duration: 0.16, ease: [0.22, 1, 0.36, 1] }}
            className="absolute right-0 top-11 z-50 w-[min(24rem,calc(100vw-2rem))] origin-top-right overflow-hidden rounded-xl border border-line bg-surface shadow-lift"
          >
            <div className="flex items-center justify-between border-b border-line px-4 py-3">
              <span className="font-display text-sm font-semibold text-fg">
                {t("notifications.title")}
              </span>
              <Badge tone={count ? "iris" : "neutral"} mono>
                {count}
              </Badge>
            </div>

            <div className="max-h-[26rem] divide-y divide-line overflow-y-auto">
              {!hasItems && (
                <Empty icon={<Inbox className="h-5 w-5" />} title={t("notifications.empty")}>
                  {t("notifications.emptyDetail")}
                </Empty>
              )}

              {passwordNotice && (
                <div data-testid="default-password-notice">
                  <Row
                    icon={<ShieldAlert className="h-3.5 w-3.5 text-amber" />}
                    title={t("notifications.defaultPasswordTitle")}
                    detail={t("firstRun.defaultPasswordNotice")}
                    footer={
                      <>
                        {passwordNoticeFresh && <Badge tone="iris">{t("notifications.pluginNoticeNew")}</Badge>}
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={() => {
                            setOpen(false);
                            navigate("/config/preferencias");
                          }}
                        >
                          {t("firstRun.defaultPasswordChange")}
                        </Button>
                        <button
                          type="button"
                          onClick={() => dismissDefaultPasswordNotice(bootId)}
                          aria-label={t("notifications.pluginNoticeDismiss")}
                          title={t("notifications.pluginNoticeDismiss")}
                          className="ml-auto inline-flex h-6 w-6 items-center justify-center rounded-md text-faint hover:bg-line/40 hover:text-fg"
                        >
                          <X className="h-3.5 w-3.5" aria-hidden="true" />
                        </button>
                      </>
                    }
                  />
                </div>
              )}

              {pendingAdmissions.map((record) => (
                <div key={record.node_id} data-testid={`admission-${record.node_id}`}>
                  <Row
                    icon={<UserPlus className="h-3.5 w-3.5 text-aqua" />}
                    title={t("notifications.admissionTitle", { name: record.card.hostname || record.node_id.slice(0, 12) })}
                    aside={record.node_id.slice(0, 12)}
                    detail={admissionDetail(record, t)}
                    footer={
                      <>
                        <Button size="sm" variant="primary" loading={deciding === record.node_id} onClick={() => void decideAdmission(record, "approve")}>
                          {t("notifications.admissionApprove")}
                        </Button>
                        <Button size="sm" variant="ghost" disabled={deciding === record.node_id} onClick={() => void decideAdmission(record, "reject")}>
                          {t("notifications.admissionReject")}
                        </Button>
                        <Button size="sm" variant="ghost" onClick={() => { setOpen(false); navigate("/network-chat"); }}>
                          {t("notifications.admissionTalk")}
                        </Button>
                      </>
                    }
                  />
                </div>
              ))}

              {answeredRequests.map((request) => (
                <div key={request.key} data-testid={`admission-answer-${request.key}`}>
                  <Row
                    icon={<UserPlus className={clsx("h-3.5 w-3.5", request.status === "approved" ? "text-lime" : "text-rose")} />}
                    title={t(request.status === "approved" ? "notifications.requestApproved" : "notifications.requestRejected", { name: request.name || request.key })}
                    detail={
                      request.status === "approved"
                        ? t("lobby.joinDetail")
                        : retryWaitMinutes(request.retry_at) > 0
                          ? t("lobby.admissionRejectedWait", { minutes: retryWaitMinutes(request.retry_at) })
                          : t("lobby.admissionRejectedMayAsk")
                    }
                    footer={
                      <>
                        {request.status === "approved" && (
                          <Button size="sm" variant="primary" loading={joinKey === request.key} onClick={() => void joinApproved(request.key)}>
                            {t("lobby.joinNow")}
                          </Button>
                        )}
                        <Button size="sm" variant="ghost" onClick={() => void admissionsApi.forget(request.key).then(() => admissionRequests.refetch())}>
                          {t("notifications.pluginNoticeDismiss")}
                        </Button>
                      </>
                    }
                  />
                </div>
              ))}

              {pluginNotices.map((notice) => (
                <Row
                  key={notice.id}
                  icon={noticeIcon(notice)}
                  title={notice.title}
                  aside={pluginDisplayName(notice.code, locale)}
                  detail={notice.body || undefined}
                  footer={
                    <>
                      {freshNotices.has(notice.id) && <Badge tone="iris">{t("notifications.pluginNoticeNew")}</Badge>}
                      <span className="font-mono text-[0.7rem] text-faint">
                        {t(notice.source === "network" ? "notifications.pluginNoticeNetwork" : "notifications.pluginNoticeLocal")} · {since(notice.createdAt)}
                      </span>
                      <button
                        type="button"
                        onClick={() => dismissPluginNotice(notice.id)}
                        aria-label={t("notifications.pluginNoticeDismiss")}
                        title={t("notifications.pluginNoticeDismiss")}
                        className="ml-auto inline-flex h-6 w-6 items-center justify-center rounded-md text-faint hover:bg-line/40 hover:text-fg"
                      >
                        <X className="h-3.5 w-3.5" aria-hidden="true" />
                      </button>
                    </>
                  }
                />
              ))}

              {pluginConfirms.map((confirm) => (
                <Row
                  key={confirm.id}
                  icon={<Blocks className="h-3.5 w-3.5" />}
                  title={confirm.request.title}
                  aside={pluginDisplayName(confirm.code, locale)}
                  detail={confirm.request.body || t("notifications.pluginConfirmDetail", { plugin: pluginDisplayName(confirm.code, locale) })}
                  footer={
                    <>
                      <Button
                        size="sm"
                        variant={confirm.request.tone === "danger" ? "danger" : "primary"}
                        icon={<Check className="h-3.5 w-3.5" />}
                        onClick={() => answerPendingPluginConfirm(confirm.id, "accepted")}
                      >
                        {confirm.request.acceptLabel || t("notifications.accept")}
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        icon={<X className="h-3.5 w-3.5" />}
                        onClick={() => answerPendingPluginConfirm(confirm.id, "cancelled")}
                      >
                        {confirm.request.cancelLabel || t("notifications.reject")}
                      </Button>
                    </>
                  }
                />
              ))}

              {running.map((j) => (
                <Row
                  key={j.id}
                  icon={<Scissors className="h-3.5 w-3.5" />}
                  title={localizeServerText(locale, j.title ?? j.step)}
                  detail={j.message ? localizeServerText(locale, j.message) : undefined}
                  progress={j.progress}
                  footer={
                    <span className="font-mono text-[0.7rem] text-faint">
                      {t("notifications.started")} {since(j.started_at)}
                    </span>
                  }
                />
              ))}

              {moving.map((job) => {
                const look = transferLook(job);
                const done = isFinishedTransfer(job.state);
                return (
                  <Row
                    key={job.id}
                    icon={look.icon}
                    title={transferTitle(job)}
                    aside={transferHash(job.name)}
                    detail={[
                      t(
                        job.direction === "sending"
                          ? "notifications.transferTo"
                          : "notifications.transferFrom",
                        { host: job.peer_host },
                      ),
                      // O motivo importa quando deu errado, e a etapa enquanto
                      // corre. Num `done` o detalhe é o caminho de destino —
                      // ruído para um resumo, e a tela de Transferências o tem.
                      job.state === "done" ? "" : localizeServerText(locale, job.detail),
                    ]
                      .filter(Boolean)
                      .join(" · ")}
                    // Uma transferência terminada não tem barra: ela não está
                    // mais andando, e uma barra parada em 100% sugere que sim.
                    progress={
                      done
                        ? undefined
                        : job.bytes_total > 0
                          ? (job.bytes_done / job.bytes_total) * 100
                          : null
                    }
                    tone={look.tone}
                    footer={
                      <span className="font-mono text-[0.7rem] text-faint">
                        {done
                          ? t(`notifications.state.${job.state}`, {
                              size: bytes(job.bytes_total),
                            })
                          : t("notifications.downloadProgress", {
                              done: bytes(job.bytes_done),
                              total: bytes(job.bytes_total),
                              rate: rate(job.bytes_per_sec),
                              remaining: job.eta_secs != null ? duration(job.eta_secs) : "",
                            })}
                      </span>
                    }
                  />
                );
              })}

              {sliceOffers.map((o) => (
                <Row
                  key={o.id}
                  icon={<Inbox className="h-3.5 w-3.5" />}
                  // O que está sendo oferecido é o título; quem oferece e
                  // quanto pesa são o subtítulo. Ao contrário, "atena quer
                  // enviar qwen3:8b · L0–15" não cabe numa linha e a faixa de
                  // camadas — a informação que decide — é justamente o que a
                  // reticência come.
                  title={transferTitle({
                    name: o.name,
                    model_label: o.pom?.model.label,
                    start_layer: o.start_layer,
                    end_layer: o.end_layer,
                  })}
                  aside={transferHash(o.name)}
                  // O caminho completo na máquina de origem enchia cinco
                  // linhas do painel e não ajuda a decidir. O tamanho, sim: é
                  // o que se aceita gravar em disco. O caminho continua na
                  // tela de Transferências, onde há largura para ele.
                  detail={t("notifications.offerTitle", {
                    host: o.from_host,
                    size: bytes(o.size_bytes),
                  })}
                  footer={
                    isPendingOffer(o) ? (
                      <>
                        <Button
                          size="sm"
                          variant="primary"
                          icon={<Check className="h-3.5 w-3.5" />}
                          onClick={() => void answerOffer(o, true)}
                        >
                          {t("notifications.accept")}
                        </Button>
                        <Button
                          size="sm"
                          variant="ghost"
                          icon={<X className="h-3.5 w-3.5" />}
                          onClick={() => void answerOffer(o, false)}
                        >
                          {t("notifications.reject")}
                        </Button>
                      </>
                    ) : undefined
                  }
                />
              ))}

              {authorityOffers.map((offer) => {
                const pending = isPendingOffer(offer);
                return (
                  <Row
                    key={offer.id}
                    icon={<Inbox className="h-3.5 w-3.5" />}
                    title={
                      pending
                        ? t("notifications.principalTransferTitle")
                        : offer.status === "accepted"
                          ? t("notifications.principalTransferAccepted")
                          : t("notifications.principalTransferRejected")
                    }
                    detail={
                      pending
                        ? t("notifications.principalTransferDetail", { host: offer.from_host })
                        : offer.status === "accepted"
                          ? t("notifications.principalTransferAcceptedDetail", { host: offer.from_host })
                          : t("notifications.principalTransferRejectedDetail", { host: offer.from_host })
                    }
                    tone="iris"
                    footer={
                      pending ? (
                        <>
                          <Button
                            size="sm"
                            variant="primary"
                            icon={<Check className="h-3.5 w-3.5" />}
                            onClick={() => void answerOffer(offer, true)}
                          >
                            {t("notifications.accept")}
                          </Button>
                          <Button
                            size="sm"
                            variant="ghost"
                            icon={<X className="h-3.5 w-3.5" />}
                            onClick={() => void answerOffer(offer, false)}
                          >
                            {t("notifications.reject")}
                          </Button>
                        </>
                      ) : undefined
                    }
                  />
                );
              })}
            </div>

            <button
              onClick={() => {
                setOpen(false);
                navigate(authorityOffers.length > 0 ? "/config/administracao" : "/transferencias");
              }}
              className="w-full border-t border-line px-4 py-2.5 text-center text-[0.8rem] text-muted transition-colors hover:bg-line/40 hover:text-fg"
            >
              {t(authorityOffers.length > 0 ? "notifications.viewAdministration" : "notifications.allTransfers")}
            </button>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
