/**
 * SDK que o POM entrega às telas de plugin (`globalThis.__POM_HOST__`).
 *
 * Uma tela de plugin é um módulo ES empacotado pelo próprio plugin. Ele **não**
 * carrega segunda cópia de React: os imports de `react`, `react-dom`,
 * `react/jsx-runtime`, `@tanstack/react-query` e do kit de design do POM são
 * trocados, na hora de empacotar, por leituras deste objeto. Assim hooks e
 * contextos (idioma, tema, QueryClient) são os mesmos da aplicação e a tela
 * herda o visual sem copiar o design system. A superfície é versionada
 * (`version`): só cresce de forma compatível dentro de uma versão.
 */
import * as React from "react";
import * as ReactDOM from "react-dom";
import * as jsxRuntime from "react/jsx-runtime";
import * as reactQuery from "@tanstack/react-query";
import * as ui from "../components/ui";
import * as format from "./format";
import { Modal } from "../components/Confirm";
import { ExecutionTelemetry } from "../components/ExecutionTelemetry";
import { Markdown } from "../components/Markdown";
import { api } from "./api";
import { storageKey } from "./brand";
import { useCluster } from "./queries";
import { useTheme } from "./theme";
import { useI18n } from "../i18n/provider";

export const PLUGIN_HOST_VERSION = 1;

export function buildPluginHost() {
  return {
    version: PLUGIN_HOST_VERSION,
    React,
    ReactDOM,
    jsxRuntime,
    reactQuery,
    ui,
    format,
    components: { Modal, ExecutionTelemetry, Markdown },
    hooks: { useI18n, useTheme, useCluster },
    api: { get: api.get },
    storageKey,
  };
}

export type PluginHost = ReturnType<typeof buildPluginHost>;

/** Instala o SDK uma vez; idempotente. */
export function installPluginHost(): PluginHost {
  const scope = globalThis as unknown as { __POM_HOST__?: PluginHost };
  scope.__POM_HOST__ ??= buildPluginHost();
  return scope.__POM_HOST__;
}
