import { Blocks, Cpu, Layers, MemoryStick, Power, Settings2, X } from "lucide-react";
import { useNavigate } from "react-router-dom";
import clsx from "clsx";
import { useEffect, useRef, useState } from "react";
import { Confirm, Modal } from "../components/Confirm";
import { Button, Empty } from "../components/ui";
import { useI18n } from "../i18n/provider";
import { closePlugin, openPlugin, setPluginLifecycle } from "../lib/api";
import { pluginAssetUrl, pluginDisplayName, pluginMenuItems, type ActivePluginUi } from "../lib/pluginUi";
import { useProfile } from "../lib/profile";
import { useNode, usePluginUi, usePublicPluginCatalog } from "../lib/queries";
import type { PublicPluginCatalogEntry } from "../lib/pluginCatalog";
import { useToast } from "../lib/toast";
import { storageKey } from "../lib/brand";
import { PuzzleBackdrop } from "../components/PuzzleBackdrop";

type DeskPosition = { x: number; y: number };
const TILE_W = 96;
const TILE_H = 96;
/** Room the dock takes at the bottom: a desktop icon is not dropped under it. */
const DOCK_SPACE = 80;
/** How long a launched app bounces on the dock before the POM goes to its screen. */
const LAUNCH_MS = 900;
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

/** `314572800` -> `300 MB`; one decimal above 1 GB. */
export function formatMemory(bytes: number): string {
  const mb = bytes / (1024 * 1024);
  if (mb < 1024) return `${Math.round(mb)} MB`;
  return `${(mb / 1024).toFixed(1)} GB`;
}

/**
 * The app's name: the product name the store lists (the logs plugin is the
 * Business Pack, whatever its first menu entry says), else its menu label,
 * else its code, capitalized.
 */
function displayName(plugin: ActivePluginUi, locale: string, entry?: PublicPluginCatalogEntry): string {
  if (entry?.display_name) return entry.display_name;
  const name = pluginDisplayName(plugin.code, locale);
  return name === plugin.code ? plugin.code.charAt(0).toUpperCase() + plugin.code.slice(1) : name;
}

/** The app icon: the store's icon, else the manifest's `icon_image`, else a glyph on a tile. */
function AppIcon({ plugin, engine, dim, storeIcon, small = false }: { plugin: ActivePluginUi; engine: boolean; dim: boolean; storeIcon?: string | null; small?: boolean }) {
  const [broken, setBroken] = useState(false);
  const own = plugin.manifest.app_icon ?? plugin.manifest.icon_image;
  const image = storeIcon ?? (own ? pluginAssetUrl(plugin.code, own) : null);
  return (
    <span
      className={clsx(
        "relative flex items-center justify-center overflow-hidden shadow-[0_6px_16px_rgb(0_0_0/0.25)] ring-1 ring-white/10",
        small ? "h-10 w-10 rounded-[0.7rem]" : "h-16 w-16 rounded-[1.1rem]",
        image && !broken ? "bg-surface" : "bg-gradient-to-br from-iris/80 to-aqua/70 text-white",
        dim && "opacity-80 saturate-50",
      )}
    >
      {image && !broken ? (
        <img src={image} alt="" draggable={false} className="pointer-events-none h-full w-full object-cover" onError={() => setBroken(true)} />
      ) : engine ? (
        <Cpu size={small ? 18 : 28} />
      ) : (
        <Blocks size={small ? 18 : 28} />
      )}
    </span>
  );
}

/**
 * Settings > Services: every plugin of this machine as an app, the open ones
 * on a dock-like shelf and the stopped ones in a folder below, each icon with
 * its name underneath. An on-demand app closes from its X (with a
 * confirmation) and opens with a click; a resident one is always on.
 * Inference engines (vLLM, Avalon) are on demand and have no menu: this is
 * where they are started and stopped. Stopping the engine this machine
 * serves its model with releases that model (the node does it); a deployment
 * starts a stopped engine by itself.
 */
