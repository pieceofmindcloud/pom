import clsx from "clsx";
import { useCallback, useEffect, useRef, useState } from "react";
import { ArrowRight, Info, Maximize2, ServerCog, X } from "lucide-react";
import { Modal } from "./Confirm";
import { Button } from "./ui";
import { useI18n } from "../i18n/provider";
import {
  GATEWAY_HEIGHT,
  GATEWAY_WIDTH,
  type CanvasPosition,
} from "../lib/networkLayout";

/** Custo de um ciclo lógico: entrada no primeiro peer, avanço entre peers e retorno. */
export type PipelineCycleCost = {
  /** Todas as pernas Gateway -> cabeça, avanço e cauda -> Gateway. */
  legsMs: Array<number | null>;
  /** Compute dos peers que participaram do ciclo. */
  computeMs: number | null;
};

export type PipelineRouteCost = {
  /** Nós do head à cauda, com o gateway virtual no fim da rota. */
  nodeIds: string[];
  /** Gateway -> head, handoffs e retorno da cauda, nessa ordem. */
  legsMs: Array<number | null>;
  /** Compute só existe quando a rota foi observada numa execução. */
  computeMs: number | null;
  /** Soma do RTT calculada pelo backend, quando todas as pernas existem. */
  rttMs?: number | null;
  /**
   * Rede paga por token no modo de volta escolhido pelo planejador. Não é a
   * soma das pernas: na resposta direta cada perna custa meio RTT, e no anel a
   * volta da cauda não é paga.
   */
  networkMs?: number | null;
  /** Um token pela rota, calculado pelo backend: rede no modo escolhido mais compute. */
  totalMs?: number | null;
  returnMode?: "direct" | "ring" | null;
  /** Alguma perna da rota passa por um circuito num relé libp2p. */
  relayed?: boolean;
  /** Quais pernas, como `[de, para]`. */
  relayedLegs?: Array<[string, string]>;
  /** A volta direta passaria por relé e o anel a contorna. */
  relayBypassedByRing?: boolean;
};

/** Os primeiros caracteres do `node_id`, como no resto do mapa. */
function shortId(nodeId: string) {
  return nodeId.slice(0, 8);
}

/** Soma somente uma decomposição completa, sem transformar ausência em zero. */
export function pipelineCycleTotalMs(cost: PipelineCycleCost | undefined): number | null {
  const computeMs = cost?.computeMs;
  if (!cost || typeof computeMs !== "number" || !Number.isFinite(computeMs) || computeMs < 0) {
    return null;
  }
  const legs = cost.legsMs.filter((leg): leg is number => typeof leg === "number" && Number.isFinite(leg) && leg >= 0);
  if (legs.length !== cost.legsMs.length) {
    return null;
  }
  return legs.reduce((sum, leg) => sum + leg, computeMs);
}

function validMs(value: number | null | undefined): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

/**
 * Tempo de um token pela rota. Vem do backend, que usa a conta do planejador;
 * somar as pernas aqui contaria ida e volta inteiras de cada aresta. Sem
 * compute medido, a rede sozinha ainda ordena as rotas.
 */
export function pipelineRouteTotalMs(route: PipelineRouteCost): number | null {
  if (validMs(route.totalMs)) return route.totalMs;
  if (validMs(route.networkMs)) return route.networkMs;
  return null;
}

/**
 * Uma linha compacta dizendo que a rota passa por relé (ou que o anel o
 * contorna); o ícone abre o card com quais pernas, se o POM contorna e como
 * melhorar.
 */
