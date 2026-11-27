import { Layers, Power, X } from "lucide-react";
import { useState } from "react";
import { Confirm } from "../components/Confirm";
import { Badge, Button, Empty, PageHeader, Panel, PanelHeader } from "../components/ui";
import { useI18n } from "../i18n/provider";
import { closePlugin, openPlugin } from "../lib/api";
import { pluginDisplayName, type ActivePluginUi } from "../lib/pluginUi";
import { usePluginUi } from "../lib/queries";
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

/**
 * Settings > Background services: the on-demand plugins of this machine,
 * running first, with the memory each uses and a Close that frees it. A
 * plugin with no menu entry (the Goose agent) has no X badge, so this screen
 * is where it is closed.
 */
export function BackgroundServices() {
  const { t, locale } = useI18n();
  const toast = useToast();
  const query = usePluginUi();
  const [confirming, setConfirming] = useState<ActivePluginUi | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const plugins = (query.data?.plugins ?? []).filter((plugin) => plugin.on_demand);
  const running = plugins.filter((plugin) => plugin.running);
  const stopped = plugins.filter((plugin) => !plugin.running);

  const act = (plugin: ActivePluginUi, open: boolean) => {
    setBusy(plugin.code);
    (open ? openPlugin(plugin.code) : closePlugin(plugin.code))
      .catch((error: unknown) => toast.fromError(t(open ? "plugins.openFailed" : "plugins.closeFailed"), error))
      .finally(() => {
        setBusy(null);
        void query.refetch();
      });
  };

  const row = (plugin: ActivePluginUi) => (
    <li
      key={plugin.code}
      data-testid={`background-service-${plugin.code}`}
      className="flex items-center gap-3 border-b border-line px-4 py-3 last:border-b-0"
    >
      <div className="min-w-0 flex-1">
        <div className="font-medium text-fg">{displayName(plugin, locale)}</div>
        <div className="flex flex-wrap items-center gap-2 text-xs text-faint">
          {plugin.version && <span>{plugin.version}</span>}
          {plugin.running && plugin.memory_bytes != null && (
            <Badge mono>{formatMemory(plugin.memory_bytes)}</Badge>
          )}
          {!plugin.running && <Badge>{t("backgroundServices.stopped")}</Badge>}
        </div>
      </div>
      {plugin.running ? (
        <Button size="sm" variant="outline" icon={<X size={14} />} loading={busy === plugin.code} onClick={() => setConfirming(plugin)}>
          {t("backgroundServices.close")}
        </Button>
      ) : (
        <Button size="sm" variant="ghost" icon={<Power size={14} />} loading={busy === plugin.code} onClick={() => act(plugin, true)}>
          {t("backgroundServices.open")}
        </Button>
      )}
    </li>
  );

  return (
    <div className="space-y-5">
      <PageHeader title={t("backgroundServices.title")} subtitle={t("backgroundServices.subtitle")} />
      <Panel>
        {plugins.length === 0 ? (
          <Empty icon={<Layers size={18} />} title={t("backgroundServices.empty")} />
        ) : (
          <>
            <PanelHeader title={t("backgroundServices.running", { count: running.length })} />
            <ul>{running.map(row)}</ul>
            {stopped.length > 0 && (
              <>
                <PanelHeader title={t("backgroundServices.stoppedList")} />
                <ul>{stopped.map(row)}</ul>
              </>
            )}
          </>
        )}
      </Panel>
      <Confirm
        open={confirming !== null}
        title={t("backgroundServices.confirmTitle", { name: confirming ? displayName(confirming, locale) : "" })}
        description={t("backgroundServices.confirmBody")}
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