export function BackgroundServices({ embedded = false }: { embedded?: boolean } = {}) {
  const { t, locale } = useI18n();
  const toast = useToast();
  const query = usePluginUi();
  const node = useNode();
  const navigate = useNavigate();
  const { profile } = useProfile();
  // The app's screen in the POM: the first menu entry its manifest gives.
  const routeOf = (code: string) => {
    const items = pluginMenuItems(profile).filter((item) => item.code === code);
    const first = items[0];
    return first ? (first.children?.[0]?.to ?? first.to) : null;
  };
  // The store names and draws the plugins it sells (as the Plugins screen does).
  const catalog = usePublicPluginCatalog();
  const entryOf = (code: string) => catalog.data?.plugins.find((entry) => entry.code === code);
  const nameOf = (plugin: ActivePluginUi) => displayName(plugin, locale, entryOf(plugin.code));
  const [confirming, setConfirming] = useState<ActivePluginUi | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  // The app whose settings window is open (a click on its icon).
  const [settingsCode, setSettingsCode] = useState<string | null>(null);
  // The plugins that publish an inference service on this machine (a stopped
  // on-demand engine keeps its service, as `stopped`).
  const engines = new Set(
    (node.data?.plugin_services ?? []).filter((s) => s.kind === "inference").map((s) => s.plugin_code),
  );
  const serving = node.data?.config?.inference_service ?? null;
  const servedModel = node.data?.config?.model_label ?? null;
  const plugins = [...(query.data?.plugins ?? [])].sort((a, b) => nameOf(a).localeCompare(nameOf(b)));
  // The app being launched: on the dock at once, bouncing, until it runs.
  const [launching, setLaunching] = useState<string | null>(null);
  // The right-click menu: which app, and where.
  const [menu, setMenu] = useState<{ code: string; x: number; y: number } | null>(null);
  // A resident plugin runs while it is installed and active.
  const isOpen = (plugin: ActivePluginUi) => plugin.running === true || !plugin.on_demand || launching === plugin.code;
  const open = plugins.filter(isOpen);
  // Apps with a screen go on the dock; the ones without (an inference engine)
  // are system notifications at the top right while they run.
  const docked = open.filter((plugin) => routeOf(plugin.code) || launching === plugin.code);
  const headless = open.filter((plugin) => !routeOf(plugin.code) && launching !== plugin.code);
  // Like the Dock: the apps that cannot be closed are pinned at the start, the
  // ones that can follow after a divider.
  const pinned = docked.filter((plugin) => !plugin.on_demand);
  const closable = docked.filter((plugin) => plugin.on_demand);
  const stopped = plugins.filter((plugin) => !isOpen(plugin));

  const act = (plugin: ActivePluginUi, start: boolean) => {
    setBusy(plugin.code);
    (start ? openPlugin(plugin.code) : closePlugin(plugin.code))
      .catch((error: unknown) => toast.fromError(t(start ? "plugins.openFailed" : "plugins.closeFailed"), error))
      .finally(() => {
        setBusy(null);
        void query.refetch();
      });
  };

  /**
   * Opens a closed app like an operating system: it lands on the dock and
   * bounces while it starts, then the POM goes to its screen (an app with no
   * screen just keeps running, as a notification).
   */
  const launch = (plugin: ActivePluginUi) => {
    const route = routeOf(plugin.code);
    setLaunching(plugin.code);
    openPlugin(plugin.code)
      .then(() => query.refetch())
      .then(() => {
        window.setTimeout(() => {
          setLaunching(null);
          if (route) navigate(route);
        }, LAUNCH_MS);
      })
      .catch((error: unknown) => {
        setLaunching(null);
        toast.fromError(t("plugins.openFailed"), error);
      });
  };

  const changeLifecycle = (plugin: ActivePluginUi, lifecycle: "on_demand" | "resident" | null) => {
    setBusy(plugin.code);
    setPluginLifecycle(plugin.code, lifecycle)
      .catch((error: unknown) => toast.fromError(t("backgroundServices.lifecycleFailed"), error))
      .finally(() => {
        setBusy(null);
        void query.refetch();
      });
  };

  const openMenu = (plugin: ActivePluginUi) => (event: React.MouseEvent) => {
    event.preventDefault();
    setMenu({ code: plugin.code, x: event.clientX, y: event.clientY });
  };

  // Where the operator put each available app on the desktop, in this browser.
  const [positions, setPositions] = useState<Record<string, DeskPosition>>(readPositions);
  const desk = useRef<HTMLDivElement>(null);
  const drag = useRef<{ code: string; dx: number; dy: number; sx: number; sy: number; moved: boolean } | null>(null);
  const justDragged = useRef(false);
  const [dragging, setDragging] = useState<string | null>(null);

  /**
   * The app's icon. On the dock (open) a click goes to its screen in the POM,
   * or to its settings when it has none; on the desktop (closed) a click
   * opens it, and it moves to the dock.
   */
  const iconButton = (plugin: ActivePluginUi, running: boolean) => {
    const name = nameOf(plugin);
    const route = routeOf(plugin.code);
    return (
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
          if (!running) launch(plugin);
          else if (route) navigate(route);
          else setSettingsCode(plugin.code);
        }}
        aria-label={running ? (route ? t("backgroundServices.goTo", { name }) : t("backgroundServices.settingsNamed", { name })) : t("backgroundServices.openNamed", { name })}
        title={name}
        className={clsx(
          "relative transition-transform duration-200 ease-out focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-aqua/70",
          running ? "rounded-[0.7rem]" : "rounded-[1.1rem]",
          dragging === plugin.code ? "scale-110" : running ? "hover:-translate-y-1 hover:scale-125" : "hover:-translate-y-1.5 hover:scale-110",
          busy === plugin.code && "animate-pulse",
          launching === plugin.code && "animate-bounce",
        )}
      >
        <AppIcon plugin={plugin} engine={engines.has(plugin.code)} dim={!running} small={running} storeIcon={entryOf(plugin.code)?.icon_url} />
      </button>
    );
  };

  /** The settings gear, shown on hover. */
  const gear = (plugin: ActivePluginUi, corner: string) => (
    <button
      type="button"
      data-no-drag
      onClick={() => setSettingsCode(plugin.code)}
      aria-label={t("backgroundServices.settingsNamed", { name: nameOf(plugin) })}
      title={t("backgroundServices.settings")}
      className={clsx(
        "absolute flex h-5 w-5 items-center justify-center rounded-full border border-white/20 bg-black/70 text-white/80 opacity-0 shadow transition-opacity hover:text-white focus-visible:opacity-100 group-hover:opacity-100",
        corner,
      )}
    >
      <Settings2 size={11} />
    </button>
  );

  const label = (name: string) => (
    <span className="w-full truncate text-[0.72rem] font-medium text-white [text-shadow:0_1px_3px_rgb(0_0_0/0.9)]">{name}</span>
  );

  /** An open app on the dock: icon, name and the running light. */
  const dockTile = (plugin: ActivePluginUi) => {
    const name = nameOf(plugin);
    return (
      <li key={plugin.code} data-testid={`background-service-${plugin.code}`} onContextMenu={openMenu(plugin)} className="group relative flex flex-col items-center gap-0.5">
        {iconButton(plugin, true)}
        {/* An app with no screen opens its settings from the icon itself. */}
        {routeOf(plugin.code) && gear(plugin, "-right-1.5 -top-1.5")}
        {plugin.on_demand && (
          <button
            type="button"
            disabled={busy === plugin.code}
            onClick={() => setConfirming(plugin)}
            aria-label={t("backgroundServices.closeNamed", { name })}
            title={t("backgroundServices.close")}
            className="absolute -left-1.5 -top-1.5 flex h-5 w-5 items-center justify-center rounded-full border border-white/20 bg-black/70 text-white/80 opacity-0 shadow transition-opacity hover:text-rose focus-visible:opacity-100 group-hover:opacity-100"
          >
            <X size={12} />
          </button>
        )}
        <span className="sr-only">{name}</span>
        <span className="h-1 w-1 rounded-full bg-white/80" aria-hidden="true" />
      </li>
    );
  };

  /** An available app on the desktop, where the operator drags it. */
  const deskIcon = (plugin: ActivePluginUi, index: number) => {
    const pos = positions[plugin.code] ?? defaultPosition(index);
    const onPointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
      if (event.button !== 0 || (event.target as HTMLElement).closest("[data-no-drag]")) return;
      const box = desk.current?.getBoundingClientRect();
      if (!box) return;
      drag.current = { code: plugin.code, dx: event.clientX - box.left - pos.x, dy: event.clientY - box.top - pos.y, sx: event.clientX, sy: event.clientY, moved: false };
      event.currentTarget.setPointerCapture?.(event.pointerId);
    };
    const onPointerMove = (event: React.PointerEvent<HTMLDivElement>) => {
      const current = drag.current;
      const box = desk.current?.getBoundingClientRect();
      if (!current || current.code !== plugin.code || !box) return;
      if (!current.moved && Math.hypot(event.clientX - current.sx, event.clientY - current.sy) < 5) return;
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
        onContextMenu={openMenu(plugin)}
        style={{ left: pos.x, top: pos.y, width: TILE_W, touchAction: "none" }}
        className={clsx("absolute flex select-none flex-col items-center gap-1.5 text-center", dragging === plugin.code ? "z-20 cursor-grabbing" : "z-10 cursor-grab")}
      >
        <span className="group relative">
          {iconButton(plugin, false)}
          {gear(plugin, "-right-1.5 -top-1.5")}
        </span>
        {label(nameOf(plugin))}
      </div>
    );
  };

  return (
    <div
      data-testid="services-desktop"
      className={clsx("relative overflow-hidden bg-[#06040f]", embedded ? "h-full min-h-[34rem]" : "h-[calc(100dvh-8.5rem)] rounded-2xl")}
    >
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
                  onContextMenu={openMenu(plugin)}
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
                    onClick={() => setSettingsCode(plugin.code)}
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
                      onClick={() => setConfirming(plugin)}
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
          {/* The open apps on a dock over the animation, as on macOS: the ones
              that cannot be closed pinned first, then a divider. */}
          <section
            aria-label={t("backgroundServices.running", { count: open.length })}
            className="pointer-events-none absolute inset-x-0 bottom-3 z-30 flex justify-center px-4"
          >
            <ul
              data-testid="services-dock"
              className="pointer-events-auto flex max-w-full flex-wrap items-end justify-center gap-x-2 gap-y-2 rounded-2xl border border-white/15 bg-white/[0.07] px-2.5 pb-1 pt-2 shadow-[0_18px_50px_rgb(0_0_0/0.5)] backdrop-blur-2xl"
            >
              {open.length === 0 && <li className="px-4 py-2 text-xs text-white/60">{t("backgroundServices.noneOpen")}</li>}
              {pinned.map(dockTile)}
              {pinned.length > 0 && closable.length > 0 && (
                <li aria-hidden="true" data-testid="dock-divider" className="mx-0.5 mb-1.5 h-9 w-px self-end bg-white/20" />
              )}
              {closable.map(dockTile)}
            </ul>
          </section>
        </>
      )}
      {(() => {
        const plugin = menu ? plugins.find((entry) => entry.code === menu.code) : undefined;
        if (!menu || !plugin) return null;
        const running = isOpen(plugin);
        const route = routeOf(plugin.code);
        const items: { key: string; label: string; act: () => void; danger?: boolean }[] = [
          ...(!running ? [{ key: "open", label: t("backgroundServices.open"), act: () => launch(plugin) }] : []),
          ...(running && route ? [{ key: "go", label: t("backgroundServices.menuGoTo"), act: () => navigate(route) }] : []),
          { key: "settings", label: t("backgroundServices.menuSettings"), act: () => setSettingsCode(plugin.code) },
          plugin.on_demand
            ? { key: "resident", label: t("backgroundServices.menuKeepOn"), act: () => changeLifecycle(plugin, plugin.lifecycle_default === "resident" ? null : "resident") }
            : { key: "on_demand", label: t("backgroundServices.menuOnDemand"), act: () => changeLifecycle(plugin, plugin.lifecycle_default === "on_demand" ? null : "on_demand") },
          ...(running && plugin.on_demand ? [{ key: "close", label: t("backgroundServices.close"), act: () => setConfirming(plugin), danger: true }] : []),
        ];
        return <ContextMenu x={menu.x} y={menu.y} title={nameOf(plugin)} items={items} onClose={() => setMenu(null)} />;
      })()}
      {(() => {
        const plugin = plugins.find((entry) => entry.code === settingsCode);
        if (!plugin) return null;
        return (
          <AppSettings
            plugin={plugin}
            name={nameOf(plugin)}
            icon={<AppIcon plugin={plugin} engine={engines.has(plugin.code)} dim={false} storeIcon={entryOf(plugin.code)?.icon_url} />}
            memory={isOpen(plugin) && plugin.memory_bytes != null ? formatMemory(plugin.memory_bytes) : null}
            busy={busy === plugin.code}
            onClose={() => setSettingsCode(null)}
            onOpen={() => { setSettingsCode(null); launch(plugin); }}
            onStop={() => setConfirming(plugin)}
            onLifecycle={(lifecycle) => changeLifecycle(plugin, lifecycle)}
          />
        );
      })()}
      <Confirm
        open={confirming !== null}
        title={t("backgroundServices.confirmTitle", { name: confirming ? nameOf(confirming) : "" })}
        description={
          confirming && serving === confirming.code
            ? t("backgroundServices.confirmEngineBody", { model: servedModel ?? "" })
            : t("backgroundServices.confirmBody")
        }
        confirmLabel={t("backgroundServices.close")}
        onCancel={() => setConfirming(null)}
        onConfirm={() => {
          if (confirming) act(confirming, false);
          setConfirming(null);
        }}
      />
    </div>
  );
}

