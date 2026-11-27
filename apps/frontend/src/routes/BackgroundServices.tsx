import { Blocks, Cpu, Layers, Lock, Power, X } from "lucide-react";
import clsx from "clsx";
import { useState } from "react";
import { Confirm } from "../components/Confirm";
import { Empty, PageHeader } from "../components/ui";
import { useI18n } from "../i18n/provider";
import { closePlugin, openPlugin } from "../lib/api";
import { pluginAssetUrl, pluginDisplayName, type ActivePluginUi } from "../lib/pluginUi";
import { useNode, usePluginUi } from "../lib/queries";
import { useToast } from "../lib/toast";

/** `314572800` -> `300 MB`; one decimal above 1 GB. */
export function formatMemory(bytes: number): string {
  const mb = bytes / (1024 * 1024);
  if (mb < 1024) return `${Math.round(mb)} MB`;
  return `${(mb / 1024).toFixed(1)} GB`;
}

/** A plugin with no menu has no label of its own: its code, capitalized. */
function displayName(plugin: ActivePluginUi, locale: string): string {
  const name = pluginDisplayName(plugin.code, locale);
  return name === plugin.code ? plugin.code.charAt(0).toUpperCase() + plugin.code.slice(1) : name;
}

/** The app icon: the manifest's `icon_image`, else a glyph on a tile. */
function AppIcon({ plugin, engine, dim }: { plugin: ActivePluginUi; engine: boolean; dim: boolean }) {
  const [broken, setBroken] = useState(false);
  const image = plugin.manifest.icon_image;
  return (
    <span
      className={clsx(
        "relative flex h-16 w-16 items-center justify-center overflow-hidden rounded-[1.1rem] shadow-[0_6px_16px_rgb(0_0_0/0.25)] ring-1 ring-white/10",
        image && !broken ? "bg-surface" : "bg-gradient-to-br from-iris/80 to-aqua/70 text-white",
        dim && "opacity-60 grayscale",
      )}
    >
      {image && !broken ? (
        <img src={pluginAssetUrl(plugin.code, image)} alt="" className="h-full w-full object-cover" onError={() => setBroken(true)} />
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
export function BackgroundServices() {
  const { t, locale } = useI18n();
  const toast = useToast();
  const query = usePluginUi();
  const node = useNode();
  const [confirming, setConfirming] = useState<ActivePluginUi | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  // The plugins that publish an inference service on this machine (a stopped
  // on-demand engine keeps its service, as `stopped`).
  const engines = new Set(
    (node.data?.plugin_services ?? []).filter((s) => s.kind === "inference").map((s) => s.plugin_code),
  );
  const serving = node.data?.config?.inference_service ?? null;
  const servedModel = node.data?.config?.model_label ?? null;
  const plugins = [...(query.data?.plugins ?? [])].sort((a, b) =>
    displayName(a, locale).localeCompare(displayName(b, locale)),
  );
  // A resident plugin runs while it is installed and active.
  const isOpen = (plugin: ActivePluginUi) => plugin.running || !plugin.on_demand;
  const open = plugins.filter(isOpen);
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

  const tile = (plugin: ActivePluginUi) => {
    const name = displayName(plugin, locale);
    const engine = engines.has(plugin.code);
    const running = isOpen(plugin);
    const working = busy === plugin.code;
    const details = [
      engine ? t("backgroundServices.engine") : null,
      serving === plugin.code && servedModel ? t("backgroundServices.serving", { model: servedModel }) : null,
      running && plugin.memory_bytes != null ? formatMemory(plugin.memory_bytes) : null,
      !plugin.on_demand ? t("backgroundServices.alwaysOn") : null,
    ].filter(Boolean) as string[];
    return (
      <li
        key={plugin.code}
        data-testid={`background-service-${plugin.code}`}
        className="group relative flex w-24 flex-col items-center gap-1.5 text-center"
      >
        <button
          type="button"
          disabled={working || running}
          onClick={() => act(plugin, true)}
          aria-label={running ? name : t("backgroundServices.openNamed", { name })}
          title={[name, ...details].join(" · ")}
          className={clsx(
            "relative rounded-[1.1rem] transition-transform duration-200 ease-out focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-aqua/70",
            "hover:-translate-y-1.5 hover:scale-110 disabled:cursor-default",
            working && "animate-pulse",
          )}
        >
          <AppIcon plugin={plugin} engine={engine} dim={!running} />
          {!running && (
            <span className="absolute inset-0 flex items-center justify-center rounded-[1.1rem] bg-black/35 text-white opacity-0 transition-opacity group-hover:opacity-100">
              <Power size={22} />
            </span>
          )}
          {engine && (
            <span className="absolute -bottom-1 -right-1 flex h-5 w-5 items-center justify-center rounded-full border border-line bg-surface text-iris shadow">
              <Cpu size={11} />
            </span>
          )}
        </button>
        {running && plugin.on_demand && (
          <button
            type="button"
            disabled={working}
            onClick={() => setConfirming(plugin)}
            aria-label={t("backgroundServices.closeNamed", { name })}
            title={t("backgroundServices.close")}
            className="absolute -left-0.5 -top-1 flex h-5 w-5 items-center justify-center rounded-full border border-line bg-surface text-muted opacity-0 shadow transition-opacity hover:text-rose focus-visible:opacity-100 group-hover:opacity-100"
          >
            <X size={12} />
          </button>
        )}
        {running && !plugin.on_demand && (
          <span className="absolute -left-0.5 -top-1 flex h-5 w-5 items-center justify-center rounded-full border border-line bg-surface text-faint opacity-0 shadow group-hover:opacity-100" title={t("backgroundServices.alwaysOn")}>
            <Lock size={10} />
          </span>
        )}
        <span className="w-full truncate text-[0.72rem] font-medium text-fg">{name}</span>
        {details.length > 0 && (
          <span className="-mt-1 w-full truncate text-[0.6rem] text-faint" data-testid={`background-service-detail-${plugin.code}`}>
            {serving === plugin.code && servedModel ? t("backgroundServices.serving", { model: servedModel }) : details[0]}
          </span>
        )}
        {/* The Dock's running light. */}
        <span className={clsx("h-1 w-1 rounded-full", running ? "bg-fg/70" : "bg-transparent")} aria-hidden="true" />
      </li>
    );
  };

  return (
    <div className="space-y-6">
      <PageHeader title={t("backgroundServices.title")} subtitle={t("backgroundServices.subtitle")} />
      {plugins.length === 0 ? (
        <Empty icon={<Layers size={18} />} title={t("backgroundServices.empty")} />
      ) : (
        <>
          <section aria-label={t("backgroundServices.running", { count: open.length })}>
            <h2 className="mb-2 text-xs font-semibold uppercase tracking-[0.12em] text-faint">
              {t("backgroundServices.running", { count: open.length })}
            </h2>
            <ul className="flex flex-wrap justify-center gap-x-3 gap-y-4 rounded-[1.75rem] border border-white/10 bg-surface/60 px-5 pb-3 pt-5 shadow-[0_18px_50px_rgb(0_0_0/0.18)] backdrop-blur-xl">
              {open.length === 0 ? <li className="py-6 text-sm text-faint">{t("backgroundServices.noneOpen")}</li> : open.map(tile)}
            </ul>
          </section>
          {stopped.length > 0 && (
            <section aria-label={t("backgroundServices.stoppedList")}>
              <h2 className="mb-2 text-xs font-semibold uppercase tracking-[0.12em] text-faint">
                {t("backgroundServices.stoppedList")}
              </h2>
              <ul className="grid grid-cols-[repeat(auto-fill,minmax(6rem,1fr))] justify-items-center gap-y-5 rounded-[1.75rem] border border-line bg-raised/40 px-5 py-5">
                {stopped.map(tile)}
              </ul>
            </section>
          )}
        </>
      )}
      <Confirm
        open={confirming !== null}
        title={t("backgroundServices.confirmTitle", { name: confirming ? displayName(confirming, locale) : "" })}
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