function RelayNotice({ route }: { route: PipelineRouteCost }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const container = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = (event: PointerEvent) => {
      if (!container.current?.contains(event.target as Node)) setOpen(false);
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", close);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("pointerdown", close);
      document.removeEventListener("keydown", escape);
    };
  }, [open]);
  const legs = (route.relayedLegs ?? []).map(([from, to]) => `${shortId(from)} → ${shortId(to)}`).join(", ");
  const tail = route.nodeIds.at(-2);
  const gateway = route.nodeIds.at(-1);
  const blocking = route.relayed === true;
  return (
    <div ref={container} className="relative mt-2" onPointerDown={(event) => event.stopPropagation()}>
      <div
        data-testid="pipeline-relay-warning"
        className={`flex items-center gap-1.5 rounded-md border-2 border-relay px-2 py-1 text-[10px] font-semibold ${blocking ? "bg-relay text-white" : "bg-relay/10 text-relay"}`}
      >
        <span className="uppercase tracking-[0.08em]">{t("canvas.relayBadge")}</span>
        <span className="min-w-0 flex-1 truncate font-normal">{blocking ? t("canvas.relayRouteTitle") : t("canvas.relayBypassedChip")}</span>
        <button
          type="button"
          data-testid="pipeline-relay-info"
          aria-label={t("canvas.relayDetails")}
          aria-expanded={open}
          title={t("canvas.relayDetails")}
          onClick={() => setOpen((value) => !value)}
          className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full hover:bg-black/15 focus:outline-none focus-visible:ring-2 focus-visible:ring-white"
        >
          <Info className="h-4 w-4" aria-hidden="true" />
        </button>
      </div>
      {open && (
        <div
          role="dialog"
          aria-label={t("canvas.relayDetails")}
          data-testid="pipeline-relay-popup"
          className="absolute right-0 top-full z-30 mt-1.5 w-72 rounded-lg border-2 border-relay bg-ink p-3 text-[11px] leading-snug text-fg shadow-panel"
        >
          <div className="flex items-start justify-between gap-2">
            <div className="font-semibold uppercase tracking-[0.1em] text-relay">
              {blocking ? t("canvas.relayRouteTitle") : t("canvas.relayBypassedChip")}
            </div>
            <button type="button" aria-label={t("canvas.relayClose")} onClick={() => setOpen(false)} className="text-muted hover:text-fg">
              <X className="h-3.5 w-3.5" aria-hidden="true" />
            </button>
          </div>
          {legs && (
            <p data-testid="pipeline-relay-legs" className="mt-1.5 font-mono">
              {t("canvas.relayLegs", { legs })}
            </p>
          )}
          {route.relayBypassedByRing && tail && gateway && (
            <p data-testid="pipeline-relay-bypass" className="mt-1.5">
              {t("canvas.relayBypassed", { from: shortId(tail), to: shortId(gateway) })}
            </p>
          )}
          {blocking && <p className="mt-1.5">{t("canvas.relayNotBypassed")}</p>}
          <p className="mt-1.5 text-muted">{t("canvas.relayRouteHint")}</p>
        </div>
      )}
    </div>
  );
}

/** Rotas da mais barata para a mais cara; sem medida, vão para o fim em ordem estável. */
export function orderPipelineRoutes(routes: PipelineRouteCost[]) {
  return routes
    .map((route) => ({ route, totalMs: pipelineRouteTotalMs(route) }))
    .sort((left, right) => {
      if (left.totalMs != null && right.totalMs != null) {
        const byTotal = left.totalMs - right.totalMs;
        if (byTotal !== 0) return byTotal;
      } else if (left.totalMs != null) {
        return -1;
      } else if (right.totalMs != null) {
        return 1;
      }
      return left.route.nodeIds.join("\u0000").localeCompare(right.route.nodeIds.join("\u0000"));
    });
}

/** O nome que o resto do mapa usa; o gateway virtual é chamado pelo que é. */
type NameOf = (nodeId: string) => string;

/**
 * Uma rota inteira, perna por perna: de onde o token sai, por onde passa e
 * quanto cada trecho custou. A primeira perna é a entrada (gateway -> cabeça),
 * a última é a volta da cauda.
 */