/**
 * The window a click on an app's icon opens: what it is, whether it runs
 * now, the memory it uses and how it loads on this machine - on demand
 * (started when opened or needed, closed to free its memory) or always on
 * (started with POM). The operator's choice wins over the plugin's own
 * (`POST /api/ui/plugins/:code/lifecycle`), and can be given back.
 */
function AppSettings({
  plugin,
  name,
  icon,
  memory,
  busy,
  onClose,
  onOpen,
  onStop,
  onLifecycle,
}: {
  plugin: ActivePluginUi;
  name: string;
  icon: React.ReactNode;
  memory: string | null;
  busy: boolean;
  onClose: () => void;
  onOpen: () => void;
  onStop: () => void;
  onLifecycle: (lifecycle: "on_demand" | "resident" | null) => void;
}) {
  const { t } = useI18n();
  const onDemand = plugin.on_demand === true;
  const running = plugin.running === true || !onDemand;
  const fallback = plugin.lifecycle_default ?? (onDemand && !plugin.lifecycle_override ? "on_demand" : "resident");
  const choice = (value: "on_demand" | "resident", title: string, hint: string) => {
    const selected = (onDemand ? "on_demand" : "resident") === value;
    return (
      <button
        type="button"
        role="radio"
        aria-checked={selected}
        disabled={busy}
        data-testid={`lifecycle-${value}`}
        onClick={() => !selected && onLifecycle(value === fallback ? null : value)}
        className={clsx(
          "flex flex-1 flex-col items-start gap-1 rounded-lg border px-3 py-2.5 text-left transition-colors disabled:opacity-60",
          selected ? "border-aqua/60 bg-aqua/10" : "border-line hover:border-line-strong",
        )}
      >
        <span className="flex items-center gap-2 text-sm font-semibold text-fg">
          {title}
          {value === fallback && (
            <span className="rounded border border-line px-1 text-[0.6rem] font-medium uppercase tracking-wide text-faint">
              {t("backgroundServices.lifecycleDefault")}
            </span>
          )}
        </span>
        <span className="text-xs text-muted">{hint}</span>
      </button>
    );
  };
  return (
    <Modal
      open
      title={name}
      onClose={onClose}
      footer={
        <>
          {onDemand && running && (
            <Button variant="outline" icon={<X size={14} />} loading={busy} onClick={onStop}>
              {t("backgroundServices.close")}
            </Button>
          )}
          {!running && (
            <Button variant="primary" icon={<Power size={14} />} loading={busy} onClick={onOpen}>
              {t("backgroundServices.open")}
            </Button>
          )}
          <Button variant="ghost" onClick={onClose}>
            {t("backgroundServices.done")}
          </Button>
        </>
      }
    >
      <div className="space-y-4" data-testid="app-settings">
        <div className="flex items-center gap-3">
          {icon}
          <div className="min-w-0 space-y-1 text-sm">
            <div className="text-muted">
              {plugin.version ? `${t("backgroundServices.version")} ${plugin.version} · ` : ""}
              {running ? t("backgroundServices.runningNow") : t("backgroundServices.stopped")}
            </div>
            {memory && (
              <span className="inline-flex items-center gap-1 rounded-full border border-aqua/40 bg-aqua/15 px-2 py-0.5 font-mono text-xs font-semibold text-aqua">
                <MemoryStick size={12} />
                {memory}
              </span>
            )}
          </div>
        </div>
        <div role="radiogroup" aria-label={t("backgroundServices.lifecycle")} className="space-y-2">
          <p className="text-xs font-semibold uppercase tracking-[0.12em] text-faint">{t("backgroundServices.lifecycle")}</p>
          <div className="flex flex-col gap-2 sm:flex-row">
            {choice("on_demand", t("backgroundServices.lifecycleOnDemand"), t("backgroundServices.lifecycleOnDemandHint"))}
            {choice("resident", t("backgroundServices.lifecycleResident"), t("backgroundServices.lifecycleResidentHint"))}
          </div>
          {plugin.lifecycle_override && (
            <p className="text-xs text-faint">{t("backgroundServices.lifecycleChanged")}</p>
          )}
        </div>
      </div>
    </Modal>
  );
}

