/**
 * The cluster's process and engine lines (`ClusterLogEntry`) drawn on the
 * shared `Terminal`. Used by the Dashboard's cluster console, the Logs
 * screen's Cluster console tab (the core's live tail, no plugin) and the
 * Deployment Logs screen (the logs plugin's history).
 */
import type { ReactNode } from "react";
import type { ClusterLogEntry } from "../lib/types";
import { Terminal, type TerminalLine } from "./Terminal";

export function clusterLines(entries: ClusterLogEntry[]): TerminalLine[] {
  return entries.map((entry) => ({
    key: entry.received_seq,
    source: entry.node_name || entry.node_id,
    time: entry.origin_at,
    level: entry.level,
    text: entry.message,
  }));
}

export function ClusterConsole({
  entries,
  ...rest
}: {
  entries: ClusterLogEntry[];
  loading?: boolean;
  error?: boolean;
  notice?: ReactNode;
  empty: { title: string; hint?: string };
  compact?: boolean;
  testId?: string;
}) {
  return <Terminal lines={clusterLines(entries)} {...rest} />;
}
