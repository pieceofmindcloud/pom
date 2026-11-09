import { useId } from "react";

/**
 * Picks a layer range on a whole model with two thumbs on one track.
 *
 * `bounds` are the places a range may start or end, in layers: `0`, every
 * valid cut + 1 and the model's layer count (`rangeBounds`). The thumbs move
 * over those places only, so every range the slider gives is one the GGUF can
 * be cut at; the range is `[bounds[lo], bounds[hi] - 1]`.
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
  const last = bounds.length - 1;
  const lo = nearestIndex(bounds, start);
  const hi = Math.max(lo + 1, nearestIndex(bounds, end + 1));
  const pct = (index: number) => (last > 0 ? (index / last) * 100 : 0);
  const thumb =
    "pointer-events-none absolute inset-x-0 top-1/2 h-4 w-full -translate-y-1/2 appearance-none bg-transparent " +
    "[&::-webkit-slider-thumb]:pointer-events-auto [&::-webkit-slider-thumb]:h-3.5 [&::-webkit-slider-thumb]:w-3.5 " +
    "[&::-webkit-slider-thumb]:cursor-pointer [&::-webkit-slider-thumb]:appearance-none [&::-webkit-slider-thumb]:rounded-full " +
    "[&::-webkit-slider-thumb]:border-2 [&::-webkit-slider-thumb]:border-aqua [&::-webkit-slider-thumb]:bg-ink " +
    "[&::-moz-range-thumb]:pointer-events-auto [&::-moz-range-thumb]:h-3 [&::-moz-range-thumb]:w-3 " +
    "[&::-moz-range-thumb]:cursor-pointer [&::-moz-range-thumb]:rounded-full [&::-moz-range-thumb]:border-2 " +
    "[&::-moz-range-thumb]:border-aqua [&::-moz-range-thumb]:bg-ink disabled:opacity-40";
  return (
    <div className="relative h-5" role="group" aria-labelledby={`${id}-label`}>
      <span id={`${id}-label`} className="sr-only">{label}</span>
      <div className="absolute inset-x-0 top-1/2 h-1 -translate-y-1/2 rounded-full bg-line" />
      <div
        className="absolute top-1/2 h-1 -translate-y-1/2 rounded-full bg-aqua/70"
        style={{ left: `${pct(lo)}%`, width: `${pct(hi) - pct(lo)}%` }}
      />
      <input
        type="range"
        aria-label={`${label}: start`}
        min={0}
        max={last}
        step={1}
        value={lo}
        disabled={disabled}
        onChange={(event) => {
          const next = Math.min(Number(event.target.value), hi - 1);
          onChange(bounds[next], bounds[hi] - 1);
        }}
        className={thumb}
      />
      <input
        type="range"
        aria-label={`${label}: end`}
        min={0}
        max={last}
        step={1}
        value={hi}
        disabled={disabled}
        onChange={(event) => {
          const next = Math.max(Number(event.target.value), lo + 1);
          onChange(bounds[lo], bounds[next] - 1);
        }}
        className={thumb}
      />
    </div>
  );
}

/** Where a range may start or end: 0, each valid end + 1, and the layer count. */
export function rangeBounds(total: number, ends: number[]): number[] {
  return [...new Set([0, ...ends.filter((end) => end < total - 1).map((end) => end + 1), total])].sort((a, b) => a - b);
}

function nearestIndex(bounds: number[], layer: number): number {
  let best = 0;
  for (let index = 1; index < bounds.length; index++) {
    if (Math.abs(bounds[index] - layer) < Math.abs(bounds[best] - layer)) best = index;
  }
  return best;
}
