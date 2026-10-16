/**
 * Plugin inbox in the top bar's bell
 * (`components/Notifications.tsx`). Keeps two kinds of item:
 *
 * - questions (`notification.confirm`): wait for Accept or Refuse, like a
 *   transfer offer, and the answer returns to the plugin's window. They live only
 *   in this tab, because this is where the plugin waits for the answer;
 * - notices (`notification.notify`, local or received from the network): just a
 *   message, without buttons. They enter as unread and become read when the person
 *   opens the bell. The read ids stay in `localStorage`, so a network notice
 *   that remains in the node's snapshot does not come back as new after a reload.
 */
import { useSyncExternalStore } from "react";
import { storageKey } from "./brand";
import type { ConfirmRequest, NotificationLevel } from "./pluginEvents";

export interface PendingPluginConfirm {
  id: string;
  code: string;
  request: ConfirmRequest;
  createdAt: string;
}

export interface PluginNotice {
  id: string;
  code: string;
  title: string;
  body?: string;
  level: NotificationLevel;
  createdAt: string;
  /** `network` when it came from a node through the network Chat. */
  source: "local" | "network";
  senderNodeId?: string;
  read: boolean;
}

type Entry = PendingPluginConfirm & { resolve: (action: "accepted" | "cancelled") => void };

/** How many notices the bell keeps; the oldest leave first. */
const MAX_NOTICES = 30;
const MAX_READ_IDS = 200;
const READ_KEY = storageKey("plugin-notices-read");

let confirms: Entry[] = [];
let notices: PluginNotice[] = [];
let snapshot: { confirms: readonly PendingPluginConfirm[]; notices: readonly PluginNotice[] } = {
  confirms: [],
  notices: [],
};
const listeners = new Set<() => void>();

function readIds(): string[] {
  try {
    const value = JSON.parse(localStorage.getItem(READ_KEY) ?? "[]");
    return Array.isArray(value) ? value.filter((id): id is string => typeof id === "string") : [];
  } catch {
    return [];
  }
}

function rememberRead(ids: string[]) {
  if (ids.length === 0) return;
  try {
    const next = [...new Set([...readIds(), ...ids])].slice(-MAX_READ_IDS);
    localStorage.setItem(READ_KEY, JSON.stringify(next));
  } catch {
    // Without storage (private tab): the read state only lasts until a reload.
  }
}

function emit() {
  snapshot = {
    confirms: confirms.map(({ resolve: _resolve, ...entry }) => entry),
    notices: [...notices],
  };
  for (const listener of listeners) listener();
}

export function addPendingPluginConfirm(
  entry: PendingPluginConfirm,
  resolve: (action: "accepted" | "cancelled") => void,
) {
  confirms = [...confirms, { ...entry, resolve }];
  emit();
}

/** Answers and removes the question. An unknown id (already answered) is ignored. */
export function answerPendingPluginConfirm(id: string, action: "accepted" | "cancelled") {
  const entry = confirms.find((item) => item.id === id);
  if (!entry) return;
  confirms = confirms.filter((item) => item.id !== id);
  emit();
  entry.resolve(action);
}

/**
 * Adds a notice to the bell. An id already present is ignored; an id already read
 * before (another visit to the page) enters as read. Returns `true` when the notice
 * is new and not yet read.
 */
export function addPluginNotice(notice: Omit<PluginNotice, "read">): boolean {
  if (notices.some((item) => item.id === notice.id)) return false;
  const read = readIds().includes(notice.id);
  notices = [{ ...notice, read }, ...notices].slice(0, MAX_NOTICES);
  emit();
  return !read;
}

/** Marks every notice as read (the bell was opened and they were seen). */
export function markPluginNoticesRead() {
  const unread = notices.filter((notice) => !notice.read);
  if (unread.length === 0) return;
  rememberRead(unread.map((notice) => notice.id));
  notices = notices.map((notice) => (notice.read ? notice : { ...notice, read: true }));
  emit();
}

/** Removes a notice from the bell. */
export function dismissPluginNotice(id: string) {
  const notice = notices.find((item) => item.id === id);
  if (!notice) return;
  rememberRead([id]);
  notices = notices.filter((item) => item.id !== id);
  emit();
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function current() {
  return snapshot;
}

export function pendingPluginConfirms(): readonly PendingPluginConfirm[] {
  return snapshot.confirms;
}

export function pluginNotices(): readonly PluginNotice[] {
  return snapshot.notices;
}

export function usePendingPluginConfirms(): readonly PendingPluginConfirm[] {
  return useSyncExternalStore(subscribe, current, current).confirms;
}

export function usePluginNotices(): readonly PluginNotice[] {
  return useSyncExternalStore(subscribe, current, current).notices;
}

/** For tests only. */
export function resetPluginInboxForTest() {
  confirms = [];
  notices = [];
  try {
    localStorage.removeItem(READ_KEY);
  } catch {
    // ignore
  }
  emit();
}
