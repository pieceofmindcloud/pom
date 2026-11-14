import { useId, useRef, useState } from "react";

/**
 * Picks a layer range on a whole model with two thumbs on one track.
 *
 * The track is in layers, so a thumb moves smoothly under the pointer; the
 * range it gives snaps to `bounds`, the places a range may start or end: `0`,
 * every valid cut + 1 and the model's layer count (`rangeBounds`). Every range
 * the slider reports is one the GGUF can be cut at: `[start, end]` with
 * `start` and `end + 1` in `bounds`. On release the thumb settles on the bound.
 *
 * The mouse goes through one layer over the whole track: a press anywhere
 * takes the nearer thumb and drags it with pointer capture, so the pointer may
 * leave the track. The two range inputs under it stay for the keyboard and
 * screen readers only; with stacked inputs the mouse had to hit a 14px thumb
 * and the top input took presses meant for the other.
 */
export function LayerRangeSlider({
  bounds,
  start,
  end,
  label,
  disabled,
  onChange,
}: {
  bounds: number[];
  start: number;
  end: number;
  label: string;
  disabled?: boolean;
  onChange: (start: number, end: number) => void;
}) {
  const id = useId();
  const total = bounds[bounds.length - 1] ?? 0;
  // Where each thumb sits while it is being dragged (in layers), else null.
  const [dragLo, setDragLo] = useState<number | null>(null);
  const [dragHi, setDragHi] = useState<number | null>(null);
  const lo = dragLo ?? start;
  const hi = dragHi ?? end + 1;
  const pct = (layer: number) => (total > 0 ? (layer / total) * 100 : 0);
  const settle = () => { setDragLo(null); setDragHi(null); };
  const moveLo = (value: number) => {
    const raw = Math.max(0, Math.min(value, total - 1));
    setDragLo(raw);
    const upper = snap(bounds, end + 1);
    const next = snap(bounds.filter((bound) => bound < upper), raw);
    if (next !== start) onChange(next, upper - 1);
  };
  const moveHi = (value: number) => {
    const raw = Math.min(total, Math.max(value, 1));
    setDragHi(raw);
    const lower = snap(bounds, start);
    const next = snap(bounds.filter((bound) => bound > lower), raw);
    if (next - 1 !== end) onChange(lower, next - 1);
  };
  const track = useRef<HTMLDivElement>(null);
  const dragging = useRef<"lo" | "hi" | null>(null);
  const layerAt = (clientX: number) => {
    const rect = track.current?.getBoundingClientRect();
    if (!rect || rect.width <= 0) return 0;
    return Math.round(Math.max(0, Math.min(1, (clientX - rect.left) / rect.width)) * total);
  };
  const thumb =
    "pointer-events-none absolute inset-x-0 top-1/2 h-4 w-full -translate-y-1/2 appearance-none bg-transparent " +
    "[&::-webkit-slider-thumb]:h-4 [&::-webkit-slider-thumb]:w-4 " +
    "[&::-webkit-slider-thumb]:appearance-none [&::-webkit-slider-thumb]:rounded-full " +
    "[&::-webkit-slider-thumb]:border-2 [&::-webkit-slider-thumb]:border-aqua [&::-webkit-slider-thumb]:bg-ink " +
    "[&::-moz-range-thumb]:h-3.5 [&::-moz-range-thumb]:w-3.5 " +
    "[&::-moz-range-thumb]:rounded-full [&::-moz-range-thumb]:border-2 " +
    "[&::-moz-range-thumb]:border-aqua [&::-moz-range-thumb]:bg-ink disabled:opacity-40";
  return (
    <div ref={track} className="relative h-6" role="group" aria-labelledby={`${id}-label`}>
      <span id={`${id}-label`} className="sr-only">{label}</span>
      <div className="absolute inset-x-0 top-1/2 h-1 -translate-y-1/2 rounded-full bg-line" />
      {/* the valid cuts, as ticks on the track */}
      {bounds.length <= 64 && bounds.slice(1, -1).map((bound) => (
        <span key={bound} aria-hidden="true" className="absolute top-1/2 h-2 w-px -translate-y-1/2 bg-faint/60" style={{ left: `${pct(bound)}%` }} />
      ))}
      <div
        className="absolute top-1/2 h-1 -translate-y-1/2 rounded-full bg-aqua/70"
        style={{ left: `${pct(Math.min(lo, hi))}%`, width: `${Math.abs(pct(hi) - pct(lo))}%` }}
      />
      <input
        type="range"
        aria-label={`${label}: start`}
        min={0}
        max={total}
        step={1}
        value={lo}
        disabled={disabled}
        onChange={(event) => moveLo(Number(event.target.value))}
        onBlur={settle}
        onKeyUp={settle}
        className={thumb}
      />
      <input
        type="range"
        aria-label={`${label}: end`}
        min={0}
        max={total}
        step={1}
        value={hi}
        disabled={disabled}
        onChange={(event) => moveHi(Number(event.target.value))}
        onBlur={settle}
        onKeyUp={settle}
        className={thumb}
      />
      {/* the mouse: a press takes the nearer thumb, a drag moves it */}
      <div
        aria-hidden="true"
        data-testid="layer-range-drag"
        className={`absolute inset-0 touch-none ${disabled ? "" : "cursor-grab active:cursor-grabbing"}`}
        onClick={(event) => event.stopPropagation()}
        onPointerDown={(event) => {
          if (disabled || event.button !== 0) return;
          event.preventDefault();
          event.stopPropagation();
          const layer = layerAt(event.clientX);
          // the nearer thumb; at equal distance (thumbs together) the side of the press decides
          const toLo = Math.abs(layer - lo);
          const toHi = Math.abs(layer - hi);
          const which = toLo < toHi || (toLo === toHi && layer < hi) ? "lo" : "hi";
          dragging.current = which;
          event.currentTarget.setPointerCapture?.(event.pointerId);
          (which === "lo" ? moveLo : moveHi)(layer);
        }}
        onPointerMove={(event) => {
          if (!dragging.current) return;
          (dragging.current === "lo" ? moveLo : moveHi)(layerAt(event.clientX));
        }}
        onPointerUp={(event) => {
          dragging.current = null;
          event.currentTarget.releasePointerCapture?.(event.pointerId);
          settle();
        }}
        onPointerCancel={() => { dragging.current = null; settle(); }}
      />
    </div>
  );
}

/** Where a range may start or end: 0, each valid end + 1, and the layer count. */
export function rangeBounds(total: number, ends: number[]): number[] {
  return [...new Set([0, ...ends.filter((end) => end < total - 1).map((end) => end + 1), total])].sort((a, b) => a - b);
}

/** The bound nearest to `layer` (the first of two at the same distance). */
function snap(bounds: number[], layer: number): number {
  let best = bounds[0];
  for (const bound of bounds) {
    if (Math.abs(bound - layer) < Math.abs(best - layer)) best = bound;
  }
  return best;
}
