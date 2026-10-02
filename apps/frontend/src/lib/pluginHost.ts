/**
 * SDK que o POM entrega às telas de plugin (`globalThis.__POM_HOST__`).
 *
 * Uma tela de plugin é um módulo ES empacotado pelo próprio plugin. Ele **não**
 * carrega segunda cópia de React: os imports de `react`, `react-dom`,
 * `react/jsx-runtime`, `@tanstack/react-query` e do kit de design do POM são
 * trocados, na hora de empacotar, por leituras deste objeto. Assim hooks e
 * contextos (idioma, tema, QueryClient) são os mesmos da aplicação e a tela
 * herda o visual sem copiar o design system. A superfície é versionada
 * (`version`): só cresce de forma compatível. A versão 2 acrescentou o
 * protocolo `pom-plugin-events/v1` (`events`, `notifications`, `plugin(code)`
 * e `context()`); ver `lib/pluginEvents.ts`. Version 3 adds the basic
 * `TelemetryLine` and the `chat.telemetry` slot contract (`lib/chatTelemetry.ts`);
 * `ExecutionTelemetry` now renders that basic line. Version 4 adds
 * `plugin(code).rpc(op, params)`, the plugin's own read/write API.
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

export const PLUGIN_HOST_VERSION = 4;

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
    api: { get: api.get },
    storageKey,
    /** Contrato de eventos e comandos entre POM e plugins. */
    protocol: {
      name: PLUGIN_EVENTS_PROTOCOL,
      domEvent: PLUGIN_EVENT_DOM,
      domRequest: PLUGIN_REQUEST_DOM,
    },
    /** Idioma e tema atuais, para quem monta fora do React do host. */
    context: () => ({
      locale: getActiveLocale(),
      theme: document.documentElement.dataset.theme === "light" ? "light" : "dark",
    }),
    events: { subscribe: subscribePluginEvents, last: lastPluginEvent },
    /** API com escopo do plugin: eventos filtrados e comandos assinados pelo código. */
    plugin: scopedPluginApi,
  };
}

export type PluginHost = ReturnType<typeof buildPluginHost>;

/** Instala o SDK uma vez; idempotente. */
export function installPluginHost(): PluginHost {
  const scope = globalThis as unknown as { __POM_HOST__?: PluginHost };
  scope.__POM_HOST__ ??= buildPluginHost();
  return scope.__POM_HOST__;
}
