import clsx from "clsx";
import { ArrowRight, LayoutGrid, Minus, Plus, RotateCcw, ServerCog, Sparkles, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type PointerEvent, type ReactNode } from "react";
import { layerRange, megabytes } from "../lib/format";
import {
  CARD_HEIGHT,
  CARD_WIDTH,
  clearPositions,
  deployLayout,
  deploymentLayoutKey,
  fallbackPosition,
  gatewayRouteGeometry,
  pipelineGatewayPosition,
  readPositions,
  routeGeometry,
  writePositions,
  type CanvasPosition,
} from "../lib/networkLayout";
import type { DeployMachine, DeployPlan, DeploySelection, InferenceRole } from "../lib/types";
import { edgeStyle, type EdgeState } from "./ModelNetworkCanvas";
import { PipelineGatewayNode, type PipelineCycleCost } from "./PipelineGatewayNode";
import { RouteRttLabel } from "./RouteRttLabel";
import { AlertTooltip, Badge, Button, Dot, Progress } from "./ui";
import { useI18n } from "../i18n/provider";

type DeployNetworkCanvasProps = {
  plan: DeployPlan;
  rows: DeploySelection[];
  inspectedNodeId: string | null;
  onInspect: (nodeId: string) => void;
  onAddPeer: (nodeId: string) => void;
  onRemovePeer: (nodeId: string) => void;
  onDropPeer: (nodeId: string) => void;
  labelOf: (nodeId: string) => string;
  mapNodeIds?: string[];
  primaryCandidateIds?: string[];
  primaryNodeId?: string;
  onTogglePrimary?: (nodeId: string) => void;
  pipelineCost?: PipelineCycleCost;
  /** Extra content at the foot of a machine's card (the model it lacks, its download). */
  cardFooter?: (nodeId: string) => ReactNode;
};

function machineRows(rows: DeploySelection[], nodeId: string) {
  return rows.filter((row) => row.target_node_id === nodeId).sort((a, b) => a.start_layer - b.start_layer);
}

function edgeId(from: string, to: string) {
  return `${from}:${to}`;
}

type Stage = {
  start_layer: number;
  end_layer: number;
  nodeIds: string[];
  /** The prefill machines of a prefill/decode split: a stage of their own, before the machines that decode. */
  prefill: boolean;
};

/** A machine's part in a prefill/decode split: its row's, else its step's. */
function roleOf(rows: DeploySelection[], plan: DeployPlan, nodeId: string): InferenceRole | null {
  return machineRows(rows, nodeId)[0]?.inference_role
    ?? plan.steps.find((step) => step.target_node_id === nodeId)?.inference_role
    ?? null;
}

function stagesOf(plan: DeployPlan, rows: DeploySelection[] = []): Stage[] {
  const stages: Stage[] = [];
  const isPrefill = (nodeId: string) => roleOf(rows, plan, nodeId) === "prefill";
  // replicas of one range keep their order, with its prefill machines first: they are the head
  const steps = [...plan.steps].sort((a, b) =>
    a.start_layer - b.start_layer ||
    a.end_layer - b.end_layer ||
    Number(isPrefill(b.target_node_id)) - Number(isPrefill(a.target_node_id)));
  for (const step of steps) {
    const previous = stages[stages.length - 1];
    const prefill = isPrefill(step.target_node_id);
    if (
      previous &&
      previous.start_layer === step.start_layer &&
      previous.end_layer === step.end_layer &&
      previous.prefill === prefill
    ) {
      if (step.target_node_id && !previous.nodeIds.includes(step.target_node_id)) {
        previous.nodeIds.push(step.target_node_id);
      }
    } else {
      stages.push({
        start_layer: step.start_layer,
        end_layer: step.end_layer,
        nodeIds: step.target_node_id ? [step.target_node_id] : [],
        prefill,
      });
    }
  }
  return stages;
}

function machineBounds(rows: DeploySelection[], machine: DeployMachine) {
  const taking = machineRows(rows, machine.node_id);
  const ranges = taking.length > 0 ? taking : machine.holds.map(([start_layer, end_layer]) => ({ start_layer, end_layer }));
  return {
    start_layer: Math.min(...ranges.map((range) => range.start_layer), 0),
    end_layer: Math.max(...ranges.map((range) => range.end_layer), 0),
  };
}

