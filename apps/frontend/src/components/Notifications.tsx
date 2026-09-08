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
  Check,
  CheckCircle2,
  Download,
  Inbox,
  Info,
  Scissors,
  Upload,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "../lib/api";
import { bytes, duration, rate, since } from "../lib/format";
import { useJobs, useOffers, useTransfers } from "../lib/queries";
import { useToast } from "../lib/toast";
import { isFinishedTransfer, isWorthNotifying } from "../lib/transferStatus";
import { transferHash, transferTitle } from "../lib/transferLabel";
import type {
  AdminTransferOffer,
  NetworkOffer,
  NodePromotionOffer,
  NodeRelinquishNotice,
  TransferJob,
  TransferOffer,
} from "../lib/types";
import { Badge, Button, Empty, Progress } from "./ui";
import { useI18n } from "../i18n/provider";
import { localizeServerText } from "../i18n/locales";

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

type AuthorityOffer = AdminTransferOffer | NodePromotionOffer | NodeRelinquishNotice;

function isAuthorityOffer(offer: NetworkOffer): offer is AuthorityOffer {
  return offer.kind === "principal_transfer"
    || offer.kind === "node_promotion"
    || offer.kind === "node_relinquish_notice";
}

function isSliceOffer(offer: NetworkOffer): offer is TransferOffer {
  return !offer.kind || offer.kind === "slice";
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

  const running = (jobs.data?.jobs ?? []).filter((j) => j.state === "running");
  // O que anda **e** o que acabou de terminar: o desfecho é a metade da
  // notificação que faltava, nos dois sentidos.
  const moving = (transfers.data?.jobs ?? []).filter((transfer) => isWorthNotifying(transfer));
  const waiting = offers.data?.offers ?? [];
  const sliceOffers = waiting.filter(isSliceOffer);
  const authorityOffers = waiting.filter(isAuthorityOffer);
  const count = running.length + moving.length + waiting.length;

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
            : kind === "node_promotion"
              ? t("notifications.nodePromotionAccepted")
              : t("notifications.transferStarted")
          : kind === "principal_transfer"
            ? t("notifications.principalTransferRejected")
            : kind === "node_promotion"
              ? t("notifications.nodePromotionRejected")
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
              {count === 0 && (
                <Empty icon={<Inbox className="h-5 w-5" />} title={t("notifications.empty")}>
                  {t("notifications.emptyDetail")}
                </Empty>
              )}

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
                const isTransfer = offer.kind === "principal_transfer";
                const isPromotion = offer.kind === "node_promotion";
                return (
                  <Row
                    key={offer.id}
                    icon={
                      offer.kind === "node_relinquish_notice" ? (
                        <Info className="h-3.5 w-3.5 text-amber" />
                      ) : (
                        <Inbox className="h-3.5 w-3.5" />
                      )
                    }
                    title={
                      isTransfer
                        ? pending
                          ? t("notifications.principalTransferTitle")
                          : offer.status === "accepted"
                            ? t("notifications.principalTransferAccepted")
                            : t("notifications.principalTransferRejected")
                        : isPromotion
                          ? pending
                            ? t("notifications.nodePromotionTitle")
                            : offer.status === "accepted"
                              ? t("notifications.nodePromotionAccepted")
                              : t("notifications.nodePromotionRejected")
                          : t("notifications.nodeRelinquishTitle")
                    }
                    detail={
                      isTransfer
                        ? pending
                          ? t("notifications.principalTransferDetail", { host: offer.from_host })
                          : offer.status === "accepted"
                            ? t("notifications.principalTransferAcceptedDetail", { host: offer.from_host })
                            : t("notifications.principalTransferRejectedDetail", { host: offer.from_host })
                        : isPromotion
                          ? pending
                            ? t("notifications.nodePromotionDetail", { host: offer.from_host })
                            : offer.status === "accepted"
                              ? t("notifications.nodePromotionAcceptedDetail", { host: offer.from_host })
                              : t("notifications.nodePromotionRejectedDetail", { host: offer.from_host })
                          : t("notifications.nodeRelinquishDetail", { host: offer.from_host })
                    }
                    tone={offer.kind === "node_relinquish_notice" ? "amber" : "iris"}
                    footer={
                      pending && (isTransfer || isPromotion) ? (
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
