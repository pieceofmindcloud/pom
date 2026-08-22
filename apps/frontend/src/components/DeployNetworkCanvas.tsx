import clsx from "clsx";
import { ArrowRight, LayoutGrid, Minus, Plus, RotateCcw, ServerCog, Sparkles, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type PointerEvent } from "react";
import { layerRange, megabytes } from "../lib/format";
import {
  autoLayout,
  CARD_HEIGHT,
  CARD_WIDTH,
  clearPositions,
  deploymentLayoutKey,
  fallbackPosition,
  mergePositions,
  readPositions,
  writePositions,
  type CanvasPosition,
} from "../lib/networkLayout";
import type { DeployMachine, DeployPlan, DeploySelection, DeployStep } from "../lib/types";
import { edgeStyle, type EdgeState } from "./ModelNetworkCanvas";
import { Badge, Button, Dot, Progress } from "./ui";

type DeployNetworkCanvasProps = {
  plan: DeployPlan;
  rows: DeploySelection[];
  inspectedNodeId: string | null;
  onInspect: (nodeId: string) => void;
  onAddPeer: (nodeId: string) => void;
  onRemovePeer: (nodeId: string) => void;
  onDropPeer: (nodeId: string) => void;
  labelOf: (nodeId: string) => string;
  extraNodeIds?: string[];
  showAllMachines?: boolean;
};

const EMPTY_NODE_IDS: string[] = [];

function machineRows(rows: DeploySelection[], nodeId: string) {
  return rows.filter((row) => row.target_node_id === nodeId).sort((a, b) => a.start_layer - b.start_layer);
}

function edgeId(from: string, to: string) {
  return `${from}:${to}`;
}