function CapacityBar({
  label,
  value,
  detail,
  tone,
}: {
  label: string;
  value: number | null;
  detail: string;
  tone: "aqua" | "amber" | "iris";
}) {
  return (
    <div className="min-w-0">
      <div className="mb-1 flex items-baseline justify-between gap-1 text-[9px]">
        <span className="truncate uppercase tracking-[0.1em] text-faint">{label}</span>
        <span className="shrink-0 font-mono tabular-nums text-muted">{detail}</span>
      </div>
      {value == null ? (
        <div className="h-1.5 w-full rounded-full bg-line" aria-hidden="true" />
      ) : (
        <Progress value={value} tone={tone} />
      )}
    </div>
  );
}

/**
 * A GPU after the deployment: the bar is the **projected** usage
 * (`vram_projected_mb / vram_total_mb`), the tick is the usage as of now
 * (`vram_used_mb`), and the bar turns rose when the projection exceeds the card.
 * It is the preview the operator adjusts by changing the context and the sessions.
 */
function GpuProjectionBar({
  index,
  usedMb,
  projectedMb,
  totalMb,
}: {
  index: number;
  usedMb: number;
  projectedMb: number;
  totalMb: number;
}) {
  const over = projectedMb > totalMb;
  const projectedPct = totalMb > 0 ? Math.min(100, (projectedMb / totalMb) * 100) : 0;
  const usedPct = totalMb > 0 ? Math.min(100, (usedMb / totalMb) * 100) : 0;
  return (
    <div className="min-w-0">
      <div className="mb-1 flex items-baseline justify-between gap-1 text-[9px]">
        <span className="truncate uppercase tracking-[0.1em] text-faint">GPU {index}</span>
        <span
          className={clsx(
            "shrink-0 font-mono tabular-nums",
            over ? "text-rose" : "text-muted",
          )}
        >
          {megabytes(projectedMb)} / {megabytes(totalMb)}
        </span>
      </div>
      <div className="relative h-1.5 w-full overflow-hidden rounded-full bg-line">
        <div
          className={clsx("h-full rounded-full", over ? "bg-rose" : "bg-aqua")}
          style={{ width: `${Math.max(2, projectedPct)}%` }}
        />
        <span
          aria-hidden="true"
          className="absolute inset-y-0 w-px bg-fg/70"
          style={{ left: `${usedPct}%` }}
        />
      </div>
    </div>
  );
}

