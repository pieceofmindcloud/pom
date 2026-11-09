import clsx from "clsx";
import { ArrowRight, LayoutGrid, Minus, Plus, RotateCcw, ServerCog, Sparkles, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type PointerEvent, type ReactNode } from "react";
import { layerRange, megabytes } from "../lib/format";
import {
  CARD_WIDTH,
  DEPLOY_CARD_SIZE,
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
import { AlertTooltip, Badge, Button, Progress } from "./ui";
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
  /** The machines marked for Split the layers; Shift+click on a card toggles one. */
  markedNodeIds?: string[];
  onToggleMarked?: (nodeId: string) => void;
};

const MIN_ZOOM = 0.4;
const MAX_ZOOM = 1.6;
const clampZoom = (zoom: number) => Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, zoom));

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
  // the machine's own first and last layer (0 only when it has no range at all):
  // `Math.min(..., 0)` put every machine at layer 0, so the arrangement stacked
  // the whole pipeline in the heads' column
  if (ranges.length === 0) return { start_layer: 0, end_layer: 0 };
  return {
    start_layer: Math.min(...ranges.map((range) => range.start_layer)),
    end_layer: Math.max(...ranges.map((range) => range.end_layer)),
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
  marked,
  onToggleMarked,
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
  marked?: boolean;
  onToggleMarked?: () => void;
}) {
  const taking = machineRows(rows, machine.node_id);
  const active = taking.length > 0;
  const { t } = useI18n();
  return (
    <article
      role="button"
      tabIndex={0}
      onClick={(event) => {
        if (event.shiftKey && onToggleMarked) onToggleMarked();
        else onInspect();
      }}
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
        marked && !selected && "border-iris/70",
        !active && "opacity-75",
      )}
      style={{ left: position.left, top: position.top, width: CARD_WIDTH, minHeight: DEPLOY_CARD_SIZE.height, touchAction: "none" }}
    >
      <div className="absolute inset-x-3.5 top-0 h-px bg-iris/70" />
      <div className="flex items-start justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2">
          <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-line bg-raised text-faint">
            <ServerCog className="h-4 w-4" />
          </span>
          <div className="min-w-0">
            <h3 className="truncate text-sm font-semibold text-fg">
              {machine.label}
              {marked && <span className="sr-only"> ({t("deployStudio.marked")})</span>}
            </h3>
            {!active && <p className="truncate text-[10px] text-faint">{t("deployStudio.dropToInclude")}</p>}
          </div>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          {taking.map((row) => (
            <Badge key={`${row.start_layer}-${row.end_layer}`} tone="iris" mono className="px-1.5 py-0 text-[0.62rem]">
              {layerRange(row.start_layer, row.end_layer)}
            </Badge>
          ))}
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
  markedNodeIds,
  onToggleMarked,
}: DeployNetworkCanvasProps) {
  const { t } = useI18n();
  const canvas = useRef<HTMLDivElement>(null);
  const dragOffset = useRef({ x: 0, y: 0 });
  const [zoom, setZoom] = useState(1);
  // the zoom the wheel handler reads between renders
  const zoomRef = useRef(1);
  zoomRef.current = zoom;
  // dragging the dotted background scrolls the map, like its scrollbars
  const pan = useRef<{ x: number; y: number; left: number; top: number } | null>(null);
  const [panning, setPanning] = useState(false);

  // The wheel and the trackpad pinch (a wheel event with ctrlKey) zoom around
  // the pointer. A native listener, not React's: React's wheel handler is
  // passive and cannot keep the page from scrolling.
  useEffect(() => {
    const element = canvas.current;
    if (!element) return;
    const onWheel = (event: WheelEvent) => {
      event.preventDefault();
      const bounds = element.getBoundingClientRect();
      const pointerX = event.clientX - bounds.left;
      const pointerY = event.clientY - bounds.top;
      const previous = zoomRef.current;
      // a pinch sends small deltas, a mouse wheel larger ones
      const next = clampZoom(previous * Math.exp(-event.deltaY * (event.ctrlKey ? 0.01 : 0.0015)));
      if (next === previous) return;
      const contentX = (element.scrollLeft + pointerX) / previous;
      const contentY = (element.scrollTop + pointerY) / previous;
      zoomRef.current = next;
      setZoom(next);
      requestAnimationFrame(() => {
        element.scrollLeft = contentX * next - pointerX;
        element.scrollTop = contentY * next - pointerY;
      });
    };
    element.addEventListener("wheel", onWheel, { passive: false });
    return () => element.removeEventListener("wheel", onWheel);
  }, []);
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
    const maxTop = Math.max(58, bounds.height / zoom - DEPLOY_CARD_SIZE.height - 16);
    updatePosition(nodeId, {
      left: Math.max(16, Math.min(maxLeft, (event.clientX - bounds.left) / zoom - dragOffset.current.x)),
      top: Math.max(58, Math.min(maxTop, (event.clientY - bounds.top) / zoom - dragOffset.current.y)),
    });
  }

  const stages = stagesOf(plan, rows);
  // The hops, as the Flow draws them: from a range to every range that starts on
  // the next layer. The heads (layer 0) are where the pipeline leaves from, never
  // a hop from one head to another; a prefill machine also hands its state to
  // the machines that decode the same layers.
  const assignedRows = rows.filter((row) => row.target_node_id && nodeIds.includes(row.target_node_id));
  const bridgeIds = new Set<string>();
  for (const from of assignedRows) {
    for (const to of assignedRows) {
      if (from.target_node_id !== to.target_node_id && to.start_layer === from.end_layer + 1) {
        bridgeIds.add(edgeId(from.target_node_id, to.target_node_id));
      }
    }
  }
  for (let index = 1; index < stages.length; index += 1) {
    const previous = stages[index - 1];
    const stage = stages[index];
    if (previous.prefill && !stage.prefill && previous.start_layer === stage.start_layer && previous.end_layer === stage.end_layer) {
      for (const from of previous.nodeIds) for (const to of stage.nodeIds) if (from !== to) bridgeIds.add(edgeId(from, to));
    }
  }
  // The backend already limits the options to the hops. This guard in the canvas
  // keeps the visual rule even while an old peer still answers with the
  // previous broad list: no jump over a layer appears on the map.
  const visibleRouteOptions = (plan.route_options ?? []).filter(
    (route) => bridgeIds.has(edgeId(route.from_node_id, route.to_node_id)),
  );
  const selectedEdges = new Set(visibleRouteOptions.filter((route) => route.selected).map((route) => edgeId(route.from_node_id, route.to_node_id)));
  const candidateEdges = new Set(visibleRouteOptions.filter((route) => !route.selected).map((route) => edgeId(route.from_node_id, route.to_node_id)));
  const edges = visibleRouteOptions.filter((route) => nodeIds.includes(route.from_node_id) && nodeIds.includes(route.to_node_id));
  const principalEdges = [...bridgeIds]
    .map((id) => { const [from, to] = id.split(":"); return { from, to }; })
    .filter((edge) => !edges.some((route) => route.from_node_id === edge.from && route.to_node_id === edge.to));
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
  // The answer leaves from the machines that run the last layer (a prefill machine never answers).
  const lastLayer = assignedRows.length > 0 ? Math.max(...assignedRows.map((row) => row.end_layer)) : null;
  const tailRows = assignedRows.filter((row) => row.end_layer === lastLayer);
  const answeringRows = tailRows.some((row) => roleOf(rows, plan, row.target_node_id) !== "prefill")
    ? tailRows.filter((row) => roleOf(rows, plan, row.target_node_id) !== "prefill")
    : tailRows;
  const terminalNodeIds = [...new Set(answeringRows.map((row) => row.target_node_id))];
  // The legend reads like the columns: the heads (layer 0) together, then each
  // later starting layer, the machines that start there joined by "|".
  const legendSteps = [...new Set(assignedRows.map((row) => row.start_layer))]
    .sort((a, b) => a - b)
    .map((start) => ({
      start,
      nodeIds: [...new Set(assignedRows.filter((row) => row.start_layer === start).map((row) => row.target_node_id))],
    }));
  const gatewayPosition = pipelineGatewayPosition(
    terminalNodeIds.map((nodeId) => positionOf(nodeId, nodeIds.indexOf(nodeId))),
    terminalNodeIds.map(() => DEPLOY_CARD_SIZE),
  );

  return (
    // The toolbar, the legend and the route chips sit on the section, outside
    // the area that scrolls, so they stay put when the map scrolls.
    <section
      className="relative h-full min-h-0 w-full overflow-hidden"
      onDragOver={(event) => event.preventDefault()}
      onDrop={(event) => {
        event.preventDefault();
        const nodeId = event.dataTransfer.getData("application/x-pom-node");
        if (nodeId) onDropPeer(nodeId);
      }}
    >
      <div
        ref={canvas}
        data-testid="deploy-map-scroll"
        className={clsx(
          "absolute inset-0 overflow-auto bg-[radial-gradient(circle_at_1px_1px,rgba(91,78,255,.16)_1px,transparent_0)]",
          panning ? "cursor-grabbing select-none" : "cursor-grab",
        )}
        style={{ backgroundSize: "18px 18px" }}
        onPointerDown={(event) => {
          // a card, a button or a field keeps its own gesture
          if (event.button !== 0 || (event.target as Element).closest("article, button, a, input, select, textarea")) return;
          const element = event.currentTarget;
          pan.current = { x: event.clientX, y: event.clientY, left: element.scrollLeft, top: element.scrollTop };
          element.setPointerCapture?.(event.pointerId);
          setPanning(true);
        }}
        onPointerMove={(event) => {
          const start = pan.current;
          if (!start) return;
          event.currentTarget.scrollLeft = start.left - (event.clientX - start.x);
          event.currentTarget.scrollTop = start.top - (event.clientY - start.y);
        }}
        onPointerUp={(event) => {
          if (!pan.current) return;
          pan.current = null;
          setPanning(false);
          if (event.currentTarget.hasPointerCapture?.(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId);
        }}
        onPointerCancel={() => { pan.current = null; setPanning(false); }}
      >
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
              const geometry = routeGeometry(from, to, DEPLOY_CARD_SIZE, DEPLOY_CARD_SIZE);
              const id = edgeId(route.from_node_id, route.to_node_id);
              const state: EdgeState = selectedEdges.has(id) || (!selectedEdges.has(id) && route.selected) ? "principal" : candidateEdges.has(id) ? "paralela" : "inativa";
              const style = edgeStyle(state);
              return <path key={`${id}-${index}`} d={`M ${geometry.x1} ${geometry.y1} C ${geometry.c1x} ${geometry.c1y}, ${geometry.c2x} ${geometry.c2y}, ${geometry.x2} ${geometry.y2}`} className={style.className} strokeWidth={style.strokeWidth} strokeDasharray={style.strokeDasharray} markerEnd={style.markerEnd?.replace("route-arrow", "deploy-route-arrow")} />;
            })}
            {gatewayPosition && terminalNodeIds.map((nodeId) => {
              const geometry = gatewayRouteGeometry(positionOf(nodeId, nodeIds.indexOf(nodeId)), gatewayPosition, DEPLOY_CARD_SIZE);
              return <path key={`gateway-${nodeId}`} d={`M ${geometry.x1} ${geometry.y1} C ${geometry.c1x} ${geometry.c1y}, ${geometry.c2x} ${geometry.c2y}, ${geometry.x2} ${geometry.y2}`} className="fill-none stroke-aqua" strokeWidth="3" markerEnd="url(#deploy-route-arrow)" />;
            })}
          </svg>
          {allEdges.map((route, index) => {
            const fromIndex = nodeIds.indexOf(route.from_node_id);
            const toIndex = nodeIds.indexOf(route.to_node_id);
            if (fromIndex < 0 || toIndex < 0) return null;
            const from = positionOf(route.from_node_id, fromIndex);
            const to = positionOf(route.to_node_id, toIndex);
            const geometry = routeGeometry(from, to, DEPLOY_CARD_SIZE, DEPLOY_CARD_SIZE);
            return <RouteRttLabel key={`rtt-${route.from_node_id}-${route.to_node_id}-${index}`} rttMs={route.rtt_ms} left={geometry.labelX} top={geometry.labelY} />;
          })}
          {gatewayPosition && terminalNodeIds.map((nodeId) => {
            const geometry = gatewayRouteGeometry(positionOf(nodeId, nodeIds.indexOf(nodeId)), gatewayPosition, DEPLOY_CARD_SIZE);
            const route = plan.route_options?.find((option) => option.from_node_id === nodeId && option.to_node_id === "gateway");
            return <RouteRttLabel key={`rtt-gateway-${nodeId}`} rttMs={route?.rtt_ms} left={geometry.labelX} top={geometry.labelY} />;
          })}
          {machines.map((machine, index) => (
            <NodeCard key={machine.node_id} marked={markedNodeIds?.includes(machine.node_id)} onToggleMarked={onToggleMarked ? () => onToggleMarked(machine.node_id) : undefined} footer={cardFooter?.(machine.node_id)} machine={machine} rows={rows} position={positionOf(machine.node_id, index)} selected={inspectedNodeId === machine.node_id} dragging={dragging === machine.node_id} primaryNodeId={primaryNodeId} onTogglePrimary={onTogglePrimary && (primaryCandidateIds ?? []).includes(machine.node_id) ? () => onTogglePrimary(machine.node_id) : undefined} onInspect={() => onInspect(machine.node_id)} onAddPeer={() => onAddPeer(machine.node_id)} onRemovePeer={() => onRemovePeer(machine.node_id)} onPointerDown={(event) => beginDrag(machine.node_id, index, event)} onPointerMove={(event) => drag(machine.node_id, event)} onPointerUp={(event) => { if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId); setDragging(null); }} />
          ))}
          {gatewayPosition && <PipelineGatewayNode position={gatewayPosition} cost={pipelineCost} />}
        </div>
      )}
      </div>

      <div className="absolute left-4 top-3 z-20 flex max-w-[calc(100%-2rem)] flex-wrap items-center gap-2 border border-line bg-ink/90 p-1.5 shadow-panel backdrop-blur-md">
        <div className="flex items-center gap-2 px-2">
          <span className="eyebrow">{t("deployStudio.mapTitle")}</span>
          <span className="font-mono text-[10px] text-muted">{t("deployStudio.mapSummary", { nodes: machines.length, layers: plan.total_layers ?? "-" })}</span>
        </div>
        <span className="h-5 w-px bg-line" />
        <Button size="sm" aria-label={t("canvas.zoomOut")} title={t("canvas.zoomOut")} onClick={() => setZoom((value) => clampZoom(value - 0.1))}><Minus className="h-4 w-4" /></Button>
        <Button size="sm" aria-label={t("canvas.zoomIn")} title={t("canvas.zoomIn")} onClick={() => setZoom((value) => clampZoom(value + 0.1))}><Plus className="h-4 w-4" /></Button>
        <Button size="sm" aria-label={t("deployStudio.fitCanvas")} title={t("deployStudio.fitCanvas")} onClick={fit}>Fit</Button>
        <Button size="sm" aria-label={t("deployStudio.organizeMachines")} title={t("deployStudio.organizeMachines")} onClick={organize}><LayoutGrid className="h-4 w-4" /></Button>
        <Button size="sm" aria-label={t("deployStudio.resetLayout")} title={t("deployStudio.resetLayout")} onClick={reset}><RotateCcw className="h-4 w-4" /></Button>
      </div>

      {machines.length > 0 && (
          <div className="absolute bottom-3 left-4 z-20 flex items-center gap-2 border border-line bg-ink/90 px-3 py-2 text-[10px] text-muted shadow-panel backdrop-blur">
            <Sparkles className="h-3 w-3 text-aqua" /><span>prefill</span>
            {legendSteps.map((step) => (
              <span key={step.start} className="flex items-center gap-1">
                <ArrowRight className="h-3 w-3 text-faint" />
                <span className={step.start === 0 ? "text-iris" : "text-aqua"}>
                  {step.nodeIds.map(labelOf).join(" | ")}
                </span>
              </span>
            ))}
            <ArrowRight className="h-3 w-3 text-faint" /><span>{t("deployStudio.legendAnswer")}</span>
          </div>
      )}
      {visibleRouteOptions.length > 0 && <div className="absolute bottom-3 right-4 z-20 flex max-w-[48%] gap-1 overflow-x-auto">{visibleRouteOptions.slice(0, 6).map((route) => <span key={`${route.from_node_id}-${route.to_node_id}`} className={clsx("shrink-0 border px-1.5 py-0.5 font-mono text-[10px]", route.selected ? "border-aqua/30 bg-aqua/10 text-aqua" : "border-line text-faint")}>{labelOf(route.from_node_id)} → {labelOf(route.to_node_id)} · {route.rtt_ms == null ? "RTT -" : `${route.rtt_ms.toFixed(1)} ms`}</span>)}</div>}
    </section>
  );
}
