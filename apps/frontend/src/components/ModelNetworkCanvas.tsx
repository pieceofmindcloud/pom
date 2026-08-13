import clsx from "clsx";
import { Minus, Plus, RotateCcw } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { machineKind, MachineIcon } from "./MachineIcon";
import { Button, Dot } from "./ui";
import { graphFromCluster, type GraphRange } from "../lib/networkGraph";
import { useI18n } from "../i18n/provider";
import {
  clearPositions,
  deploymentLayoutKey,
  fallbackPosition,
  readPositions,
  writePositions,
  type CanvasPosition,
} from "../lib/networkLayout";
import { isServing, serverStateLabel, serverStateTone } from "../lib/format";
import type { ClusterView, RoutingEdge } from "../lib/types";

export type EdgeState = "principal" | "paralela" | "inativa";

/** Três estados, porque com réplicas "escolhida ou não" deixou de bastar:
 *  uma aresta pode não estar em uso e ainda assim ser um caminho pronto. */
export function edgeStyle(state: EdgeState) {
  if (state === "principal") {
    return { className: "fill-none stroke-aqua", strokeWidth: "3", strokeDasharray: undefined, markerEnd: "url(#route-arrow)" };
  }
  if (state === "paralela") {
    return { className: "fill-none stroke-iris", strokeWidth: "2", strokeDasharray: undefined, markerEnd: undefined };
  }
  return { className: "fill-none stroke-faint/70", strokeWidth: "2", strokeDasharray: "7 7", markerEnd: undefined };
}

const CARD_WIDTH = 264;
const CARD_HEIGHT = 202;

function keyFor(range: Pick<GraphRange, "node_id" | "model_id" | "start_layer" | "end_layer">) {
  return `${range.node_id}:${range.model_id}:${range.start_layer}:${range.end_layer}`;
}

function rangeForEdge(ranges: GraphRange[], edge: RoutingEdge, side: "from" | "to") {
  const id = side === "from" ? edge.from_node_id : edge.to_node_id;
  const boundary = side === "from" ? edge.from_end_layer : edge.to_start_layer;
  return ranges.find((range) => range.node_id === id && (side === "from" ? range.end_layer === boundary : range.start_layer === boundary))
    ?? ranges.find((range) => range.node_id === id);
}

function Meter({ label, used, total, unknown = false }: { label: string; used: number | null; total: number | null; unknown?: boolean }) {
  const percent = used != null && total != null && total > 0 ? Math.min(100, (used / total) * 100) : null;
  const tone = percent != null && percent >= 90 ? "bg-rose" : percent != null && percent >= 70 ? "bg-amber" : "bg-aqua";
  const value = unknown || percent == null ? "desconhecido" : `${used!.toFixed(1)} / ${total!.toFixed(1)} GiB`;
  return <div className="mt-2">
    <div className="mb-1 flex justify-between gap-2 text-[9px] uppercase tracking-[0.12em] text-faint"><span>{label}</span><span className="normal-case tracking-normal text-muted">{value}</span></div>
    <div className={clsx("h-1.5 overflow-hidden rounded-full bg-line", (unknown || percent == null) && "bg-[repeating-linear-gradient(135deg,transparent_0,transparent_4px,rgba(124,135,165,.3)_4px,rgba(124,135,165,.3)_6px)]")} aria-label={`${label}: ${value}`}>
      {percent != null && <div className={clsx("h-full rounded-full", tone)} style={{ width: `${percent}%` }} />}
    </div>
  </div>;
}

