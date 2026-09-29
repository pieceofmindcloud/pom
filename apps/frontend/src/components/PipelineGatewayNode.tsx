import { ServerCog } from "lucide-react";
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
};

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

export function pipelineRouteTotalMs(route: PipelineRouteCost): number | null {
  const networkMs = route.legsMs.every(validMs)
    ? route.legsMs.reduce((sum, leg) => sum + leg, 0)
    : null;
  const computeMs = validMs(route.computeMs) ? route.computeMs : null;
  if (computeMs != null && networkMs != null) return networkMs + computeMs;
  if (validMs(route.rttMs)) return route.rttMs;
  return networkMs;
}

export function PipelineGatewayNode({
  position,
  cost,
  routes = [],
}: {
  position: CanvasPosition;
  cost?: PipelineCycleCost;
  routes?: PipelineRouteCost[];
}) {
  const { t } = useI18n();
  const totalMs = pipelineCycleTotalMs(cost);
  const orderedRoutes = routes
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
  const routeTotalMs = orderedRoutes[0]?.totalMs ?? null;
  const displayedTotalMs = orderedRoutes.length > 0 ? routeTotalMs : totalMs;
  const networkMs = cost?.legsMs.every((leg): leg is number => leg != null && Number.isFinite(leg) && leg >= 0)
    ? cost.legsMs.reduce((sum, leg) => sum + leg, 0)
    : null;
  const computeMs = cost?.computeMs != null && Number.isFinite(cost.computeMs) && cost.computeMs >= 0
    ? cost.computeMs
    : null;

  return (
    <article
      data-testid="pipeline-gateway"
      className="absolute rounded-xl border border-aqua/35 bg-ink/95 p-3.5 shadow-panel backdrop-blur"
      style={{ left: position.left, top: position.top, width: GATEWAY_WIDTH, minHeight: GATEWAY_HEIGHT }}
    >
      <div className="absolute inset-x-3.5 top-0 h-px bg-aqua/70" />
      <div className="flex items-start gap-2">
        <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-aqua/25 bg-aqua/10 text-aqua">
          <ServerCog className="h-4 w-4" />
        </span>
        <div className="min-w-0">
          <h3 className="truncate text-sm font-semibold text-fg">{t("canvas.gateway")}</h3>
          <p className="truncate text-[10px] text-faint">{t("canvas.gatewayReturn")}</p>
        </div>
      </div>
      <div className="mt-3 rounded-lg border border-aqua/25 bg-aqua/10 px-2.5 py-2">
        <div className="text-[9px] uppercase tracking-[0.12em] text-faint">{t("canvas.pipelineTotalLabel")}</div>
        <div className="mt-0.5 font-mono text-sm font-semibold tabular-nums text-aqua">
          {displayedTotalMs == null ? t("canvas.pipelineTotalUnknown") : t("canvas.pipelineTotal", { ms: displayedTotalMs.toFixed(1) })}
        </div>
        {/* Com o compute medido, o total é o tempo de um token pela rota. */}
        {displayedTotalMs != null && displayedTotalMs > 0 && orderedRoutes[0]?.route.computeMs != null && (
          <div data-testid="pipeline-throughput" className="mt-0.5 font-mono text-[10px] tabular-nums text-muted">
            {t("canvas.pipelineThroughput", { tps: (1000 / displayedTotalMs).toFixed(1) })}
          </div>
        )}
      </div>
      {networkMs != null && computeMs != null && (
        <p className="mt-2 font-mono text-[10px] tabular-nums text-muted">
          {t("canvas.pipelineCostBreakdown", { network: networkMs.toFixed(1), compute: computeMs.toFixed(1) })}
        </p>
      )}
      {orderedRoutes.length > 0 && (
        <div data-testid="pipeline-routes" className="mt-2 border-t border-aqua/15 pt-2">
          <div className="text-[9px] uppercase tracking-[0.12em] text-faint">
            {t("canvas.pipelineRouteCount", { count: orderedRoutes.length })}
          </div>
          <ol className="mt-1.5 space-y-1">
            {orderedRoutes.map(({ route, totalMs: routeMs }) => {
              const path = route.nodeIds.join(" → ");
              return (
                <li key={path} className="flex min-w-0 items-center justify-between gap-2 font-mono text-[10px] tabular-nums text-muted" title={path}>
                  <span className="min-w-0 truncate">{path}</span>
                  <span className="shrink-0 text-aqua">{routeMs == null ? t("canvas.pipelineTotalUnknown") : `${routeMs.toFixed(1)} ms`}</span>
                </li>
              );
            })}
          </ol>
        </div>
      )}
    </article>
  );
}
