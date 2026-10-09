/**
 * Renders a screen supplied by a plugin.
 *
 * Loads the ES module of the asset declared in the manifest (served by the node at
 * `/api/ui/plugins/<code>/assets/...`, same origin, so the session applies) and
 * mounts its `default` as a React component of the tree itself. A load or
 * render failure becomes an error card; it never brings down the shell.
 */
import { Component, useEffect, useState, type ComponentType, type ReactNode } from "react";
import { installPluginHost } from "../lib/pluginHost";
import { pluginAssetUrl, type ResolvedPluginRoute } from "../lib/pluginUi";
import { useI18n } from "../i18n/provider";
import { Panel, PanelHeader } from "./ui";
import { formatMessage, getActiveLocale } from "../i18n/locales";

const loaded = new Map<string, Promise<ComponentType>>();

function loadStyles(code: string, styles: string[] | undefined, version: string | null) {
  for (const style of styles ?? []) {
    const href = pluginAssetUrl(code, style) + (version ? `?v=${encodeURIComponent(version)}` : "");
    if (document.querySelector(`link[data-plugin-style="${CSS.escape(href)}"]`)) continue;
    const link = document.createElement("link");
    link.rel = "stylesheet";
    link.href = href;
    link.dataset.pluginStyle = href;
    document.head.appendChild(link);
  }
}

/** Each (plugin, version, module) is imported only once. */
export function loadPluginScreen(
  resolved: Pick<ResolvedPluginRoute, "code" | "screen" | "version">,
): Promise<ComponentType> {
  const { code, screen, version } = resolved;
  const url = pluginAssetUrl(code, screen.module) + (version ? `?v=${encodeURIComponent(version)}` : "");
  const cacheKey = `${url}#${screen.export ?? "default"}`;
  let pending = loaded.get(cacheKey);
  if (!pending) {
    installPluginHost();
    loadStyles(code, screen.styles, version);
    const exportName = screen.export ?? "default";
    pending = import(/* @vite-ignore */ url).then((module: Record<string, unknown>) => {
      const component = module[exportName];
      if (typeof component !== "function") {
        throw new Error(formatMessage(getActiveLocale(), "plugins.screenExportMissing", { name: exportName }));
      }
      return component as ComponentType;
    });
    pending.catch(() => loaded.delete(cacheKey));
    loaded.set(cacheKey, pending);
  }
  return pending;
}

function PluginError({ message }: { message: string }) {
  const { t } = useI18n();
  return (
    <main className="flex min-h-full items-center justify-center bg-ink px-5 py-10">
      <Panel className="w-full max-w-xl">
        <PanelHeader title={t("plugins.screenError.title")} subtitle={message} />
      </Panel>
    </main>
  );
}

class Boundary extends Component<{ children: ReactNode }, { error: string | null }> {
  state = { error: null as string | null };
  static getDerivedStateFromError(error: unknown) {
    return { error: error instanceof Error ? error.message : String(error) };
  }
  render() {
    return this.state.error ? <PluginError message={this.state.error} /> : this.props.children;
  }
}

export function PluginScreen({ resolved }: { resolved: ResolvedPluginRoute }) {
  const [state, setState] = useState<{ Screen?: ComponentType; error?: string }>({});
  useEffect(() => {
    let cancelled = false;
    setState({});
    loadPluginScreen(resolved).then(
      (Screen) => !cancelled && setState({ Screen }),
      (error: unknown) => !cancelled && setState({ error: error instanceof Error ? error.message : String(error) }),
    );
    return () => {
      cancelled = true;
    };
  }, [resolved.code, resolved.route.path, resolved.screen.module, resolved.screen.export, resolved.version]);
  if (state.error) return <PluginError message={state.error} />;
  if (!state.Screen) return null;
  const { Screen } = state;
  return (
    <Boundary>
      <Screen />
    </Boundary>
  );
}