function NetworkNode({ range, position, dragging, onPointerDown, onPointerMove, onPointerUp }: { range: GraphRange; position: CanvasPosition; dragging: boolean; onPointerDown: (event: React.PointerEvent<HTMLElement>) => void; onPointerMove: (event: React.PointerEvent<HTMLElement>) => void; onPointerUp: (event: React.PointerEvent<HTMLElement>) => void }) {
  const system = range.node?.system;
  const gpu = system?.gpus[0];
  const vramUsed = gpu?.vram_used_mb != null ? gpu.vram_used_mb / 1024 : null;
  const vramTotal = gpu?.vram_total_mb != null ? gpu.vram_total_mb / 1024 : null;
  const ramUsed = system ? system.ram.used_mb / 1024 : null;
  const ramTotal = system ? system.ram.total_mb / 1024 : null;
  const hostname = range.node?.hostname || range.short_id;
  const serving = isServing(range.node?.status.state);
  const unreachable = Boolean(range.unreachable);
  const kind = machineKind(system);
  return <article
    onPointerDown={onPointerDown}
    onPointerMove={onPointerMove}
    onPointerUp={onPointerUp}
    onPointerCancel={onPointerUp}
    className={clsx("absolute select-none rounded-xl border bg-ink/95 p-3.5 shadow-panel backdrop-blur transition-[box-shadow,border-color]", dragging ? "cursor-grabbing border-iris shadow-lift" : "cursor-grab border-line-strong hover:border-iris/55 hover:shadow-lift", unreachable && "opacity-75")}
    style={{ left: position.left, top: position.top, width: CARD_WIDTH, minHeight: CARD_HEIGHT, touchAction: "none" }}
  >
    <div className="absolute inset-x-3.5 top-0 h-px bg-iris/70" />
    <div className="flex items-start justify-between gap-3">
      <div className="flex min-w-0 items-center gap-2">
        <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-line bg-raised"><MachineIcon system={system} size="sm" /></span>
        <div className="min-w-0"><h3 className="truncate text-sm font-semibold text-fg">{hostname}</h3><p className="truncate text-[10px] text-faint">{kind.label}</p></div>
      </div>
      <span className="flex shrink-0 items-center gap-1.5 text-[11px] text-muted"><Dot tone={unreachable ? "rose" : serverStateTone(range.node?.status.state)} pulse={serving && !unreachable} />{unreachable ? "timeout" : serverStateLabel(range.node?.status.state)}</span>
    </div>
    <div className="mt-3 flex items-center justify-between rounded-lg border border-line bg-raised/90 px-2.5 py-1.5"><span className="font-mono text-xs font-semibold text-iris">L{range.start_layer} - L{range.end_layer}</span>{range.load > 0 && <span className="font-mono text-[10px] text-amber">{range.load} em voo</span>}</div>
    <Meter label="VRAM" used={vramUsed} total={vramTotal} unknown={!gpu || vramTotal == null} />
    <Meter label="Memória" used={ramUsed} total={ramTotal} unknown={!system} />
    <Meter label="Carga" used={range.load} total={8} />
  </article>;
}

