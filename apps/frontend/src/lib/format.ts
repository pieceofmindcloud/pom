/** Number and time formatting, in pt-BR, used everywhere numbers are shown. */

import type { ServerState } from "./types";
import type { Tone } from "../components/ui";
import { getActiveLocale } from "../i18n/locales";

const nf = (digits: number) =>
  new Intl.NumberFormat(getActiveLocale(), {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });

export function bytes(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—";
  if (value < 1024) return `${value} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = value / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${nf(v < 10 ? 1 : 0).format(v)} ${units[i]}`;
}

export function megabytes(mb: number | null | undefined): string {
  if (mb == null || !Number.isFinite(mb)) return "—";
  return bytes(mb * 1024 * 1024);
}

export function pct(value: number | null | undefined, digits = 0): string {
  if (value == null || !Number.isFinite(value)) return "—";
  return `${nf(digits).format(value)}%`;
}

export function num(value: number | null | undefined, digits = 0): string {
  if (value == null || !Number.isFinite(value)) return "—";
  return nf(digits).format(value);
}

export function duration(seconds: number | null | undefined): string {
  if (seconds == null || !Number.isFinite(seconds) || seconds < 0) return "—";
  const s = Math.round(seconds);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}min ${s % 60}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}min`;
}

export function ms(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—";
  if (value < 1000) return `${Math.round(value)} ms`;
  return `${nf(value < 10_000 ? 2 : 1).format(value / 1000)} s`;
}

export function rate(bytesPerSec: number | null | undefined): string {
  if (!bytesPerSec || !Number.isFinite(bytesPerSec)) return "—";
  return `${bytes(bytesPerSec)}/s`;
}

/** "há 4 min", for timestamps the operator reads as recency, not as a clock. */
export function since(iso: string | null | undefined): string {
  if (!iso) return "—";
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return "—";
  const secs = (Date.now() - then) / 1000;
  const portuguese = getActiveLocale() === "pt-BR";
  if (secs < 5) return portuguese ? "agora" : "now";
  if (secs < 60) return portuguese ? `há ${Math.round(secs)}s` : `${Math.round(secs)}s ago`;
  if (secs < 3600) return portuguese ? `há ${Math.round(secs / 60)} min` : `${Math.round(secs / 60)} min ago`;
  if (secs < 86_400) return portuguese ? `há ${Math.round(secs / 3600)} h` : `${Math.round(secs / 3600)} h ago`;
  return portuguese ? `há ${Math.round(secs / 86_400)} d` : `${Math.round(secs / 86_400)} d ago`;
}

export function clock(iso: string | null | undefined): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "—";
  return d.toLocaleTimeString(getActiveLocale(), {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

export function layerRange(start: number, end: number): string {
  return start === end ? `L${start}` : `L${start}–${end}`;
}

/** Last segment of a path, which is what identifies a file on screen. */
export function basename(path: string): string {
  const parts = path.split("/");
  return parts[parts.length - 1] || path;
}

/** Human-readable model label; the catalog identity stays unchanged. */
export function deployModelDisplayName(name: string): string {
  return name
    .replace(/\.gguf$/i, "")
    .replace(/-[0-9a-f]{8}$/i, "");
}

// ── Node state ───────────────────────────────────────────────────────────────

/**
 * Whether a node is actually answering for its layers.
 *
 * One function instead of `state === "..."` spread over the panels: the values
 * come from `ServerState` in `src/runtime.rs`, and every screen that compares
 * them by hand is one rename away from showing a live cluster as idle.
 */
export function isServing(state: ServerState | undefined): boolean {
  return state === "ready";
}

export function serverStateLabel(state: ServerState | undefined): string {
  const portuguese = getActiveLocale() === "pt-BR";
  switch (state) {
    case "ready":
      return portuguese ? "servindo" : "serving";
    case "starting":
      return portuguese ? "carregando" : "loading";
    case "idle":
      return portuguese ? "ocioso" : "idle";
    case "failed":
      return portuguese ? "falhou" : "failed";
    case "thermal":
      return portuguese ? "aguardando resfriar" : "cooling down";
    case "disabled":
      return portuguese ? "sem modelo" : "no model";
    default:
      return portuguese ? "desconhecido" : "unknown";
  }
}

export function serverStateTone(state: ServerState | undefined): Tone {
  switch (state) {
    case "ready":
      return "aqua";
    case "starting":
      return "amber";
    case "idle":
      return "amber";
    case "failed":
      return "rose";
    case "thermal":
      return "amber";
    default:
      return "neutral";
  }
}
