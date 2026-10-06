/**
 * The bell's notice that the administrative password is still the default.
 *
 * It comes back on every start of the node: the state is kept per `boot_id`
 * (`/api/access`), so a page reload keeps what the person already did with it
 * and a restart shows it again. Opening the bell reads it, and it can be
 * dismissed; ignoring it is fine, it only stops counting on the bell.
 */
import { useSyncExternalStore } from "react";
import { storageKey } from "./brand";

export type DefaultPasswordNoticeState = "unread" | "read" | "dismissed";

const KEY = storageKey("default-password-notice");
const listeners = new Set<() => void>();
/** Used when there is no storage (private tab): valid until the page reloads. */
let memory: string | null = null;

function stored(): string | null {
  try {
    return localStorage.getItem(KEY) ?? memory;
  } catch {
    return memory;
  }
}

function write(boot: string, state: "read" | "dismissed") {
  memory = JSON.stringify({ boot, state });
  try {
    localStorage.setItem(KEY, memory);
  } catch {
    // No storage: `memory` keeps it for this page.
  }
  for (const listener of listeners) listener();
}

/** What the person did with the notice of this start of the node. */
export function defaultPasswordNoticeState(boot: string, raw = stored()): DefaultPasswordNoticeState {
  try {
    const value = JSON.parse(raw ?? "null") as { boot?: unknown; state?: unknown } | null;
    if (value?.boot === boot && (value.state === "read" || value.state === "dismissed")) return value.state;
  } catch {
    // An unreadable record is the same as none.
  }
  return "unread";
}

export function markDefaultPasswordNoticeRead(boot: string) {
  if (defaultPasswordNoticeState(boot) === "unread") write(boot, "read");
}

export function dismissDefaultPasswordNotice(boot: string) {
  write(boot, "dismissed");
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function useDefaultPasswordNoticeState(boot: string): DefaultPasswordNoticeState {
  const raw = useSyncExternalStore(subscribe, stored, () => null);
  return defaultPasswordNoticeState(boot, raw);
}

export function resetDefaultPasswordNoticeForTest() {
  memory = null;
  try {
    localStorage.removeItem(KEY);
  } catch {
    // Nothing to clear.
  }
}