function RouteDetail({ route, totalMs, best, nameOf }: { route: PipelineRouteCost; totalMs: number | null; best: boolean; nameOf: NameOf }) {
  const { t } = useI18n();
  const hops = ["gateway", ...route.nodeIds];
  const relayed = new Set((route.relayedLegs ?? []).map(([from, to]) => `${from}\u0000${to}`));
  return (
    <li
      data-testid="pipeline-route"
      className={clsx(
        "rounded-lg border p-3",
        route.relayed ? "border-relay/60 bg-relay/5" : best ? "border-aqua/40 bg-aqua/5" : "border-line bg-raised/40",
      )}
    >
      <div className="flex flex-wrap items-center gap-1.5">
        {route.nodeIds.map((nodeId, index) => (
          <span key={`${nodeId}-${index}`} className="flex items-center gap-1.5">
            {index > 0 && <ArrowRight className="h-3 w-3 text-faint" aria-hidden="true" />}
            <span className="rounded-md border border-line bg-ink px-1.5 py-0.5 font-mono text-[11px] text-fg">{nameOf(nodeId)}</span>
          </span>
        ))}
        <span className="ml-auto flex items-center gap-1.5">
          {best && <span className="rounded bg-aqua/15 px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-[0.08em] text-aqua">{t("canvas.gatewayRouteBest")}</span>}
          {route.relayed && <span data-testid="pipeline-route-relay" className="rounded bg-relay px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-[0.08em] text-white">{t("canvas.relayBadge")}</span>}
          <span className={clsx("font-mono text-sm font-semibold tabular-nums", route.relayed ? "text-relay" : "text-aqua")}>
            {totalMs == null ? t("canvas.pipelineTotalUnknown") : `${totalMs.toFixed(1)} ms`}
          </span>
        </span>
      </div>
      <div className="mt-2 flex flex-wrap gap-x-4 gap-y-1 font-mono text-[11px] tabular-nums text-muted">
        {validMs(route.networkMs) && <span>{t("canvas.gatewayNetwork")} {route.networkMs.toFixed(1)} ms</span>}
        {validMs(route.computeMs) && <span>{t("canvas.gatewayCompute")} {route.computeMs.toFixed(1)} ms</span>}
        {route.returnMode && (
          <span title={route.returnMode === "direct" ? t("canvas.returnDirectHint") : t("canvas.returnRingHint")}>
            {route.returnMode === "direct" ? t("canvas.returnDirect") : t("canvas.returnRing")}
          </span>
        )}
      </div>
      {route.legsMs.length > 0 && (
        <ol className="mt-2 grid gap-1 sm:grid-cols-2" aria-label={t("canvas.gatewayLegs")}>
          {route.legsMs.map((leg, index) => {
            const from = hops[index];
            const to = hops[index + 1] ?? "gateway";
            const isRelay = relayed.has(`${from}\u0000${to}`);
            return (
              <li key={index} className={clsx("flex items-center justify-between gap-2 rounded px-2 py-1 font-mono text-[10px] tabular-nums", isRelay ? "bg-relay/15 text-relay" : "bg-ink/60 text-muted")}>
                <span className="min-w-0 truncate">{nameOf(from)} → {nameOf(to)}</span>
                <span className="shrink-0">{validMs(leg) ? `${leg.toFixed(1)} ms` : "-"}</span>
              </li>
            );
          })}
        </ol>
      )}
      {(route.relayed || route.relayBypassedByRing) && (
        <p className="mt-2 text-[11px] leading-snug text-muted">
          {route.relayBypassedByRing && route.nodeIds.length >= 2
            ? t("canvas.relayBypassed", { from: nameOf(route.nodeIds.at(-2)!), to: nameOf(route.nodeIds.at(-1)!) })
            : t("canvas.relayNotBypassed")}
        </p>
      )}
    </li>
  );
}

function Stat({ label, value, tone = "text-fg" }: { label: string; value: string; tone?: string }) {
  return (
    <div className="rounded-lg border border-line bg-raised/50 px-3 py-2">
      <div className="text-[9px] uppercase tracking-[0.12em] text-faint">{label}</div>
      <div className={clsx("mt-0.5 font-mono text-sm font-semibold tabular-nums", tone)}>{value}</div>
    </div>
  );
}

/**
 * O gateway no mapa: o fim de cada ciclo de token e o resumo do que ele custa.
 *
 * O card fica pequeno de propósito - o número que importa (tempo por token) e
 * o aviso de relé. A lista de rotas, que numa rede com réplicas cresce com o
 * produto das colunas, foi para a janela que o botão do card (ou o duplo
 * clique) abre: dentro do card ela empurrava a altura e soltava as arestas.
 *
 * Arrastar é opcional: quem passa os handlers (o Fluxo do Painel) move o
 * gateway como uma máquina; o mapa da implantação continua com ele fixo.
 */
