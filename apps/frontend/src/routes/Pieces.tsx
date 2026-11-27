/**
 * Pieces: the POM's "operating system", a main menu entry below Plugins, apart
 * from the operational Dashboard. A dark night sky with the POM pieces locking
 * together behind it (`PuzzleBackdrop`), the apps installed here that are
 * closed as free icons the operator drags anywhere (kept in this browser), and
 * the running apps with no screen of their own (inference engines) as system
 * notifications at the top right. The open apps are on the dock, which floats
 * over every screen (`components/pieces/Dock.tsx`). A click on a closed app
 * launches it (it bounces on the dock, then its screen opens); a hover gear
 * opens its settings; a right click opens the system menu.
 */
import { Layers, Settings2, X } from "lucide-react";
import clsx from "clsx";
import { useRef, useState } from "react";
import { Empty } from "../components/ui";
import { PuzzleBackdrop } from "../components/PuzzleBackdrop";
import { AppIcon } from "../components/pieces/shared";
import { usePieces } from "../components/pieces/PiecesProvider";
import { useI18n } from "../i18n/provider";
import { storageKey } from "../lib/brand";
import type { ActivePluginUi } from "../lib/pluginUi";

type DeskPosition = { x: number; y: number };
const TILE_W = 96;
const TILE_H = 96;
/** Room the dock takes at the bottom: a desktop icon is not dropped under it. */
const DOCK_SPACE = 80;
const POSITIONS_KEY = storageKey("services.desktop");

/** The desktop's first layout: columns from the top-left, like a desktop. */
function defaultPosition(index: number): DeskPosition {
  const rows = 4;
  return { x: 24 + Math.floor(index / rows) * 112, y: 24 + (index % rows) * 116 };
}