function NodeCard({
  machine,
  rows,
  position,
  selected,
  dragging,
  onInspect,
  onAddPeer,
  onRemovePeer,
  onPointerDown,
  onPointerMove,
  onPointerUp,
  primaryNodeId,
  onTogglePrimary,
  footer,
}: {
  machine: DeployMachine;
  rows: DeploySelection[];
  position: CanvasPosition;
  selected: boolean;
  dragging: boolean;
  onInspect: () => void;
  onAddPeer: () => void;
  onRemovePeer: () => void;
  onPointerDown: (event: PointerEvent<HTMLElement>) => void;
  onPointerMove: (event: PointerEvent<HTMLElement>) => void;
  onPointerUp: (event: PointerEvent<HTMLElement>) => void;
  primaryNodeId?: string;
  onTogglePrimary?: () => void;
  footer?: ReactNode;
}) {
  const taking = machineRows(rows, machine.node_id);
  const active = taking.length > 0;
  const { t } = useI18n();
  return (
    <article
      role="button"
      tabIndex={0}
      onClick={onInspect}
      onDoubleClick={(event) => {
        event.stopPropagation();
        onAddPeer();
      }}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") onInspect();
      }}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      className={clsx(
        "absolute select-none rounded-xl border bg-ink/95 p-3.5 shadow-panel backdrop-blur transition-[box-shadow,border-color]",
        dragging ? "cursor-grabbing border-iris shadow-lift" : "cursor-grab border-line-strong hover:border-iris/55 hover:shadow-lift",
        selected && "border-aqua ring-2 ring-aqua/30 shadow-[0_0_24px_rgba(65,224,206,0.16)]",
        !active && "opacity-75",
      )}
      style={{ left: position.left, top: position.top, width: CARD_WIDTH, minHeight: CARD_HEIGHT, touchAction: "none" }}
    >
      <div className="absolute inset-x-3.5 top-0 h-px bg-iris/70" />
      <div className="flex items-start justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2">
          <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-line bg-raised text-faint">
            <ServerCog className="h-4 w-4" />
          </span>
          <div className="min-w-0">
            <h3 className="truncate text-sm font-semibold text-fg">{machine.label}</h3>
            <p className="flex min-w-0 items-center gap-1 truncate text-[10px] text-faint">
              <span className="truncate">{active ? t("deployStudio.inDeployment") : t("deployStudio.availableForPlan")}</span>
              {machine.rtt_ms != null && <><span aria-hidden="true">·</span><span className="shrink-0">RTT {machine.rtt_ms.toFixed(1)} ms</span></>}
            </p>
          </div>
        </div>
        <div className="flex items-center gap-1">
          <Dot tone={active ? "aqua" : "neutral"} pulse={active} />
          {onTogglePrimary && (
            <button
              type="button"
              aria-label={t(primaryNodeId === machine.node_id ? "deployStudio.unmarkPrimaryAria" : "deployStudio.markPrimaryAria", { machine: machine.label })}
              aria-pressed={primaryNodeId === machine.node_id}
              title={primaryNodeId === machine.node_id ? t("deployStudio.unmarkPrimary") : t("deployStudio.markPrimary")}
              onClick={(event) => {
                event.stopPropagation();
                onTogglePrimary();
              }}
              onPointerDown={(event) => event.stopPropagation()}
              onPointerUp={(event) => event.stopPropagation()}
              className={clsx(
                "flex h-5 items-center gap-1 rounded-md px-1 text-[9px] font-semibold uppercase tracking-[0.08em]",
                primaryNodeId === machine.node_id
                  ? "bg-aqua/12 text-aqua"
                  : "text-faint hover:bg-aqua/10 hover:text-aqua",
              )}
            >
              <Sparkles className="h-3 w-3" />
              <span className="sr-only">primary</span>
            </button>
          )}
          <button
            type="button"
            aria-label={t("deployStudio.removeMachineAria", { machine: machine.label })}
            title={t("deployStudio.removeFromDeployment")}
            onClick={(event) => {
              event.stopPropagation();
              onRemovePeer();
            }}
            onPointerDown={(event) => event.stopPropagation()}
            onPointerUp={(event) => event.stopPropagation()}
            className="flex h-5 w-5 items-center justify-center rounded-md text-faint hover:bg-rose/10 hover:text-rose"
          >
            <X className="h-3.5 w-3.5" />
          </button>
        </div>
      </div>
      <div className="mt-3 flex min-h-7 flex-wrap items-center gap-1 rounded-lg border border-line bg-raised/90 px-2 py-1.5">
        {active ? taking.map((row) => (
          <Badge key={`${row.start_layer}-${row.end_layer}`} tone="iris" mono className="px-1.5 py-0 text-[0.62rem]">
            {layerRange(row.start_layer, row.end_layer)}
          </Badge>
        )) : <span className="font-mono text-[10px] text-faint">{t("deployStudio.dropToInclude")}</span>}
      </div>
      <div className="mt-3 space-y-2">
        {machine.capacity && machine.capacity.gpus.length > 0 ? (
          <div className="space-y-2">
            <div className="flex items-center justify-between gap-1">
              <span
                className={clsx(
                  "text-[9px] uppercase tracking-[0.1em]",
                  !machine.capacity.fits
                    ? "text-rose"
                    : machine.capacity.degraded
                      ? "text-amber"
                      : "text-faint",
                )}
              >
                {t("deployStudio.gpuProjection")}
                {machine.capacity.fits && machine.capacity.degraded && ` · ${t("deployStudio.trimmed")}`}
              </span>
              {(!machine.capacity.fits || machine.capacity.degraded) && (
                <AlertTooltip
                  label={t("deployStudio.vramProjectionOf", { machine: machine.label })}
                  message={
                    (!machine.capacity.fits
                      ? t(machine.capacity.gpus.length > 1 ? "deployStudio.projectionExceedsMany" : "deployStudio.projectionExceedsOne", { machine: machine.label }) + " "
                      : t("deployStudio.fitsButTrimmed", { machine: machine.label }) + " ") +
                    (machine.capacity.note ? machine.capacity.note + ". " : "") +
                    machine.capacity.gpus
                      .map(
                        (g) =>
                          t("deployStudio.gpuProjectedOf", { index: g.index, projected: megabytes(g.vram_projected_mb), total: megabytes(g.vram_total_mb) }),
                      )
                      .join("; ")
                  }
                />
              )}
            </div>
            {machine.capacity.gpus.map((g) => (
              <GpuProjectionBar
                key={g.index}
                index={g.index}
                usedMb={g.vram_used_mb}
                projectedMb={g.vram_projected_mb}
                totalMb={g.vram_total_mb}
              />
            ))}
            {machine.capacity.note && machine.capacity.fits && (
              <p
                className={clsx(
                  "text-[9px] leading-snug",
                  machine.capacity.degraded ? "text-amber" : "text-faint",
                )}
              >
                {machine.capacity.note}
              </p>
            )}
          </div>
        ) : (
          <CapacityBar
            label="GPU"
            value={machine.gpu_used_pct ?? null}
            detail={
              machine.gpu_used_pct == null
                ? t("deployStudio.noGpu")
                : active
                  ? t("deployStudio.nowProjection", { pct: machine.gpu_used_pct.toFixed(0) })
                  : `${machine.gpu_used_pct.toFixed(0)}% · ${megabytes(machine.gpu_free_mb)}`
            }
            tone={machine.gpu_used_pct != null && machine.gpu_used_pct >= 88 ? "amber" : "aqua"}
          />
        )}
        <CapacityBar
          label="RAM"
          value={machine.ram_used_pct ?? null}
          detail={machine.ram_used_pct == null ? "-" : `${machine.ram_used_pct.toFixed(0)}% · ${megabytes(machine.ram_free_mb)}`}
          tone={machine.ram_used_pct != null && machine.ram_used_pct >= 88 ? "amber" : "iris"}
        />
        <CapacityBar
          label={t("canvas.load")}
          value={machine.load == null ? null : Math.min(100, (machine.load / 8) * 100)}
          detail={machine.load == null ? t("dashboard.inFlightUnavailable") : t("canvas.inFlight", { count: machine.load })}
          tone={machine.load != null && machine.load >= 7 ? "amber" : "aqua"}
        />
      </div>
      {footer}
    </article>
  );
}