export function PipelineGatewayNode({
  position,
  cost,
  routes = [],
  dragging = false,
  nameOf = (nodeId) => nodeId,
  onPointerDown,
  onPointerMove,
  onPointerUp,
}: {
  position: CanvasPosition;
  cost?: PipelineCycleCost;
  routes?: PipelineRouteCost[];
  dragging?: boolean;
  nameOf?: NameOf;
  onPointerDown?: (event: React.PointerEvent<HTMLElement>) => void;
  onPointerMove?: (event: React.PointerEvent<HTMLElement>) => void;
  onPointerUp?: (event: React.PointerEvent<HTMLElement>) => void;
}) {
  const { t } = useI18n();
  const [detailsOpen, setDetailsOpen] = useState(false);
  const closeDetails = useCallback(() => setDetailsOpen(false), []);
  const label = (nodeId: string) => (nodeId === "gateway" ? t("canvas.gateway") : nameOf(nodeId));
  const totalMs = pipelineCycleTotalMs(cost);
  const orderedRoutes = orderPipelineRoutes(routes);
  const routeTotalMs = orderedRoutes[0]?.totalMs ?? null;
  const bestRoute = orderedRoutes[0]?.route;
  const displayedTotalMs = orderedRoutes.length > 0 ? routeTotalMs : totalMs;
  const networkMs = cost?.legsMs.every((leg): leg is number => leg != null && Number.isFinite(leg) && leg >= 0)
    ? cost.legsMs.reduce((sum, leg) => sum + leg, 0)
    : null;
  const computeMs = cost?.computeMs != null && Number.isFinite(cost.computeMs) && cost.computeMs >= 0
    ? cost.computeMs
    : null;
  // Com o compute medido, o total é o tempo de um token pela rota.
  const throughput = displayedTotalMs != null && displayedTotalMs > 0 && (orderedRoutes.length === 0 ? computeMs != null : validMs(bestRoute?.totalMs))
    ? 1000 / displayedTotalMs
    : null;
  const breakdown = bestRoute && validMs(bestRoute.networkMs) && validMs(bestRoute.computeMs)
    ? t("canvas.pipelineRouteBreakdown", {
      network: bestRoute.networkMs.toFixed(1),
      mode: bestRoute.returnMode === "direct" ? t("canvas.returnDirect") : t("canvas.returnRing"),
      compute: bestRoute.computeMs.toFixed(1),
    })
    : orderedRoutes.length === 0 && networkMs != null && computeMs != null
      ? t("canvas.pipelineCostBreakdown", { network: networkMs.toFixed(1), compute: computeMs.toFixed(1) })
      : null;
  const draggable = onPointerDown != null;

  return (
    <>
      <article
        data-testid="pipeline-gateway"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerUp}
        onDoubleClick={(event) => { if (!(event.target instanceof Element && event.target.closest("button"))) setDetailsOpen(true); }}
        className={clsx(
          "absolute select-none rounded-xl border bg-ink/95 p-3 shadow-panel backdrop-blur transition-[box-shadow,border-color]",
          dragging ? "cursor-grabbing border-aqua shadow-lift" : clsx("border-aqua/35 hover:border-aqua/70 hover:shadow-lift", draggable && "cursor-grab"),
        )}
        style={{ left: position.left, top: position.top, width: GATEWAY_WIDTH, minHeight: GATEWAY_HEIGHT, touchAction: draggable ? "none" : undefined }}
      >
        <div className="absolute inset-x-3 top-0 h-px bg-aqua/70" />
        <div className="flex items-start gap-2">
          <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-aqua/25 bg-aqua/10 text-aqua">
            <ServerCog className="h-4 w-4" />
          </span>
          <div className="min-w-0 flex-1">
            <h3 className="truncate text-sm font-semibold text-fg">{t("canvas.gateway")}</h3>
            <p className="truncate text-[10px] text-faint">
              {orderedRoutes.length > 0 ? t("canvas.pipelineRouteCount", { count: orderedRoutes.length }) : t("canvas.gatewayReturn")}
            </p>
          </div>
          <button
            type="button"
            data-testid="pipeline-gateway-open"
            aria-label={t("canvas.gatewayOpenDetails")}
            title={t("canvas.gatewayOpenDetails")}
            onPointerDown={(event) => event.stopPropagation()}
            onClick={() => setDetailsOpen(true)}
            className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted hover:bg-raised hover:text-fg"
          >
            <Maximize2 className="h-3.5 w-3.5" aria-hidden="true" />
          </button>
        </div>
        <div className="mt-2.5 flex items-end justify-between gap-2 rounded-lg border border-aqua/20 bg-aqua/[0.07] px-2.5 py-2">
          <div className="min-w-0">
            <div className="text-[9px] uppercase tracking-[0.12em] text-faint">{t("canvas.pipelineTotalLabel")}</div>
            <div data-testid="pipeline-total" className="mt-0.5 font-mono text-base font-semibold leading-tight tabular-nums text-aqua">
              {displayedTotalMs == null ? t("canvas.pipelineTotalUnknown") : `${displayedTotalMs.toFixed(1)} ms`}
            </div>
          </div>
          {throughput != null && (
            <span data-testid="pipeline-throughput-badge" title={t("canvas.pipelineThroughput", { tps: throughput.toFixed(1) })} className="shrink-0 rounded-md bg-aqua/15 px-1.5 py-0.5 font-mono text-[10px] font-semibold tabular-nums text-aqua">
              {throughput.toFixed(1)} tok/s
            </span>
          )}
        </div>
        {breakdown && <p className="mt-1.5 truncate font-mono text-[10px] tabular-nums text-muted" title={breakdown}>{breakdown}</p>}
        {bestRoute && (bestRoute.relayed || bestRoute.relayBypassedByRing) && <RelayNotice route={bestRoute} />}
      </article>
      <Modal
        open={detailsOpen}
        size="lg"
        title={t("canvas.gatewayDialogTitle")}
        onClose={closeDetails}
        footer={<Button variant="ghost" onClick={closeDetails}>{t("common.close")}</Button>}
      >
        <div data-testid="pipeline-gateway-details" className="space-y-4">
          <p>{t("canvas.gatewayIntro")}</p>
          <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
            <Stat label={t("canvas.gatewayPerToken")} value={displayedTotalMs == null ? t("canvas.pipelineTotalUnknown") : `${displayedTotalMs.toFixed(1)} ms`} tone="text-aqua" />
            <Stat label={t("canvas.gatewayThroughput")} value={throughput == null ? "-" : `${throughput.toFixed(1)} tok/s`} />
            <Stat label={t("canvas.gatewayNetwork")} value={validMs(bestRoute?.networkMs) ? `${bestRoute.networkMs.toFixed(1)} ms` : networkMs != null && orderedRoutes.length === 0 ? `${networkMs.toFixed(1)} ms` : "-"} />
            <Stat label={t("canvas.gatewayCompute")} value={validMs(bestRoute?.computeMs) ? `${bestRoute.computeMs.toFixed(1)} ms` : computeMs != null && orderedRoutes.length === 0 ? `${computeMs.toFixed(1)} ms` : "-"} />
          </div>
          {displayedTotalMs != null && (
            <p className="flex flex-wrap gap-x-3 gap-y-0.5 font-mono text-[11px] tabular-nums text-muted">
              <span data-testid="pipeline-total-detail" className="text-fg">{t("canvas.pipelineTotal", { ms: displayedTotalMs.toFixed(1) })}</span>
              {throughput != null && <span data-testid="pipeline-throughput">{t("canvas.pipelineThroughput", { tps: throughput.toFixed(1) })}</span>}
              {breakdown && <span data-testid="pipeline-route-breakdown">{breakdown}</span>}
            </p>
          )}
          {orderedRoutes.length === 0 && cost && cost.legsMs.length > 0 && (
            <section>
              <h3 className="text-[10px] font-semibold uppercase tracking-[0.12em] text-faint">{t("canvas.gatewayCycleLegs")}</h3>
              <ol className="mt-1.5 grid gap-1 sm:grid-cols-2">
                {cost.legsMs.map((leg, index) => (
                  <li key={index} className="flex justify-between gap-2 rounded bg-raised/50 px-2 py-1 font-mono text-[11px] tabular-nums">
                    <span>{t("canvas.gatewayLeg", { index: index + 1 })}</span>
                    <span>{validMs(leg) ? `${leg.toFixed(1)} ms` : "-"}</span>
                  </li>
                ))}
              </ol>
            </section>
          )}
          <section>
            <h3 className="text-[10px] font-semibold uppercase tracking-[0.12em] text-faint">
              {t("canvas.gatewayRoutesTitle")}{orderedRoutes.length > 0 && ` · ${orderedRoutes.length}`}
            </h3>
            {orderedRoutes.length === 0 ? (
              <p className="mt-1.5 text-xs text-muted">{t("canvas.gatewayRoutesEmpty")}</p>
            ) : (
              <ol data-testid="pipeline-routes" className="mt-1.5 space-y-2">
                {orderedRoutes.map(({ route, totalMs: routeMs }, index) => (
                  <RouteDetail key={route.nodeIds.join("\u0000")} route={route} totalMs={routeMs} best={index === 0 && routeMs != null} nameOf={label} />
                ))}
              </ol>
            )}
          </section>
          {/* O texto fala da rota mais rápida; uma réplica por relé fora de uso não o justifica. */}
          {bestRoute && (bestRoute.relayed || bestRoute.relayBypassedByRing) && (
            <p className="rounded-lg border border-relay/40 bg-relay/5 p-3 text-xs text-muted">{t("canvas.relayRouteHint")}</p>
          )}
        </div>
      </Modal>
    </>
  );
}
