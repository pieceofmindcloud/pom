/**
 * SDK that POM hands to plugin screens (`globalThis.__POM_HOST__`).
 *
 * A plugin screen is an ES module bundled by the plugin itself. It does **not**
 * load a second copy of React: the imports of `react`, `react-dom`,
 * `react/jsx-runtime`, `@tanstack/react-query` and POM's design kit are
 * replaced, at bundling time, by reads of this object. That way hooks and
 * contexts (language, theme, QueryClient) are the application's own and the
 * screen inherits the look without copying the design system. The surface is
 * versioned (`version`): it only grows in a compatible way. Version 2 added the
 * `pom-plugin-events/v1` protocol (`events`, `notifications`, `plugin(code)`
 * and `context()`); see `lib/pluginEvents.ts`. Version 3 adds the basic
 * `TelemetryLine` and the `chat.telemetry` slot contract (`lib/chatTelemetry.ts`);
 * `ExecutionTelemetry` now renders that basic line. Version 4 adds
 * `plugin(code).rpc(op, params)`, the plugin's own read/write API. Version 5
 * adds `ui.Tabs`, the panel's tab row (theme tokens `--tab-*`). Version 6 adds
 * `ready()`, the screen's "finished rendering" signal: until a plugin screen gives
 * it, POM keeps the pieces animation in place of the screen (`lib/pluginReady.ts`).
 * Version 7 adds `plugin(code).loader`, POM's official loader and installer over
 * the plugin's screen (`loader.*` commands and the `loader.retry` event; see
 * `lib/pluginLoader.ts`).
 * Version 8 adds `api.post`, a same-origin POST to the node's API with the
 * panel's own credentials, so a plugin screen can act on the node routes it
 * draws (the Hugging Face plugin starts downloads and saves the token).
 */
import * as React from "react";
import * as ReactDOM from "react-dom";
import * as jsxRuntime from "react/jsx-runtime";
import * as reactQuery from "@tanstack/react-query";
import * as ui from "../components/ui";
import * as format from "./format";
import { Modal } from "../components/Confirm";
import { TelemetryLine, type TelemetryLineProps } from "../components/TelemetryLine";
import { Markdown } from "../components/Markdown";
import { api } from "./api";
import { storageKey } from "./brand";
import { useCluster } from "./queries";
import { useTheme } from "./theme";
import { useI18n } from "../i18n/provider";
import { getActiveLocale } from "../i18n/locales";
import {
  lastPluginEvent,
  PLUGIN_EVENT_DOM,
  PLUGIN_EVENTS_PROTOCOL,
  PLUGIN_REQUEST_DOM,
  scopedPluginApi,
  subscribePluginEvents,
} from "./pluginEvents";
import { signalPluginReady } from "./pluginReady";

export const PLUGIN_HOST_VERSION = 8;

/**
 * Compatibility for plugins built against host version 2, which rendered the
 * POM's detailed `ExecutionTelemetry`. That view moved into the plugin that
 * owns it; older bundles now get the basic line instead of a crash.
 */
function ExecutionTelemetryCompat(props: TelemetryLineProps) {
  return React.createElement(TelemetryLine, { trace: props.trace, timings: props.timings, source: props.source });
}

export function buildPluginHost() {
  return {
    version: PLUGIN_HOST_VERSION,
    React,
    ReactDOM,
    jsxRuntime,
    reactQuery,
    ui,
    format,
    components: { Modal, ExecutionTelemetry: ExecutionTelemetryCompat, TelemetryLine, Markdown },
    hooks: { useI18n, useTheme, useCluster },
    api: { get: api.get, post: api.post },
    storageKey,
    /** Events and commands contract between POM and plugins. */
    protocol: {
      name: PLUGIN_EVENTS_PROTOCOL,
      domEvent: PLUGIN_EVENT_DOM,
      domRequest: PLUGIN_REQUEST_DOM,
    },
    /** Current language and theme, for whoever mounts outside the host's React. */
    context: () => ({
      locale: getActiveLocale(),
      theme: document.documentElement.dataset.theme === "light" ? "light" : "dark",
    }),
    events: { subscribe: subscribePluginEvents, last: lastPluginEvent },
    /** Plugin-scoped API: filtered events and commands signed by the code. */
    plugin: scopedPluginApi,
    /**
     * "My screen finished rendering": POM takes the pieces animation away and
     * shows the screen. Call it once the first content is really on screen (after
     * the first data settles, not on the first render). A screen that never calls
     * it is shown anyway, after a short wait.
     */
    ready: signalPluginReady,
  };
}

export type PluginHost = ReturnType<typeof buildPluginHost>;

/** Installs the SDK once; idempotent. */
export function installPluginHost(): PluginHost {
  const scope = globalThis as unknown as { __POM_HOST__?: PluginHost };
  scope.__POM_HOST__ ??= buildPluginHost();
  return scope.__POM_HOST__;
}
