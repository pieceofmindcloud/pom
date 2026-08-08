/**
 * What this node is busy with right now.
 *
 * Everything listed here is read back from the node — slicing runs from
 * `GET /api/ui/jobs`, downloads from `GET /api/transfer/jobs`, pending offers
 * from `GET /api/transfer/offers`. None of it is browser state, so reloading
 * the page in the middle of a 17-minute transfer shows the same bar at the same
 * percentage instead of losing it.
 */
import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import { Bell, Check, Download, Inbox, Scissors, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "../lib/api";
import { bytes, duration, rate, since } from "../lib/format";
import { useJobs, useOffers, useTransfers } from "../lib/queries";
import { useToast } from "../lib/toast";
import { Badge, Button, Empty, Progress } from "./ui";

function Row({
  icon,
  title,
  detail,
  progress,
  tone,
  footer,
}: {
  icon: React.ReactNode;
  title: string;
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
        <p className="truncate text-[0.83rem] font-medium text-fg" title={title}>
          {title}
        </p>
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

export function Notifications() {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const navigate = useNavigate();
  const toast = useToast();

  const jobs = useJobs();
  const transfers = useTransfers();
  const offers = useOffers();

  const running = (jobs.data?.jobs ?? []).filter((j) => j.state === "running");
  const downloads = (transfers.data?.jobs ?? []).filter(
    (t) => t.state !== "done" && t.state !== "failed",
  );
  const waiting = offers.data?.offers ?? [];
  const count = running.length + downloads.length + waiting.length;

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

  async function answerOffer(id: string, accept: boolean) {
    try {
      await api.post(`/api/transfer/offers/${id}/${accept ? "accept" : "reject"}`);
      toast.success(accept ? "Transferência iniciada" : "Oferta recusada");
      void offers.refetch();
      void transfers.refetch();
    } catch (e) {
      toast.fromError("A oferta não pôde ser respondida", e);
    }
  }

  return (
    <div className="relative" ref={ref}>
      <button
        onClick={() => setOpen((o) => !o)}
        aria-label={count ? `${count} atividades em andamento` : "Nada em andamento"}
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
                Em andamento
              </span>
              <Badge tone={count ? "iris" : "neutral"} mono>
                {count}
              </Badge>
            </div>

            <div className="max-h-[26rem] divide-y divide-line overflow-y-auto">
              {count === 0 && (
                <Empty icon={<Inbox className="h-5 w-5" />} title="Nada acontecendo">
                  Fatiamentos e transferências aparecem aqui enquanto rodam, mesmo depois
                  de recarregar a página.
                </Empty>
              )}

              {running.map((j) => (
                <Row
                  key={j.id}
                  icon={<Scissors className="h-3.5 w-3.5" />}
                  title={j.title ?? j.step}
                  detail={j.message || undefined}
                  progress={j.progress}
                  footer={
                    <span className="font-mono text-[0.7rem] text-faint">
                      começou {since(j.started_at)}
                    </span>
                  }
                />
              ))}

              {downloads.map((t) => (
                <Row
                  key={t.id}
                  icon={<Download className="h-3.5 w-3.5" />}
                  title={t.name}
                  detail={`de ${t.from_host} · ${t.detail}`}
                  progress={
                    t.bytes_total > 0 ? (t.bytes_done / t.bytes_total) * 100 : null
                  }
                  tone="aqua"
                  footer={
                    <span className="font-mono text-[0.7rem] text-faint">
                      {bytes(t.bytes_done)} de {bytes(t.bytes_total)} · {rate(t.bytes_per_sec)}
                      {t.eta_secs != null && ` · faltam ${duration(t.eta_secs)}`}
                    </span>
                  }
                />
              ))}

              {waiting.map((o) => (
                <Row
                  key={o.id}
                  icon={<Inbox className="h-3.5 w-3.5" />}
                  title={`${o.from_host} quer enviar ${o.name}`}
                  detail={`${bytes(o.size_bytes)} · ${o.remote_path}`}
                  footer={
                    <>
                      <Button
                        size="sm"
                        variant="primary"
                        icon={<Check className="h-3.5 w-3.5" />}
                        onClick={() => answerOffer(o.id, true)}
                      >
                        Aceitar
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        icon={<X className="h-3.5 w-3.5" />}
                        onClick={() => answerOffer(o.id, false)}
                      >
                        Recusar
                      </Button>
                    </>
                  }
                />
              ))}
            </div>

            <button
              onClick={() => {
                setOpen(false);
                navigate("/transferencias");
              }}
              className="w-full border-t border-line px-4 py-2.5 text-center text-[0.8rem] text-muted transition-colors hover:bg-line/40 hover:text-fg"
            >
              Ver todas as transferências
            </button>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