export function DeployNetworkCanvas({
  plan,
  rows,
  inspectedNodeId,
  onInspect,
  onAddPeer,
  onRemovePeer,
  onDropPeer,
  labelOf,
  mapNodeIds,
  primaryCandidateIds,
  primaryNodeId,
  onTogglePrimary,
  pipelineCost,
  cardFooter,
}: DeployNetworkCanvasProps) {
  const { t } = useI18n();
  const canvas = useRef<HTMLDivElement>(null);
  const dragOffset = useRef({ x: 0, y: 0 });
  const [zoom, setZoom] = useState(1);
  const [positions, setPositions] = useState<Record<string, CanvasPosition>>({});
  const [dragging, setDragging] = useState<string | null>(null);

  const visibleMapNodeIds = useMemo(
    () => mapNodeIds ?? plan.machines.map((machine) => machine.node_id),
    [mapNodeIds, plan.machines],
  );
  const machines = useMemo(() => {
    return visibleMapNodeIds
      .map((nodeId) => plan.machines.find((machine) => machine.node_id === nodeId))
      .filter((machine): machine is DeployMachine => Boolean(machine));
  }, [plan.machines, visibleMapNodeIds]);
  const nodeIds = useMemo(() => machines.map((machine) => machine.node_id), [machines]);
  const layoutKey = useMemo(
    // the role is part of the topology: a machine that becomes the prefill head moves to its own column
    () => deploymentLayoutKey(plan.deployment_name || plan.group, machines.map((machine) => {
      const role = roleOf(rows, plan, machine.node_id);
      return { node_id: role && role !== "both" ? `${machine.node_id}:${role}` : machine.node_id, ...machineBounds(rows, machine) };
    })),
    [machines, plan, rows],
  );
  const arrangement = useMemo(() => {
    const assigned = machines.filter((machine) => machineRows(rows, machine.node_id).length > 0);
    return deployLayout(
      assigned.map((machine) => {
        const role = roleOf(rows, plan, machine.node_id);
        return { id: machine.node_id, ...machineBounds(rows, machine), role: role === "both" ? null : role };
      }),
      machines.filter((machine) => !assigned.includes(machine)).map((machine) => machine.node_id),
    );
  }, [machines, plan, rows]);
  // `positions` holds only the cards the operator dragged, for this topology
  // (`layoutKey`: the machines and their ranges). Every other card follows the
  // arrangement, which is recomputed when a machine or a range changes - the
  // old merge kept the spot a card had before it got a range, so the map stayed
  // tangled until Reset.
  const positionOf = (nodeId: string, index: number) => positions[nodeId] ?? arrangement[nodeId] ?? fallbackPosition(index);

  // A string key: `nodeIds` is a new array on every render of the screen.
  const nodeKey = nodeIds.join("|");
  useEffect(() => {
    const present = new Set(nodeKey.split("|"));
    const dragged = readPositions(layoutKey);
    setPositions(Object.fromEntries(Object.entries(dragged).filter(([nodeId]) => present.has(nodeId))));
  }, [layoutKey, nodeKey]);

  const updatePosition = (nodeId: string, next: CanvasPosition) => setPositions((current) => {
    const updated = { ...current, [nodeId]: next };
    writePositions(layoutKey, updated);
    return updated;
  });
  // Organize puts every card back on the arrangement, dragged ones included.
  const organize = () => {
    clearPositions(layoutKey);
    setPositions({});
    setZoom(1);
  };
  const reset = () => { clearPositions(layoutKey); setPositions({}); setZoom(1); };
  const fit = () => setZoom(1);

  function beginDrag(nodeId: string, index: number, event: PointerEvent<HTMLElement>) {
    if (!canvas.current) return;
    const bounds = canvas.current.getBoundingClientRect();
    const current = positionOf(nodeId, index);
    dragOffset.current = { x: (event.clientX - bounds.left) / zoom - current.left, y: (event.clientY - bounds.top) / zoom - current.top };
    setDragging(nodeId);
    event.currentTarget.setPointerCapture(event.pointerId);
  }
  function drag(nodeId: string, event: PointerEvent<HTMLElement>) {
    if (dragging !== nodeId || !canvas.current) return;
    const bounds = canvas.current.getBoundingClientRect();
    const maxLeft = Math.max(16, bounds.width / zoom - CARD_WIDTH - 16);
    const maxTop = Math.max(58, bounds.height / zoom - CARD_HEIGHT - 16);
    updatePosition(nodeId, {
      left: Math.max(16, Math.min(maxLeft, (event.clientX - bounds.left) / zoom - dragOffset.current.x)),
      top: Math.max(58, Math.min(maxTop, (event.clientY - bounds.top) / zoom - dragOffset.current.y)),
    });
  }

  const stages = stagesOf(plan, rows);
  const bridgeIds = new Set<string>();
  for (let index = 1; index < stages.length; index += 1) {
    for (const from of stages[index - 1].nodeIds) {
      for (const to of stages[index].nodeIds) bridgeIds.add(edgeId(from, to));
    }
  }
  // The backend already limits the options to the next stage. This guard in the canvas
  // keeps the visual rule even while an old peer still answers with the
  // previous broad list: no jump over a layer appears on the map.
  const visibleRouteOptions = (plan.route_options ?? []).filter(
    (route) => bridgeIds.has(edgeId(route.from_node_id, route.to_node_id)),
  );
  const selectedEdges = new Set(visibleRouteOptions.filter((route) => route.selected).map((route) => edgeId(route.from_node_id, route.to_node_id)));
  const candidateEdges = new Set(visibleRouteOptions.filter((route) => !route.selected).map((route) => edgeId(route.from_node_id, route.to_node_id)));
  const edges = visibleRouteOptions.filter((route) => nodeIds.includes(route.from_node_id) && nodeIds.includes(route.to_node_id));
  const principalEdges = stages.slice(1).flatMap((stage, stageIndex) => {
    const previous = stages[stageIndex];
    return previous.nodeIds.flatMap((from) => stage.nodeIds.map((to) => ({ from, to })));
  }).filter((edge) => edge.from !== edge.to && !edges.some((route) => route.from_node_id === edge.from && route.to_node_id === edge.to));
  const allEdges = [
    ...edges,
    ...principalEdges.map((edge) => ({
      from_node_id: edge.from,
      to_node_id: edge.to,
      // Without an explicit option there is no directional measurement for this edge.
      // Never copy the destination's RTT, which would be the measurement from another origin.
      rtt_ms: null,
      selected: true,
      note: "pipeline",
    })),
  ];
  const terminalNodeIds = stages.at(-1)?.nodeIds.filter((nodeId) => nodeIds.includes(nodeId)) ?? [];
  const gatewayPosition = pipelineGatewayPosition(
    terminalNodeIds.map((nodeId) => positionOf(nodeId, nodeIds.indexOf(nodeId))),
  );

  return (
    <section
      ref={canvas}
      className="relative h-full min-h-0 w-full overflow-auto bg-[radial-gradient(circle_at_1px_1px,rgba(91,78,255,.16)_1px,transparent_0)]"
      style={{ backgroundSize: "18px 18px" }}
      onDragOver={(event) => event.preventDefault()}
      onDrop={(event) => {
        event.preventDefault();
        const nodeId = event.dataTransfer.getData("application/x-pom-node");
        if (nodeId) onDropPeer(nodeId);
      }}
    >
      <div className="absolute left-4 top-3 z-20 flex max-w-[calc(100%-2rem)] flex-wrap items-center gap-2 border border-line bg-ink/90 p-1.5 shadow-panel backdrop-blur-md">
        <div className="flex items-center gap-2 px-2">
          <span className="eyebrow">{t("deployStudio.mapTitle")}</span>
          <span className="font-mono text-[10px] text-muted">{t("deployStudio.mapSummary", { nodes: machines.length, layers: plan.total_layers ?? "-" })}</span>
        </div>
        <span className="h-5 w-px bg-line" />
        <Button size="sm" aria-label={t("canvas.zoomOut")} title={t("canvas.zoomOut")} onClick={() => setZoom((value) => Math.max(0.65, value - 0.1))}><Minus className="h-4 w-4" /></Button>
        <Button size="sm" aria-label={t("canvas.zoomIn")} title={t("canvas.zoomIn")} onClick={() => setZoom((value) => Math.min(1.25, value + 0.1))}><Plus className="h-4 w-4" /></Button>
        <Button size="sm" aria-label={t("deployStudio.fitCanvas")} title={t("deployStudio.fitCanvas")} onClick={fit}>Fit</Button>
        <Button size="sm" aria-label={t("deployStudio.organizeMachines")} title={t("deployStudio.organizeMachines")} onClick={organize}><LayoutGrid className="h-4 w-4" /></Button>
        <Button size="sm" aria-label={t("deployStudio.resetLayout")} title={t("deployStudio.resetLayout")} onClick={reset}><RotateCcw className="h-4 w-4" /></Button>
      </div>

      {machines.length === 0 ? (
        <div className="flex h-full items-center justify-center text-sm text-muted">{t("deployStudio.dropMachine")}</div>
      ) : (
        <div className="absolute inset-0 origin-top-left" style={{ transform: `scale(${zoom})`, width: `${100 / zoom}%`, height: `${100 / zoom}%` }}>
          <svg aria-hidden="true" className="pointer-events-none absolute inset-0 h-full w-full overflow-visible">
            <defs><marker id="deploy-route-arrow" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><path d="M0,0 L8,4 L0,8 Z" className="fill-aqua" /></marker></defs>
            {allEdges.map((route, index) => {
              const fromIndex = nodeIds.indexOf(route.from_node_id);
              const toIndex = nodeIds.indexOf(route.to_node_id);
              if (fromIndex < 0 || toIndex < 0) return null;
              const from = positionOf(route.from_node_id, fromIndex);
              const to = positionOf(route.to_node_id, toIndex);
              const geometry = routeGeometry(from, to);
              const id = edgeId(route.from_node_id, route.to_node_id);
              const state: EdgeState = selectedEdges.has(id) || (!selectedEdges.has(id) && route.selected) ? "principal" : candidateEdges.has(id) ? "paralela" : "inativa";
              const style = edgeStyle(state);
              return <path key={`${id}-${index}`} d={`M ${geometry.x1} ${geometry.y1} C ${geometry.c1x} ${geometry.c1y}, ${geometry.c2x} ${geometry.c2y}, ${geometry.x2} ${geometry.y2}`} className={style.className} strokeWidth={style.strokeWidth} strokeDasharray={style.strokeDasharray} markerEnd={style.markerEnd?.replace("route-arrow", "deploy-route-arrow")} />;
            })}
            {gatewayPosition && terminalNodeIds.map((nodeId) => {
              const geometry = gatewayRouteGeometry(positionOf(nodeId, nodeIds.indexOf(nodeId)), gatewayPosition);
              return <path key={`gateway-${nodeId}`} d={`M ${geometry.x1} ${geometry.y1} C ${geometry.c1x} ${geometry.c1y}, ${geometry.c2x} ${geometry.c2y}, ${geometry.x2} ${geometry.y2}`} className="fill-none stroke-aqua" strokeWidth="3" markerEnd="url(#deploy-route-arrow)" />;
            })}
          </svg>
          {allEdges.map((route, index) => {
            const fromIndex = nodeIds.indexOf(route.from_node_id);
            const toIndex = nodeIds.indexOf(route.to_node_id);
            if (fromIndex < 0 || toIndex < 0) return null;
            const from = positionOf(route.from_node_id, fromIndex);
            const to = positionOf(route.to_node_id, toIndex);
            const geometry = routeGeometry(from, to);
            return <RouteRttLabel key={`rtt-${route.from_node_id}-${route.to_node_id}-${index}`} rttMs={route.rtt_ms} left={geometry.labelX} top={geometry.labelY} />;
          })}
          {gatewayPosition && terminalNodeIds.map((nodeId) => {
            const geometry = gatewayRouteGeometry(positionOf(nodeId, nodeIds.indexOf(nodeId)), gatewayPosition);
            const route = plan.route_options?.find((option) => option.from_node_id === nodeId && option.to_node_id === "gateway");
            return <RouteRttLabel key={`rtt-gateway-${nodeId}`} rttMs={route?.rtt_ms} left={geometry.labelX} top={geometry.labelY} />;
          })}
          {machines.map((machine, index) => (
            <NodeCard key={machine.node_id} footer={cardFooter?.(machine.node_id)} machine={machine} rows={rows} position={positionOf(machine.node_id, index)} selected={inspectedNodeId === machine.node_id} dragging={dragging === machine.node_id} primaryNodeId={primaryNodeId} onTogglePrimary={onTogglePrimary && (primaryCandidateIds ?? []).includes(machine.node_id) ? () => onTogglePrimary(machine.node_id) : undefined} onInspect={() => onInspect(machine.node_id)} onAddPeer={() => onAddPeer(machine.node_id)} onRemovePeer={() => onRemovePeer(machine.node_id)} onPointerDown={(event) => beginDrag(machine.node_id, index, event)} onPointerMove={(event) => drag(machine.node_id, event)} onPointerUp={(event) => { if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId); setDragging(null); }} />
          ))}
          {gatewayPosition && <PipelineGatewayNode position={gatewayPosition} cost={pipelineCost} />}
          <div className="absolute bottom-3 left-4 flex items-center gap-2 border border-line bg-ink/90 px-3 py-2 text-[10px] text-muted shadow-panel backdrop-blur">
            <Sparkles className="h-3 w-3 text-aqua" /><span>prefill</span>
            {nodeIds.map((nodeId, index) => <span key={nodeId} className="flex items-center gap-1"><ArrowRight className="h-3 w-3 text-faint" /><span className={index === 0 ? "text-iris" : "text-aqua"}>{labelOf(nodeId)}</span></span>)}
            <ArrowRight className="h-3 w-3 text-faint" /><span>resposta</span>
          </div>
        </div>
      )}
      {visibleRouteOptions.length > 0 && <div className="absolute bottom-3 right-4 z-20 flex max-w-[48%] gap-1 overflow-x-auto">{visibleRouteOptions.slice(0, 6).map((route) => <span key={`${route.from_node_id}-${route.to_node_id}`} className={clsx("shrink-0 border px-1.5 py-0.5 font-mono text-[10px]", route.selected ? "border-aqua/30 bg-aqua/10 text-aqua" : "border-line text-faint")}>{labelOf(route.from_node_id)} → {labelOf(route.to_node_id)} · {route.rtt_ms == null ? "RTT -" : `${route.rtt_ms.toFixed(1)} ms`}</span>)}</div>}
    </section>
  );
}