export function ModelNetworkCanvas({ cluster }: { cluster: ClusterView }) {
  const { t } = useI18n();
  const graph = useMemo(() => graphFromCluster(cluster), [cluster]);
  const [model, setModel] = useState<string | undefined>();
  const [zoom, setZoom] = useState(1);
  const [positions, setPositions] = useState<Record<string, CanvasPosition>>({});
  const [dragging, setDragging] = useState<string | null>(null);
  const canvas = useRef<HTMLDivElement>(null);
  const dragOffset = useRef({ x: 0, y: 0 });
  const modelIds = useMemo(() => Array.from(new Set([
    ...cluster.model_inventory.map((inventory) => inventory.model_id),
    ...graph.models,
    ...(cluster.active_model ? [cluster.active_model] : []),
  ])).sort(), [cluster.active_model, cluster.model_inventory, graph.models]);
  const selected = model && modelIds.includes(model) ? model : cluster.active_model || modelIds[0];
  const ranges = selected ? graph.byModel[selected] : [];
  const inventory = cluster.model_inventory.find((entry) => entry.model_id === selected);
  const layoutKey = useMemo(() => deploymentLayoutKey(selected ?? "", ranges.map((range) => ({ node_id: keyFor(range), start_layer: range.start_layer, end_layer: range.end_layer, changed_at: range.node?.status.changed_at }))), [ranges, selected]);
  const selectedEdges = selected ? cluster.routing_edges.filter((edge) => edge.model_id === selected) : [];
  const candidates = selected ? cluster.candidate_edges.filter((edge) => edge.model_id === selected) : [];
  const selectedEdgeIds = new Set(selectedEdges.map((edge) => `${edge.from_node_id}:${edge.to_node_id}:${edge.from_end_layer}:${edge.to_start_layer}`));
  const candidateIds = new Set(candidates.map((edge) => `${edge.from_node_id}:${edge.to_node_id}:${edge.from_end_layer}:${edge.to_start_layer}`));
  const edges = [...candidates, ...selectedEdges.filter((edge) => !candidates.some((candidate) => candidate.from_node_id === edge.from_node_id && candidate.to_node_id === edge.to_node_id && candidate.from_end_layer === edge.from_end_layer && candidate.to_start_layer === edge.to_start_layer))];

  useEffect(() => { setPositions(readPositions(layoutKey)); }, [layoutKey]);

  const positionOf = (range: GraphRange, index: number) => positions[keyFor(range)] ?? fallbackPosition(index);
  const updatePosition = (id: string, next: CanvasPosition) => setPositions((current) => {
    const updated = { ...current, [id]: next };
    writePositions(layoutKey, updated);
    return updated;
  });

  const reset = () => { clearPositions(layoutKey); setPositions({}); setZoom(1); };
  const fit = () => setZoom(1);

  function beginDrag(range: GraphRange, index: number, event: React.PointerEvent<HTMLElement>) {
    const surface = canvas.current;
    if (!surface) return;
    const bounds = surface.getBoundingClientRect();
    const current = positionOf(range, index);
    dragOffset.current = {
      x: (event.clientX - bounds.left) / zoom - current.left,
      y: (event.clientY - bounds.top) / zoom - current.top,
    };
    setDragging(keyFor(range));
    event.currentTarget.setPointerCapture(event.pointerId);
  }

  function drag(range: GraphRange, event: React.PointerEvent<HTMLElement>) {
    if (dragging !== keyFor(range) || !canvas.current) return;
    const bounds = canvas.current.getBoundingClientRect();
    const maxLeft = Math.max(16, bounds.width / zoom - CARD_WIDTH - 16);
    const maxTop = Math.max(58, bounds.height / zoom - CARD_HEIGHT - 16);
    const left = Math.max(16, Math.min(maxLeft, (event.clientX - bounds.left) / zoom - dragOffset.current.x));
    const top = Math.max(58, Math.min(maxTop, (event.clientY - bounds.top) / zoom - dragOffset.current.y));
    updatePosition(keyFor(range), { left, top });
  }

  return <section ref={canvas} className="relative h-full min-h-0 w-full overflow-hidden bg-[radial-gradient(circle_at_1px_1px,rgba(91,78,255,.16)_1px,transparent_0)]" style={{ backgroundSize: "18px 18px" }}>
    <div className="absolute left-4 top-3 z-20 flex max-w-[calc(100%-2rem)] flex-wrap items-center gap-2 rounded-xl border border-line bg-ink/90 p-1.5 shadow-panel backdrop-blur-md">
      <div className="flex max-w-[min(46rem,60vw)] overflow-x-auto" role="group" aria-label="Modelo">{modelIds.length ? modelIds.map((name) => <button key={name} onClick={() => setModel(name)} aria-pressed={selected === name} className={clsx("h-8 shrink-0 rounded-md px-3 font-mono text-xs", selected === name ? "bg-iris text-white" : "text-muted hover:bg-raised hover:text-fg")}>{name}</button>) : <span className="px-2 text-xs text-muted">Nenhum modelo implantado</span>}</div>
      {inventory && <span className={clsx("shrink-0 rounded-md px-2 py-1 font-mono text-[10px]", inventory.complete ? "bg-aqua/15 text-aqua" : "bg-amber/15 text-amber")}>L{inventory.covered_layers}/{inventory.total_layers}</span>}
      <span className="h-5 w-px bg-line" />
      <Button size="sm" aria-label="Diminuir zoom" onClick={() => setZoom((value) => Math.max(.65, value - .1))}><Minus className="h-4 w-4" /></Button>
      <Button size="sm" aria-label="Aumentar zoom" onClick={() => setZoom((value) => Math.min(1.25, value + .1))}><Plus className="h-4 w-4" /></Button>
      <Button size="sm" aria-label="Ajustar à rede" onClick={fit}>Fit</Button>
      <Button size="sm" aria-label="Reorganizar nós automaticamente" onClick={reset}><RotateCcw className="h-4 w-4" /></Button>
    </div>
    {ranges.length === 0 ? <div className="flex h-full items-center justify-center text-sm text-muted">Nenhuma capacidade de inferência anunciada ainda.</div> : <div className="absolute inset-0 origin-top-left" style={{ transform: `scale(${zoom})`, width: `${100 / zoom}%`, height: `${100 / zoom}%` }}>
      <svg aria-hidden="true" className="pointer-events-none absolute inset-0 h-full w-full overflow-visible">
        <defs><marker id="route-arrow" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><path d="M0,0 L8,4 L0,8 Z" className="fill-aqua" /></marker></defs>
        {edges.map((edge) => {
          const from = rangeForEdge(ranges, edge, "from"); const to = rangeForEdge(ranges, edge, "to");
          if (!from || !to) return null;
          const fromIndex = ranges.indexOf(from); const toIndex = ranges.indexOf(to);
          const a = positionOf(from, fromIndex); const b = positionOf(to, toIndex);
          const x1 = a.left + CARD_WIDTH; const y1 = a.top + CARD_HEIGHT / 2; const x2 = b.left; const y2 = b.top + CARD_HEIGHT / 2; const bend = Math.max(60, Math.abs(x2 - x1) * .42);
          const id = `${edge.from_node_id}:${edge.to_node_id}:${edge.from_end_layer}:${edge.to_start_layer}`;
          // Uma aresta que está num `candidate_edges` é caminho pronto, não
          // tráfego hipotético: com réplicas, é por onde a rede serviria se a
          // principal saísse.
          const state: EdgeState = selectedEdgeIds.has(id) ? "principal" : candidateIds.has(id) ? "paralela" : "inativa";
          const style = edgeStyle(state);
          return <path key={id} d={`M ${x1} ${y1} C ${x1 + bend} ${y1}, ${x2 - bend} ${y2}, ${x2} ${y2}`} className={style.className} strokeWidth={style.strokeWidth} strokeDasharray={style.strokeDasharray} markerEnd={style.markerEnd} />;
        })}
      </svg>
      {ranges.map((range, index) => <NetworkNode key={keyFor(range)} range={range} position={positionOf(range, index)} dragging={dragging === keyFor(range)} onPointerDown={(event) => beginDrag(range, index, event)} onPointerMove={(event) => drag(range, event)} onPointerUp={(event) => { if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId); setDragging(null); }} />)}
    </div>}
    {ranges.length > 0 && <div className="absolute bottom-3 left-4 z-20 flex items-center gap-3 rounded-lg border border-line bg-ink/90 px-3 py-2 text-[10px] text-muted shadow-panel backdrop-blur"><span className="flex items-center gap-1.5"><i className="h-0.5 w-5 bg-aqua" />{t("canvas.legendPrimary")}</span><span className="flex items-center gap-1.5"><i className="h-0.5 w-5 bg-iris" />{t("canvas.legendParallel")}</span><span className="flex items-center gap-1.5"><i className="h-0.5 w-5 border-t-2 border-dashed border-faint" />{t("canvas.legendInactive")}</span></div>}
  </section>;
}
