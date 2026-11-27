/**
 * The state the POM's "operating system" shares between the Pieces desktop
 * and the dock that floats over every screen: the apps (the plugins installed
 * here), which one is launching, and the windows any icon opens - an app's
 * settings, the right-click menu and the close confirmation. Mounted once in
 * the shell (`App.tsx`), so launching an app on the desktop bounces it on the
 * global dock, and a right click on the dock works on any screen.
 */
import { MemoryStick, Power, X } from "lucide-react";
import clsx from "clsx";
import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import { useNavigate } from "react-router-dom";
import { Confirm, Modal } from "../Confirm";
import { Button } from "../ui";
import { useI18n } from "../../i18n/provider";
import { closePlugin, openPlugin, setPluginLifecycle } from "../../lib/api";
import { pluginMenuItems, type ActivePluginUi } from "../../lib/pluginUi";
import { useProfile } from "../../lib/profile";
import { useNode, usePluginUi, usePublicPluginCatalog } from "../../lib/queries";
import type { PublicPluginCatalogEntry } from "../../lib/pluginCatalog";
import { useToast } from "../../lib/toast";
import { AppIcon, displayName, formatMemory } from "./shared";

/** How long a launched app bounces on the dock before the POM goes to its screen. */
export const LAUNCH_MS = 900;

type Lifecycle = "on_demand" | "resident" | null;

export interface PiecesState {
  plugins: ActivePluginUi[];
  nameOf: (plugin: ActivePluginUi) => string;
  entryOf: (code: string) => PublicPluginCatalogEntry | undefined;
  /** The app's screen in the POM: the first menu entry its manifest gives. */
  routeOf: (code: string) => string | null;
  engines: Set<string>;
  serving: string | null;
  servedModel: string | null;
  /** Running now, resident, or being launched. */
  isOpen: (plugin: ActivePluginUi) => boolean;
  launching: string | null;
  busy: string | null;
  /** Opens a closed app like a system: on the dock, bouncing, then its screen. */
  launch: (plugin: ActivePluginUi) => void;
  goTo: (route: string) => void;
  openSettings: (code: string) => void;
  askClose: (plugin: ActivePluginUi) => void;
  /** The right-click handler for an app's icon. */
  openMenu: (plugin: ActivePluginUi) => (event: React.MouseEvent) => void;
}

const PiecesContext = createContext<PiecesState | null>(null);

export function usePieces(): PiecesState {
  const state = useContext(PiecesContext);
  if (!state) throw new Error("usePieces needs PiecesProvider");
  return state;
}

/** The same, or `null` outside the provider (a screen mounted alone in a test). */
export function usePiecesOptional(): PiecesState | null {
  return useContext(PiecesContext);
}

