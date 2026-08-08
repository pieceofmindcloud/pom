/**
 * Requests as they land.
 *
 * Same source as the tallies next door, read on a short interval and rendered
 * newest-first with the new lines animating in — the point of this screen is to
 * watch a harness hit the cluster and see which machines answered.
 */
import { AnimatePresence, motion } from "framer-motion";
import { Activity, Pause, Play, Radio } from "lucide-react";
import { useState } from "react";
import { HopChain } from "./LogsByToken";
import {
  Badge,
  Button,
  Dot,
  Empty,
  ErrorState,
  Footnote,
  Mono,
  PageHeader,
  Panel,
  PanelHeader,
  Skeleton,
} from "../components/ui";
import { clock, ms, num } from "../lib/format";
import { useCluster, useRequestLog } from "../lib/queries";
import type { RequestLogEntry } from "../lib/types";

export function LogsLive() {
  const [live, setLive] = useState(true);
  const cluster = useCluster();
  const log = useRequestLog();

  const names = new Map<string, string>();
  for (const p of cluster.data?.peers ?? []) {
    names.set(p.node_id, p.node?.hostname || p.short_id);
  }
  const name = (id: string) => names.get(id) ?? `${id.slice(0, 8)}…`;

  // Pausing freezes the list without stopping the query: resuming shows what
  // arrived meanwhile instead of a gap.
  const [frozen, setFrozen] = useState<RequestLogEntry[] | null>(null);
  const entries = live ? (log.data?.entries ?? []) : (frozen ?? []);

  function toggle() {
    if (live) setFrozen(log.data?.entries ?? []);
    setLive((l) => !l);
  }

  return (
    <>
      <PageHeader
        title="Requisições em tempo real"
        subtitle="Cada linha é uma geração que a rede atendeu, com o caminho que ela percorreu entre as máquinas."
        actions={
          <Button
            icon={live ? <Pause className="h-3.5 w-3.5" /> : <Play className="h-3.5 w-3.5" />}
            onClick={toggle}
          >
            {live ? "Pausar" : "Retomar"}
          </Button>
        }
      />

      {log.isError && <ErrorState error={log.error} onRetry={() => void log.refetch()} />}
      {log.isLoading && <Skeleton className="h-64 w-full" />}

      {log.data && (
        <Panel>
          <PanelHeader
            title="Fluxo"
            actions={
              <Badge tone={live ? "aqua" : "neutral"}>
                <Dot tone={live ? "aqua" : "neutral"} pulse={live} />
                {live ? "ao vivo" : "pausado"}
              </Badge>
            }
          />
          {entries.length === 0 ? (
            <Empty icon={<Radio className="h-5 w-5" />} title="Silêncio na rede">
              Nada chegou ainda. Mande uma mensagem pelo chat de teste ou aponte um harness
              para <span className="font-mono">/v1</span> nesta porta.
            </Empty>
          ) : (
            <div className="max-h-[62vh] divide-y divide-line overflow-y-auto">
              <AnimatePresence initial={false}>
                {entries.map((e) => (
                  <motion.div
                    key={e.id}
                    layout
                    initial={{ opacity: 0, backgroundColor: "rgb(var(--iris) / 0.10)" }}
                    animate={{ opacity: 1, backgroundColor: "rgb(var(--iris) / 0)" }}
                    transition={{ duration: 0.9, ease: [0.22, 1, 0.36, 1] }}
                    className="flex flex-wrap items-center gap-x-4 gap-y-1.5 px-5 py-2.5"
                  >
                    <Mono className="shrink-0 text-faint">{clock(e.at)}</Mono>
                    <Badge
                      tone={
                        e.status === "ok" ? "aqua" : e.status === "partial" ? "amber" : "rose"
                      }
                      className="shrink-0"
                    >
                      {e.status}
                    </Badge>
                    <span className="shrink-0 text-[0.8rem] text-fg">{e.token_label}</span>
                    <div className="min-w-[12rem] flex-1">
                      <HopChain entry={e} name={name} />
                    </div>
                    <Mono className="shrink-0 text-muted">
                      {num(e.completion_tokens ?? 0)} tok · {ms(e.duration_ms)}
                    </Mono>
                    {e.error && (
                      <p className="break-path w-full text-[0.75rem] text-rose">{e.error}</p>
                    )}
                  </motion.div>
                ))}
              </AnimatePresence>
            </div>
          )}
          <Footnote
            className="border-t border-line px-5 py-3"
            icon={<Activity className="h-3.5 w-3.5" />}
          >
            Uma linha com dois ou mais saltos é a prova de que a inferência foi conjunta.
            Um salto só, ou nenhum, significa que o planejador não fechou a cobertura.
          </Footnote>
        </Panel>
      )}
    </>
  );
}