function executionRange(step: DeployStep): [number, number] {
  return [step.exec_start ?? step.start_layer, step.exec_end ?? step.end_layer];
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

function NodeCard({
  machine,
  steps,
  position,
  selected,
  dragging,
  onInspect,
  onAddPeer,
  onRemovePeer,
  onPointerDown,
  onPointerMove,
  onPointerUp,
}: {
  machine: DeployMachine;
  steps: DeployStep[];
  position: CanvasPosition;
  selected: boolean;
  dragging: boolean;
  onInspect: () => void;
  onAddPeer: () => void;
  onRemovePeer: () => void;
  onPointerDown: (event: PointerEvent<HTMLElement>) => void;
  onPointerMove: (event: PointerEvent<HTMLElement>) => void;
  onPointerUp: (event: PointerEvent<HTMLElement>) => void;
}) {
  const taking = steps
    .filter((step) => step.target_node_id === machine.node_id)
    .sort((a, b) => a.start_layer - b.start_layer || a.end_layer - b.end_layer);
  const active = taking.length > 0;
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
              <span className="truncate">{active ? "na implantação" : "disponível para o plano"}</span>
              {machine.rtt_ms != null && <><span aria-hidden="true">·</span><span className="shrink-0">RTT {machine.rtt_ms.toFixed(1)} ms</span></>}
            </p>
          </div>
        </div>
        <div className="flex items-center gap-1">
          <Dot tone={active ? "aqua" : "neutral"} pulse={active} />
          <button
            type="button"
            aria-label={`Remover ${machine.label} da implantação`}
            title="Remover da implantação"
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
      <div className="mt-3 space-y-1.5 rounded-lg border border-line bg-raised/90 px-2 py-1.5">
        {active ? taking.map((step) => {
          const [execStart, execEnd] = executionRange(step);
          return (
            <div key={`${step.start_layer}-${step.end_layer}-${execStart}-${execEnd}`} className="flex min-w-0 items-center justify-between gap-2">
              <Badge tone="iris" mono className="min-w-0 truncate px-1.5 py-0 text-[0.62rem]">
                carrega {layerRange(step.start_layer, step.end_layer)}
              </Badge>
              <span className="shrink-0 font-mono text-[0.62rem] text-aqua">
                executa {layerRange(execStart, execEnd)}
              </span>
            </div>
          );
        }) : <span className="font-mono text-[10px] text-faint">solte aqui para incluir</span>}
      </div>
      <div className="mt-3 space-y-2">
        <CapacityBar
          label="GPU"
          value={machine.gpu_used_pct ?? null}
          detail={machine.gpu_used_pct == null ? "sem GPU" : `${machine.gpu_used_pct.toFixed(0)}% · ${megabytes(machine.gpu_free_mb)}`}
          tone={machine.gpu_used_pct != null && machine.gpu_used_pct >= 88 ? "amber" : "aqua"}
        />
        <CapacityBar
          label="RAM"
          value={machine.ram_used_pct ?? null}
          detail={machine.ram_used_pct == null ? "-" : `${machine.ram_used_pct.toFixed(0)}% · ${megabytes(machine.ram_free_mb)}`}
          tone={machine.ram_used_pct != null && machine.ram_used_pct >= 88 ? "amber" : "iris"}
        />
        <CapacityBar
          label="Carga"
          value={Math.min(100, (machine.load / 8) * 100)}
          detail={`${machine.load} em voo`}
          tone={machine.load >= 7 ? "amber" : "aqua"}
        />
      </div>
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
  extraNodeIds = EMPTY_NODE_IDS,
  showAllMachines = false,
}: DeployNetworkCanvasProps) {
  const canvas = useRef<HTMLDivElement>(null);
  const dragOffset = useRef({ x: 0, y: 0 });
  const [zoom, setZoom] = useState(1);
  const [positions, setPositions] = useState<Record<string, CanvasPosition>>({});
  const [dragging, setDragging] = useState<string | null>(null);

  const machines = useMemo(() => {
    const ids = new Set(rows.map((row) => row.target_node_id).filter(Boolean));
    for (const step of plan.steps) if (step.target_node_id) ids.add(step.target_node_id);
    if (showAllMachines) for (const machine of plan.machines) ids.add(machine.node_id);
    for (const nodeId of extraNodeIds) ids.add(nodeId);
    return [...ids]
      .map((nodeId) => plan.machines.find((machine) => machine.node_id === nodeId))
      .filter((machine): machine is DeployMachine => Boolean(machine));
  }, [extraNodeIds, plan.machines, plan.steps, rows, showAllMachines]);
  const nodeIds = useMemo(() => machines.map((machine) => machine.node_id), [machines]);
  const layoutKey = useMemo(
    () => deploymentLayoutKey(plan.deployment_name || plan.group, machines.map((machine) => ({ node_id: machine.node_id, ...machineBounds(rows, machine) }))),
    [machines, plan.deployment_name, plan.group, rows],
  );
  const arrangement = useMemo(
    () => autoLayout(machines.map((machine) => ({ id: machine.node_id, ...machineBounds(rows, machine) }))),
    [machines, rows],
  );
  const positionOf = (nodeId: string, index: number) => positions[nodeId] ?? arrangement[nodeId] ?? fallbackPosition(index);

  useEffect(() => {
    setPositions((current) => {
      const next = mergePositions(current, readPositions(layoutKey), arrangement, nodeIds);
      writePositions(layoutKey, next);
      return next;
    });
  }, [arrangement, layoutKey, nodeIds]);

  const updatePosition = (nodeId: string, next: CanvasPosition) => setPositions((current) => {
    const updated = { ...current, [nodeId]: next };
    writePositions(layoutKey, updated);
    return updated;
  });
  const organize = () => {
    setPositions((current) => {
      const manuallyMoved = Object.fromEntries(Object.entries(current).filter(([id]) => id in arrangement));
      const next = { ...arrangement, ...manuallyMoved };
      writePositions(layoutKey, next);
      return next;
    });
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

  // O backend já devolve o grafo de execução: cada aresta representa duas
  // sub-faixas contíguas. O canvas não reconstrói estágios pelas faixas
  // carregadas, porque isso apagaria ramos quando o inventário se sobrepõe.
  const visibleRouteOptions = (plan.route_options ?? []).filter(
    (route) => nodeIds.includes(route.from_node_id) && nodeIds.includes(route.to_node_id),
  );
  const selectedEdges = new Set(visibleRouteOptions.filter((route) => route.selected).map((route) => edgeId(route.from_node_id, route.to_node_id)));
  const candidateEdges = new Set(visibleRouteOptions.filter((route) => !route.selected).map((route) => edgeId(route.from_node_id, route.to_node_id)));
  const allEdges = visibleRouteOptions;
  const entryNodeIds = [...new Set(plan.steps
    .filter((step) => step.target_node_id && executionRange(step)[0] === 0)
    .map((step) => step.target_node_id))];

  return (
    <section
      ref={canvas}
      className="relative h-full min-h-0 w-full overflow-hidden bg-[radial-gradient(circle_at_1px_1px,rgba(91,78,255,.16)_1px,transparent_0)]"
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
          <span className="eyebrow">Mapa da implantação</span>
          <span className="font-mono text-[10px] text-muted">{machines.length} nós · {plan.total_layers ?? "-"} camadas</span>
        </div>
        <span className="h-5 w-px bg-line" />
        <Button size="sm" aria-label="Diminuir zoom" title="Diminuir zoom" onClick={() => setZoom((value) => Math.max(0.65, value - 0.1))}><Minus className="h-4 w-4" /></Button>
        <Button size="sm" aria-label="Aumentar zoom" title="Aumentar zoom" onClick={() => setZoom((value) => Math.min(1.25, value + 0.1))}><Plus className="h-4 w-4" /></Button>
        <Button size="sm" aria-label="Ajustar ao canvas" title="Ajustar ao canvas" onClick={fit}>Fit</Button>
        <Button size="sm" aria-label="Organizar máquinas" title="Organizar máquinas" onClick={organize}><LayoutGrid className="h-4 w-4" /></Button>
        <Button size="sm" aria-label="Redefinir disposição" title="Redefinir disposição" onClick={reset}><RotateCcw className="h-4 w-4" /></Button>
      </div>

      {machines.length === 0 ? (
        <div className="flex h-full items-center justify-center text-sm text-muted">Solte uma máquina aqui para montar a topologia.</div>
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
              const x1 = from.left + CARD_WIDTH;
              const y1 = from.top + CARD_HEIGHT / 2;
              const x2 = to.left;
              const y2 = to.top + CARD_HEIGHT / 2;
              const bend = Math.max(60, Math.abs(x2 - x1) * 0.42);
              const id = edgeId(route.from_node_id, route.to_node_id);
              const state: EdgeState = selectedEdges.has(id) || (!selectedEdges.has(id) && route.selected) ? "principal" : candidateEdges.has(id) ? "paralela" : "inativa";
              const style = edgeStyle(state);
              return <path key={`${id}-${index}`} d={`M ${x1} ${y1} C ${x1 + bend} ${y1}, ${x2 - bend} ${y2}, ${x2} ${y2}`} className={style.className} strokeWidth={style.strokeWidth} strokeDasharray={style.strokeDasharray} markerEnd={style.markerEnd?.replace("route-arrow", "deploy-route-arrow")} />;
            })}
          </svg>
          {allEdges.map((route, index) => {
            const fromIndex = nodeIds.indexOf(route.from_node_id);
            const toIndex = nodeIds.indexOf(route.to_node_id);
            if (fromIndex < 0 || toIndex < 0) return null;
            const from = positionOf(route.from_node_id, fromIndex);
            const to = positionOf(route.to_node_id, toIndex);
            const x = (from.left + CARD_WIDTH + to.left) / 2;
            const y = (from.top + to.top + CARD_HEIGHT) / 2;
            return <span key={`rtt-${route.from_node_id}-${route.to_node_id}-${index}`} className="absolute z-10 -translate-x-1/2 rounded-md border border-aqua/25 bg-ink/95 px-1.5 py-0.5 font-mono text-[10px] text-aqua" style={{ left: x, top: y }}>{route.rtt_ms == null ? "RTT -" : `RTT ${route.rtt_ms.toFixed(1)} ms`}</span>;
          })}
          {machines.map((machine, index) => (
            <NodeCard key={machine.node_id} machine={machine} steps={plan.steps} position={positionOf(machine.node_id, index)} selected={inspectedNodeId === machine.node_id} dragging={dragging === machine.node_id} onInspect={() => onInspect(machine.node_id)} onAddPeer={() => onAddPeer(machine.node_id)} onRemovePeer={() => onRemovePeer(machine.node_id)} onPointerDown={(event) => beginDrag(machine.node_id, index, event)} onPointerMove={(event) => drag(machine.node_id, event)} onPointerUp={(event) => { if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId); setDragging(null); }} />
          ))}
          <div className="absolute bottom-3 left-4 flex max-w-[calc(100%-2rem)] flex-wrap items-center gap-2 border border-line bg-ink/90 px-3 py-2 text-[10px] text-muted shadow-panel backdrop-blur">
            <Sparkles className="h-3 w-3 shrink-0 text-aqua" />
            <span className="uppercase tracking-[0.12em] text-faint">entrada</span>
            {entryNodeIds.length > 0 ? entryNodeIds.map((nodeId) => <span key={nodeId} className="flex min-w-0 items-center gap-1 text-iris"><ArrowRight className="h-3 w-3 shrink-0 text-faint" /><span className="truncate">{labelOf(nodeId)}</span></span>) : <span className="text-amber">sem faixa em L0</span>}
            <ArrowRight className="h-3 w-3 shrink-0 text-faint" /><span>execução contígua</span>
            <ArrowRight className="h-3 w-3 shrink-0 text-faint" /><span>resposta</span>
          </div>
        </div>
      )}
      {visibleRouteOptions.length > 0 && <div className="absolute bottom-3 right-4 z-20 flex max-w-[48%] gap-1 overflow-x-auto">{visibleRouteOptions.slice(0, 6).map((route) => <span key={`${route.from_node_id}-${route.to_node_id}`} className={clsx("shrink-0 border px-1.5 py-0.5 font-mono text-[10px]", route.selected ? "border-aqua/30 bg-aqua/10 text-aqua" : "border-line text-faint")}>{labelOf(route.from_node_id)} → {labelOf(route.to_node_id)} · {route.rtt_ms == null ? "RTT -" : `${route.rtt_ms.toFixed(1)} ms`}</span>)}</div>}
    </section>
  );
}
