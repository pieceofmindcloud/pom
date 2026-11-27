/**
 * The Logs screen's tabs, for every admin and worker, with or without the
 * logs plugin:
 *
 * - Request logs (`/logs`) - only when a plugin serves that route;
 * - Cluster console (`/logs/console`) - admins: every node's process lines,
 *   the network's deployment log, kept live by the core;
 * - one tab per log source (`/logs/source/<code>`): llama.cpp, and each
 *   plugin service that publishes a log (`PluginService.logs`, or any
 *   inference service). A worker sees its own machine's; an admin sees the
 *   sources of every machine and picks the machine inside the tab;
 * - Deployment logs (`/deployments/logs`) - admins, when the logs plugin
 *   keeps that history.
 *
 * The plugin only adds its tabs; the rest is the POM's.
 */
import type { Profile } from "./profile";
import type { NodeInfo, PeerView } from "./types";
import { pluginRouteProfiles } from "./pluginUi";

export const LLAMA_CPP_SOURCE = "llama.cpp";
export const CONSOLE_ROUTE = "/logs/console";
export const SOURCE_ROUTE_PREFIX = "/logs/source/";
export const DEPLOYMENT_LOGS_ROUTE = "/deployments/logs";

export type LogsTab = { value: string; label: string };

/** A log source and the machines that have it. */
export type LogSource = { code: string; name: string; machines: { nodeId: string; name: string }[] };

export function sourceRoute(code: string): string {
  return `${SOURCE_ROUTE_PREFIX}${encodeURIComponent(code)}`;
}

export function sourceOfPath(path: string): string | null {
  const pathname = path.split(/[?#]/, 1)[0];
  if (!pathname.startsWith(SOURCE_ROUTE_PREFIX)) return null;
  return decodeURIComponent(pathname.slice(SOURCE_ROUTE_PREFIX.length)) || null;
}

/**
 * The log sources of these machines: llama.cpp on every machine that may
 * hold a model, and each plugin service that publishes a log.
 */
export function logSources(nodes: { nodeId: string; node: NodeInfo | null | undefined }[]): LogSource[] {
  const byCode = new Map<string, LogSource>();
  const add = (code: string, name: string, nodeId: string, machine: string) => {
    const entry = byCode.get(code) ?? { code, name, machines: [] };
    if (!entry.machines.some((m) => m.nodeId === nodeId)) entry.machines.push({ nodeId, name: machine });
    byCode.set(code, entry);
  };
  for (const { nodeId, node } of nodes) {
    if (!node || node.profile === "visitor") continue;
    const machine = node.hostname || nodeId.slice(0, 8);
    add(LLAMA_CPP_SOURCE, "llama.cpp", nodeId, machine);
    for (const service of node.plugin_services ?? []) {
      if (service.kind !== "inference" && !service.logs) continue;
      add(service.plugin_code, service.name || service.plugin_code, nodeId, machine);
    }
  }
  return [...byCode.values()];
}

/** The machines whose sources this profile reads: its own, or the network's for an admin. */
export function logNodes(profile: Profile, self: NodeInfo | undefined, peers: PeerView[] | undefined) {
  if (profile === "admin" && peers?.length) return peers.map((peer) => ({ nodeId: peer.node_id, node: peer.node }));
  return self ? [{ nodeId: self.node_id, node: self }] : [];
}

/** Whether the hub is this profile's at all: admins and workers. */
export function hasLogsHub(profile: Profile): boolean {
  return profile === "admin" || profile === "worker";
}

export function logsTabs(profile: Profile, sources: LogSource[], t: (key: string) => string): LogsTab[] {
  const tabs: LogsTab[] = [];
  const plugin = pluginServesLogs(profile);
  if (plugin) tabs.push({ value: "/logs", label: t("nav.requestLogs") });
  if (!hasLogsHub(profile)) return tabs;
  if (profile === "admin") tabs.push({ value: CONSOLE_ROUTE, label: t("logs.console") });
  for (const source of sources) tabs.push({ value: sourceRoute(source.code), label: source.name });
  if (plugin && profile === "admin") tabs.push({ value: DEPLOYMENT_LOGS_ROUTE, label: t("nav.deploymentLogs") });
  return tabs;
}

/** A plugin serves `/logs` (Request logs) to this profile: its manifest's profiles decide. */
export function pluginServesLogs(profile: Profile): boolean {
  return pluginRouteProfiles("/logs")?.includes(profile) ?? false;
}

/** Where the Logs entry lands when no plugin serves `/logs` to this profile. */
export function logsHome(profile: Profile): string {
  return profile === "admin" ? CONSOLE_ROUTE : sourceRoute(LLAMA_CPP_SOURCE);
}

/** The hub's own routes, which are not the advertising fallback of `/logs`. */
export function isLogsHubPath(path: string): boolean {
  const pathname = path.split(/[?#]/, 1)[0];
  return pathname === CONSOLE_ROUTE || pathname.startsWith(SOURCE_ROUTE_PREFIX);
}
