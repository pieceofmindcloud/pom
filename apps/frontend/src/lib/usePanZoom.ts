import { useCallback, useEffect, useRef, useState, type CSSProperties, type PointerEvent } from "react";
import { storageKey } from "./brand";

/** Where the map is: its offset on the surface, in screen pixels, and its zoom. */
export interface MapView {
  x: number;
  y: number;
  zoom: number;
}

const HOME: MapView = { x: 0, y: 0, zoom: 1 };
/** The spacing of the dotted background at zoom 1. */
const GRID = 18;

/** The view saved for a map, or the origin when there is none (or storage is off). */
function readView(key: string | undefined, clamp: (zoom: number) => number): MapView {
  if (!key) return HOME;
  try {
    const saved = JSON.parse(window.localStorage.getItem(storageKey(key)) ?? "null") as Partial<MapView> | null;
    if (saved && [saved.x, saved.y, saved.zoom].every((value) => typeof value === "number" && Number.isFinite(value))) {
      return { x: saved.x!, y: saved.y!, zoom: clamp(saved.zoom!) };
    }
  } catch {
    // private window or blocked storage: the map opens at the origin
  }
  return HOME;
}

/**
 * Free pan and zoom for a map (the Dashboard's and the Deploy studio's).
 *
 * The map is not scrolled: it is translated, so it can be dragged anywhere,
 * past any edge, whether or not its content overflows. Dragging the background
 * moves it; the wheel and the trackpad pinch (a wheel event with ctrlKey) zoom
 * around the pointer; the zoom buttons zoom around the surface's centre. The
 * dotted background moves and scales with the map, so the drag reads as a drag.
 * With a `persistKey`, the last view is kept in this browser (`localStorage`)
 * and the map reopens where it was left after a reload.
 */
export function usePanZoom(minZoom: number, maxZoom: number, persistKey?: string) {
  const surface = useRef<HTMLDivElement>(null);
  const [view, setView] = useState<MapView>(() => readView(persistKey, (zoom) => Math.min(maxZoom, Math.max(minZoom, zoom))));
  // the view the wheel and pointer handlers read between renders
  const viewRef = useRef(view);
  viewRef.current = view;
  const pan = useRef<{ x: number; y: number; startX: number; startY: number } | null>(null);
  const [panning, setPanning] = useState(false);
  // Saved a moment after the view stops changing, not on every pointer move.
  useEffect(() => {
    if (!persistKey) return;
    const timer = window.setTimeout(() => {
      try {
        window.localStorage.setItem(storageKey(persistKey), JSON.stringify(view));
      } catch {
        // storage unavailable: the view just is not remembered
      }
    }, 250);
    return () => window.clearTimeout(timer);
  }, [persistKey, view]);
  const clamp = useCallback((zoom: number) => Math.min(maxZoom, Math.max(minZoom, zoom)), [minZoom, maxZoom]);

  /** Zooms to `next`, keeping the surface point (px, py) where it is. */
  const zoomAround = useCallback((next: number, px: number, py: number) => {
    const previous = viewRef.current;
    const zoom = clamp(next);
    if (zoom === previous.zoom) return;
    const ratio = zoom / previous.zoom;
    const nextView = { zoom, x: px - (px - previous.x) * ratio, y: py - (py - previous.y) * ratio };
    viewRef.current = nextView;
    setView(nextView);
  }, [clamp]);

  // A native listener, not React's: React's wheel handler is passive and
  // cannot keep the page from scrolling.
  useEffect(() => {
    const element = surface.current;
    if (!element) return;
    const onWheel = (event: WheelEvent) => {
      event.preventDefault();
      const bounds = element.getBoundingClientRect();
      // a pinch sends small deltas, a mouse wheel larger ones
      const factor = Math.exp(-event.deltaY * (event.ctrlKey ? 0.01 : 0.0015));
      zoomAround(viewRef.current.zoom * factor, event.clientX - bounds.left, event.clientY - bounds.top);
    };
    element.addEventListener("wheel", onWheel, { passive: false });
    return () => element.removeEventListener("wheel", onWheel);
  }, [zoomAround]);

  /** The zoom buttons: a value or an update, around the centre of the surface. */
  const setZoom = useCallback((next: number | ((zoom: number) => number)) => {
    const zoom = typeof next === "function" ? next(viewRef.current.zoom) : next;
    const bounds = surface.current?.getBoundingClientRect();
    zoomAround(zoom, (bounds?.width ?? 0) / 2, (bounds?.height ?? 0) / 2);
  }, [zoomAround]);

  /** Back to the origin at zoom 1: Fit, Organize and Reset. */
  const resetView = useCallback(() => {
    viewRef.current = HOME;
    setView(HOME);
  }, []);

  /** A pointer position in map coordinates, the unit the cards are placed in. */
  const toMap = useCallback((clientX: number, clientY: number) => {
    const bounds = surface.current?.getBoundingClientRect();
    const { x, y, zoom } = viewRef.current;
    return { x: (clientX - (bounds?.left ?? 0) - x) / zoom, y: (clientY - (bounds?.top ?? 0) - y) / zoom };
  }, []);

  /** Pointer handlers for the surface; `startsPan` says whether a press on that target pans. */
  const panHandlers = (startsPan: (event: PointerEvent<HTMLDivElement>) => boolean) => ({
    onPointerDown: (event: PointerEvent<HTMLDivElement>) => {
      if (event.button !== 0 || !startsPan(event)) return;
      pan.current = { x: event.clientX, y: event.clientY, startX: viewRef.current.x, startY: viewRef.current.y };
      event.currentTarget.setPointerCapture?.(event.pointerId);
      setPanning(true);
    },
    onPointerMove: (event: PointerEvent<HTMLDivElement>) => {
      const start = pan.current;
      if (!start) return;
      const nextView = { ...viewRef.current, x: start.startX + event.clientX - start.x, y: start.startY + event.clientY - start.y };
      viewRef.current = nextView;
      setView(nextView);
    },
    onPointerUp: (event: PointerEvent<HTMLDivElement>) => {
      if (!pan.current) return;
      pan.current = null;
      setPanning(false);
      if (event.currentTarget.hasPointerCapture?.(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId);
    },
    onPointerCancel: () => { pan.current = null; setPanning(false); },
  });

  const surfaceStyle: CSSProperties = {
    backgroundSize: `${GRID * view.zoom}px ${GRID * view.zoom}px`,
    backgroundPosition: `${view.x}px ${view.y}px`,
  };
  const contentStyle: CSSProperties = {
    transform: `translate(${view.x}px, ${view.y}px) scale(${view.zoom})`,
    width: `${100 / view.zoom}%`,
    height: `${100 / view.zoom}%`,
  };

  return { surface, view, zoom: view.zoom, setZoom, resetView, toMap, panning, panHandlers, surfaceStyle, contentStyle };
}
