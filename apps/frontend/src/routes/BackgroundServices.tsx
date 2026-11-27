import { Blocks, Cpu, Layers, MemoryStick, Power, X } from "lucide-react";
import clsx from "clsx";
import { useRef, useState } from "react";
import { Confirm, Modal } from "../components/Confirm";
import { Button, Empty } from "../components/ui";
import { useI18n } from "../i18n/provider";
import { closePlugin, openPlugin, setPluginLifecycle } from "../lib/api";
import { pluginAssetUrl, pluginDisplayName, type ActivePluginUi } from "../lib/pluginUi";
import { useNode, usePluginUi, usePublicPluginCatalog } from "../lib/queries";
import type { PublicPluginCatalogEntry } from "../lib/pluginCatalog";
import { useToast } from "../lib/toast";
import { storageKey } from "../lib/brand";
import { PuzzleBackdrop } from "../components/PuzzleBackdrop";

type DeskPosition = { x: number; y: number };
const TILE_W = 96;
const TILE_H = 96;
/** Room the dock takes at the bottom: a desktop icon is not dropped under it. */
const DOCK_SPACE = 120;
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
function AppIcon({ plugin, engine, dim, storeIcon }: { plugin: ActivePluginUi; engine: boolean; dim: boolean; storeIcon?: string | null }) {
  const [broken, setBroken] = useState(false);
  const own = plugin.manifest.app_icon ?? plugin.manifest.icon_image;
  const image = storeIcon ?? (own ? pluginAssetUrl(plugin.code, own) : null);
  return (
    <span
      className={clsx(
        "relative flex h-16 w-16 items-center justify-center overflow-hidden rounded-[1.1rem] shadow-[0_6px_16px_rgb(0_0_0/0.25)] ring-1 ring-white/10",
        image && !broken ? "bg-surface" : "bg-gradient-to-br from-iris/80 to-aqua/70 text-white",
        dim && "opacity-80 saturate-50",
      )}
    >
      {image && !broken ? (
        <img src={image} alt="" className="h-full w-full object-cover" onError={() => setBroken(true)} />
      ) : engine ? (
        <Cpu size={28} />
      ) : (
        <Blocks size={28} />
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
  // A resident plugin runs while it is installed and active.
  const isOpen = (plugin: ActivePluginUi) => plugin.running || !plugin.on_demand;
  const open = plugins.filter(isOpen);
  // Like the Dock: the apps that cannot be closed are pinned at the start, the
  // ones that can follow after a divider.
  const pinned = open.filter((plugin) => !plugin.on_demand);
  const closable = open.filter((plugin) => plugin.on_demand);
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

  // Where the operator put each available app on the desktop, in this browser.
  const [positions, setPositions] = useState<Record<string, DeskPosition>>(readPositions);
  const desk = useRef<HTMLDivElement>(null);
  const drag = useRef<{ code: string; dx: number; dy: number; sx: number; sy: number; moved: boolean } | null>(null);
  const justDragged = useRef(false);
  const [dragging, setDragging] = useState<string | null>(null);

  const iconButton = (plugin: ActivePluginUi, running: boolean) => {
    const name = nameOf(plugin);
    return (
      <button
        type="button"
        disabled={busy === plugin.code}
        onClick={() => {
          if (justDragged.current) {
            justDragged.current = false;
            return;
          }
          setSettingsCode(plugin.code);
        }}
        aria-label={t("backgroundServices.settingsNamed", { name })}
        title={name}
        className={clsx(
          "relative rounded-[1.1rem] transition-transform duration-200 ease-out focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-aqua/70",
          dragging === plugin.code ? "scale-110" : "hover:-translate-y-1.5 hover:scale-110",
          busy === plugin.code && "animate-pulse",
        )}
      >
        <AppIcon plugin={plugin} engine={engines.has(plugin.code)} dim={!running} storeIcon={entryOf(plugin.code)?.icon_url} />
      </button>
    );
  };

  const label = (name: string) => (
    <span className="w-full truncate text-[0.72rem] font-medium text-white [text-shadow:0_1px_3px_rgb(0_0_0/0.9)]">{name}</span>
  );

  /** An open app on the dock: icon, name and the running light. */
  const dockTile = (plugin: ActivePluginUi) => {
    const name = nameOf(plugin);
    return (
      <li key={plugin.code} data-testid={`background-service-${plugin.code}`} className="group relative flex w-20 flex-col items-center gap-1 text-center">
        {iconButton(plugin, true)}
        {plugin.on_demand && (
          <button
            type="button"
            disabled={busy === plugin.code}
            onClick={() => setConfirming(plugin)}
            aria-label={t("backgroundServices.closeNamed", { name })}
            title={t("backgroundServices.close")}
            className="absolute -left-1 -top-1 flex h-5 w-5 items-center justify-center rounded-full border border-white/20 bg-black/70 text-white/80 opacity-0 shadow transition-opacity hover:text-rose focus-visible:opacity-100 group-hover:opacity-100"
          >
            <X size={12} />
          </button>
        )}
        {label(name)}
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
        style={{ left: pos.x, top: pos.y, width: TILE_W, touchAction: "none" }}
        className={clsx("absolute flex select-none flex-col items-center gap-1.5 text-center", dragging === plugin.code ? "z-20 cursor-grabbing" : "z-10 cursor-grab")}
      >
        {iconButton(plugin, false)}
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
          {/* The open apps on a dock over the animation, as on macOS: the ones
              that cannot be closed pinned first, then a divider. */}
          <section
            aria-label={t("backgroundServices.running", { count: open.length })}
            className="pointer-events-none absolute inset-x-0 bottom-4 z-30 flex justify-center px-4"
          >
            <ul
              data-testid="services-dock"
              className="pointer-events-auto flex max-w-full flex-wrap items-start justify-center gap-x-2.5 gap-y-3 rounded-[1.75rem] border border-white/15 bg-white/[0.07] px-4 pb-2 pt-3 shadow-[0_18px_50px_rgb(0_0_0/0.5)] backdrop-blur-2xl"
            >
              {open.length === 0 && <li className="px-6 py-4 text-sm text-white/60">{t("backgroundServices.noneOpen")}</li>}
              {pinned.map(dockTile)}
              {pinned.length > 0 && closable.length > 0 && (
                <li aria-hidden="true" data-testid="dock-divider" className="mx-1 w-px self-stretch bg-white/20" />
              )}
              {closable.map(dockTile)}
            </ul>
          </section>
        </>
      )}
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
            onOpen={() => act(plugin, true)}
            onStop={() => setConfirming(plugin)}
            onLifecycle={(lifecycle) => {
              setBusy(plugin.code);
              setPluginLifecycle(plugin.code, lifecycle)
                .catch((error: unknown) => toast.fromError(t("backgroundServices.lifecycleFailed"), error))
                .finally(() => {
                  setBusy(null);
                  void query.refetch();
                });
            }}
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