function readPositions(): Record<string, DeskPosition> {
  try {
    const raw = window.localStorage.getItem(POSITIONS_KEY);
    const parsed = raw ? (JSON.parse(raw) as Record<string, DeskPosition>) : {};
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch {
    return {};
  }
}

function writePositions(positions: Record<string, DeskPosition>) {
  try {
    window.localStorage.setItem(POSITIONS_KEY, JSON.stringify(positions));
  } catch {
    // No storage (private window): the layout lasts until the page reloads.
  }
}

export function Pieces() {
  const { t } = useI18n();
  const pieces = usePieces();
  const { plugins, isOpen, routeOf, launching, busy, nameOf, entryOf, engines, serving, servedModel } = pieces;
  const stopped = plugins.filter((plugin) => !isOpen(plugin));
  const headless = plugins.filter((plugin) => isOpen(plugin) && !routeOf(plugin.code) && launching !== plugin.code);

  const [positions, setPositions] = useState<Record<string, DeskPosition>>(readPositions);
  const desk = useRef<HTMLDivElement>(null);
  const drag = useRef<{ code: string; dx: number; dy: number; sx: number; sy: number; moved: boolean } | null>(null);
  const justDragged = useRef(false);
  const [dragging, setDragging] = useState<string | null>(null);

  const deskIcon = (plugin: ActivePluginUi, index: number) => {
    const name = nameOf(plugin);
    const pos = positions[plugin.code] ?? defaultPosition(index);
    const onPointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
      if (event.button !== 0 || (event.target as HTMLElement).closest("[data-no-drag]")) return;
      const box = desk.current?.getBoundingClientRect();
      if (!box) return;
      // No pointer capture yet: a capture now would send the click to this
      // wrapper instead of the icon's button, and a plain click would be lost.
      drag.current = { code: plugin.code, dx: event.clientX - box.left - pos.x, dy: event.clientY - box.top - pos.y, sx: event.clientX, sy: event.clientY, moved: false };
    };
    const onPointerMove = (event: React.PointerEvent<HTMLDivElement>) => {
      const current = drag.current;
      const box = desk.current?.getBoundingClientRect();
      if (!current || current.code !== plugin.code || !box) return;
      if (!current.moved && Math.hypot(event.clientX - current.sx, event.clientY - current.sy) < 5) return;
      if (!current.moved) event.currentTarget.setPointerCapture?.(event.pointerId);
      current.moved = true;
      setDragging(plugin.code);
      const x = Math.min(Math.max(0, event.clientX - box.left - current.dx), Math.max(0, box.width - TILE_W));
      const y = Math.min(Math.max(0, event.clientY - box.top - current.dy), Math.max(0, box.height - TILE_H - DOCK_SPACE));
      setPositions((all) => ({ ...all, [plugin.code]: { x, y } }));
    };
    const onPointerUp = () => {
      const current = drag.current;
      drag.current = null;
      setDragging(null);
      if (current?.moved) {
        justDragged.current = true;
        setPositions((all) => {
          writePositions(all);
          return all;
        });
      }
    };
    return (
      <div
        key={plugin.code}
        data-testid={`background-service-${plugin.code}`}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerUp}
        onContextMenu={pieces.openMenu(plugin)}
        style={{ left: pos.x, top: pos.y, width: TILE_W, touchAction: "none" }}
        className={clsx("absolute flex select-none flex-col items-center gap-1.5 text-center", dragging === plugin.code ? "z-20 cursor-grabbing" : "z-10 cursor-grab")}
      >
        <span className="group relative">
          <button
            type="button"
            disabled={busy === plugin.code}
            draggable={false}
            onDragStart={(event) => event.preventDefault()}
            onClick={() => {
              if (justDragged.current) {
                justDragged.current = false;
                return;
              }
              pieces.launch(plugin);
            }}
            aria-label={t("backgroundServices.openNamed", { name })}
            title={name}
            className={clsx(
              "relative rounded-[1.1rem] transition-transform duration-200 ease-out focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-aqua/70",
              dragging === plugin.code ? "scale-110" : "hover:-translate-y-1.5 hover:scale-110",
              busy === plugin.code && "animate-pulse",
            )}
          >
            <AppIcon plugin={plugin} engine={engines.has(plugin.code)} dim storeIcon={entryOf(plugin.code)?.icon_url} />
          </button>
          <button
            type="button"
            data-no-drag
            onClick={() => pieces.openSettings(plugin.code)}
            aria-label={t("backgroundServices.settingsNamed", { name })}
            title={t("backgroundServices.settings")}
            className="absolute -right-1.5 -top-1.5 flex h-5 w-5 items-center justify-center rounded-full border border-white/20 bg-black/70 text-white/80 opacity-0 shadow transition-opacity hover:text-white focus-visible:opacity-100 group-hover:opacity-100"
          >
            <Settings2 size={11} />
          </button>
        </span>
        <span className="w-full truncate text-[0.72rem] font-medium text-white [text-shadow:0_1px_3px_rgb(0_0_0/0.9)]">{name}</span>
      </div>
    );
  };

  return (
    <div data-testid="services-desktop" className="relative h-full min-h-[34rem] overflow-hidden bg-[#06040f]">
      {/* The night sky and the POM pieces, behind everything. */}
      <div className="pointer-events-none absolute inset-0 bg-[radial-gradient(120%_90%_at_50%_10%,#1a1035_0%,#0a0718_55%,#05030c_100%)]" />
      <PuzzleBackdrop />
      {plugins.length === 0 ? (
        <div className="relative z-10 flex h-full items-center justify-center">
          <Empty icon={<Layers size={18} />} title={t("backgroundServices.empty")} />
        </div>
      ) : (
        <>
          <section ref={desk} aria-label={t("backgroundServices.stoppedList")} className="absolute inset-0">
            {stopped.map(deskIcon)}
          </section>
          {/* Running apps with no screen of their own (inference engines), as
              system notifications at the top right, one under the other. */}
          {headless.length > 0 && (
            <ul data-testid="services-notifications" className="absolute right-3 top-3 z-30 flex w-72 max-w-[calc(100%-1.5rem)] flex-col gap-2">
              {headless.map((plugin) => (
                <li
                  key={plugin.code}
                  data-testid={`background-service-${plugin.code}`}
                  onContextMenu={pieces.openMenu(plugin)}
                  className="flex items-center gap-2.5 rounded-xl border border-white/15 bg-black/55 p-2.5 text-white shadow-[0_10px_30px_rgb(0_0_0/0.45)] backdrop-blur-xl"
                >
                  <AppIcon plugin={plugin} engine={engines.has(plugin.code)} dim={false} small storeIcon={entryOf(plugin.code)?.icon_url} />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-sm font-semibold">{nameOf(plugin)}</div>
                    <div className="truncate text-xs text-white/60">
                      {serving === plugin.code && servedModel
                        ? t("backgroundServices.serving", { model: servedModel })
                        : t("backgroundServices.runningNow")}
                    </div>
                  </div>
                  <button
                    type="button"
                    onClick={() => pieces.openSettings(plugin.code)}
                    aria-label={t("backgroundServices.settingsNamed", { name: nameOf(plugin) })}
                    title={t("backgroundServices.settings")}
                    className="flex h-7 w-7 items-center justify-center rounded-md text-white/70 hover:bg-white/10 hover:text-white"
                  >
                    <Settings2 size={14} />
                  </button>
                  {plugin.on_demand && (
                    <button
                      type="button"
                      disabled={busy === plugin.code}
                      onClick={() => pieces.askClose(plugin)}
                      aria-label={t("backgroundServices.closeNamed", { name: nameOf(plugin) })}
                      title={t("backgroundServices.close")}
                      className="flex h-7 w-7 items-center justify-center rounded-md text-white/70 hover:bg-white/10 hover:text-rose"
                    >
                      <X size={14} />
                    </button>
                  )}
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </div>
  );
}