/**
 * A right-click menu like an operating system's: the app's name, then what can
 * be done with it. It closes on a pick, a click elsewhere or Escape, and stays
 * inside the window.
 */
function ContextMenu({
  x,
  y,
  title,
  items,
  onClose,
}: {
  x: number;
  y: number;
  title: string;
  items: { key: string; label: string; act: () => void; danger?: boolean }[];
  onClose: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const away = (event: MouseEvent) => {
      if (!ref.current?.contains(event.target as Node)) onClose();
    };
    const key = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("mousedown", away);
    document.addEventListener("keydown", key);
    ref.current?.querySelector<HTMLButtonElement>("button")?.focus();
    return () => {
      document.removeEventListener("mousedown", away);
      document.removeEventListener("keydown", key);
    };
  }, [onClose]);
  const left = Math.min(x, window.innerWidth - 216);
  const top = Math.min(y, window.innerHeight - (items.length * 34 + 44));
  return (
    <div
      ref={ref}
      role="menu"
      aria-label={title}
      data-testid="app-context-menu"
      style={{ left, top }}
      className="fixed z-50 w-52 overflow-hidden rounded-lg border border-white/15 bg-[#14111f]/95 py-1 text-sm text-white shadow-[0_16px_40px_rgb(0_0_0/0.5)] backdrop-blur-xl"
    >
      <div className="truncate px-3 pb-1 pt-1.5 text-[0.68rem] font-semibold uppercase tracking-[0.1em] text-white/50">{title}</div>
      {items.map((item) => (
        <button
          key={item.key}
          type="button"
          role="menuitem"
          onClick={() => {
            onClose();
            item.act();
          }}
          className={clsx(
            "block w-full px-3 py-1.5 text-left hover:bg-iris/80 focus-visible:bg-iris/80 focus-visible:outline-none",
            item.danger && "text-rose-300 hover:text-white",
          )}
        >
          {item.label}
        </button>
      ))}
    </div>
  );
}