export function PiecesProvider({ children }: { children: ReactNode }) {
  const { t, locale } = useI18n();
  const toast = useToast();
  const query = usePluginUi();
  const node = useNode();
  const navigate = useNavigate();
  const { profile } = useProfile();
  const catalog = usePublicPluginCatalog();
  const [confirming, setConfirming] = useState<ActivePluginUi | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [settingsCode, setSettingsCode] = useState<string | null>(null);
  const [launching, setLaunching] = useState<string | null>(null);
  const [menu, setMenu] = useState<{ code: string; x: number; y: number } | null>(null);

  const entryOf = (code: string) => catalog.data?.plugins.find((entry) => entry.code === code);
  const nameOf = (plugin: ActivePluginUi) => displayName(plugin, locale, entryOf(plugin.code));
  const routeOf = (code: string) => {
    const first = pluginMenuItems(profile).find((item) => item.code === code);
    return first ? (first.children?.[0]?.to ?? first.to) : null;
  };
  // The plugins that publish an inference service on this machine (a stopped
  // on-demand engine keeps its service, as `stopped`).
  const engines = new Set(
    (node.data?.plugin_services ?? []).filter((s) => s.kind === "inference").map((s) => s.plugin_code),
  );
  const serving = node.data?.config?.inference_service ?? null;
  const servedModel = node.data?.config?.model_label ?? null;
  const plugins = [...(query.data?.plugins ?? [])].sort((a, b) => nameOf(a).localeCompare(nameOf(b)));
  const isOpen = (plugin: ActivePluginUi) => plugin.running === true || !plugin.on_demand || launching === plugin.code;

  const act = (plugin: ActivePluginUi, start: boolean) => {
    setBusy(plugin.code);
    (start ? openPlugin(plugin.code) : closePlugin(plugin.code))
      .catch((error: unknown) => toast.fromError(t(start ? "plugins.openFailed" : "plugins.closeFailed"), error))
      .finally(() => {
        setBusy(null);
        void query.refetch();
      });
  };

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

  const changeLifecycle = (plugin: ActivePluginUi, lifecycle: Lifecycle) => {
    setBusy(plugin.code);
    setPluginLifecycle(plugin.code, lifecycle)
      .catch((error: unknown) => toast.fromError(t("backgroundServices.lifecycleFailed"), error))
      .finally(() => {
        setBusy(null);
        void query.refetch();
      });
  };

  const state: PiecesState = {
    plugins,
    nameOf,
    entryOf,
    routeOf,
    engines,
    serving,
    servedModel,
    isOpen,
    launching,
    busy,
    launch,
    goTo: (route) => navigate(route),
    openSettings: setSettingsCode,
    askClose: setConfirming,
    openMenu: (plugin) => (event) => {
      event.preventDefault();
      setMenu({ code: plugin.code, x: event.clientX, y: event.clientY });
    },
  };

  const menuPlugin = menu ? plugins.find((entry) => entry.code === menu.code) : undefined;
  const settingsPlugin = plugins.find((entry) => entry.code === settingsCode);

  return (
    <PiecesContext.Provider value={state}>
      {children}
      {menu && menuPlugin && (() => {
        const running = isOpen(menuPlugin);
        const route = routeOf(menuPlugin.code);
        const items: MenuItem[] = [
          ...(!running ? [{ key: "open", label: t("backgroundServices.open"), act: () => launch(menuPlugin) }] : []),
          ...(running && route ? [{ key: "go", label: t("backgroundServices.menuGoTo"), act: () => navigate(route) }] : []),
          { key: "settings", label: t("backgroundServices.menuSettings"), act: () => setSettingsCode(menuPlugin.code) },
          menuPlugin.on_demand
            ? { key: "resident", label: t("backgroundServices.menuKeepOn"), act: () => changeLifecycle(menuPlugin, menuPlugin.lifecycle_default === "resident" ? null : "resident") }
            : { key: "on_demand", label: t("backgroundServices.menuOnDemand"), act: () => changeLifecycle(menuPlugin, menuPlugin.lifecycle_default === "on_demand" ? null : "on_demand") },
          ...(running && menuPlugin.on_demand ? [{ key: "close", label: t("backgroundServices.close"), act: () => setConfirming(menuPlugin), danger: true }] : []),
        ];
        return <ContextMenu x={menu.x} y={menu.y} title={nameOf(menuPlugin)} items={items} onClose={() => setMenu(null)} />;
      })()}
      {settingsPlugin && (
        <AppSettings
          plugin={settingsPlugin}
          name={nameOf(settingsPlugin)}
          icon={<AppIcon plugin={settingsPlugin} engine={engines.has(settingsPlugin.code)} dim={false} storeIcon={entryOf(settingsPlugin.code)?.icon_url} />}
          memory={isOpen(settingsPlugin) && settingsPlugin.memory_bytes != null ? formatMemory(settingsPlugin.memory_bytes) : null}
          busy={busy === settingsPlugin.code}
          onClose={() => setSettingsCode(null)}
          onOpen={() => {
            setSettingsCode(null);
            launch(settingsPlugin);
          }}
          onStop={() => setConfirming(settingsPlugin)}
          onLifecycle={(lifecycle) => changeLifecycle(settingsPlugin, lifecycle)}
        />
      )}
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
    </PiecesContext.Provider>
  );
}

type MenuItem = { key: string; label: string; act: () => void; danger?: boolean };

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
  items: MenuItem[];
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
