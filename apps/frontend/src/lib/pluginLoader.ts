/**
 * POM's official loader and installer of a plugin screen, driven by the plugin.
 *
 * A mounted plugin screen may ask POM to keep it covered and say what is going
 * on: the generic pieces (`loader.show`), an installer with a bar
 * (`loader.progress`), an error with "Try again" (`loader.error`), a short
 * success (`loader.success`) or nothing at all (`loader.hide`). The commands
 * arrive over `pom-plugin-events/v1` (`runPluginCommand` in `pluginEvents.ts`)
 * and are kept here per `plugin_code`; `PluginScreen` draws them. The state
 * belongs to the mounted screen: a command for a plugin whose screen is not
 * mounted is refused, and unmounting forgets it.
 */
import { useSyncExternalStore } from "react";

export type LoaderCommandType = "loader.show" | "loader.progress" | "loader.error" | "loader.success" | "loader.hide";

export type LoaderState =
  | { kind: "generic"; title?: string; message?: string }
  | { kind: "progress"; title?: string; message?: string; progress: number }
  | { kind: "error"; title?: string; message: string; retry: boolean }
  | { kind: "success"; title?: string; message?: string };

/** The text limits of every plugin command (the notification ones too). */
export const MAX_TITLE = 200;
export const MAX_BODY = 2048;

/** How long the success state shows before the screen opens. */
export const LOADER_SUCCESS_MS = 1000;

const COMMANDS: readonly LoaderCommandType[] = ["loader.show", "loader.progress", "loader.error", "loader.success", "loader.hide"];

/** `undefined` when absent, `null` when invalid, else the trimmed text. */
function text(value: unknown, max: number): string | undefined | null {
  if (value === undefined) return undefined;
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  if (trimmed.length > max) return null;
  return trimmed || undefined;
}

/** Validates a command; `null` when invalid, `state: null` for `loader.hide`. */
export function normalizeLoaderCommand(
  type: unknown,
  payload: unknown,
): { type: LoaderCommandType; state: LoaderState | null } | null {
  if (!COMMANDS.includes(type as LoaderCommandType)) return null;
  const command = type as LoaderCommandType;
  if (command === "loader.hide") return { type: command, state: null };
  const request = (typeof payload === "object" && payload !== null ? payload : {}) as Record<string, unknown>;
  const title = text(request.title, MAX_TITLE);
  const message = text(request.message, MAX_BODY);
  if (title === null || message === null) return null;
  const words = { ...(title ? { title } : {}), ...(message ? { message } : {}) };
  switch (command) {
    case "loader.show":
      return { type: command, state: { kind: "generic", ...words } };
    case "loader.success":
      return { type: command, state: { kind: "success", ...words } };
    case "loader.progress": {
      const progress = request.progress;
      if (typeof progress !== "number" || !Number.isFinite(progress)) return null;
      return { type: command, state: { kind: "progress", ...words, progress: Math.round(Math.min(100, Math.max(0, progress))) } };
    }
    case "loader.error": {
      if (!message) return null;
      if (request.retry !== undefined && typeof request.retry !== "boolean") return null;
      return { type: command, state: { kind: "error", ...words, message, retry: request.retry !== false } };
    }
  }
}

interface Entry {
  mounts: number;
  state: LoaderState | null;
  timer?: ReturnType<typeof setTimeout>;
}

const entries = new Map<string, Entry>();
const subscribers = new Set<() => void>();
const openers = new Map<string, Set<() => void>>();

function changed() {
  for (const subscriber of [...subscribers]) subscriber();
}

function open(code: string) {
  for (const listener of [...(openers.get(code) ?? [])]) listener();
}

function settle(entry: Entry, state: LoaderState | null) {
  if (entry.timer !== undefined) clearTimeout(entry.timer);
  entry.timer = undefined;
  entry.state = state;
}

/** Arms the store for the mounted screen of `code`. The returned disarm forgets its state. */
export function mountPluginLoader(code: string): () => void {
  const entry = entries.get(code) ?? { mounts: 0, state: null };
  entry.mounts += 1;
  entries.set(code, entry);
  let armed = true;
  return () => {
    if (!armed) return;
    armed = false;
    entry.mounts -= 1;
    if (entry.mounts > 0) return;
    settle(entry, null);
    entries.delete(code);
    changed();
  };
}

/** Applies a validated command to the mounted screen of `code`. */
export function applyLoaderCommand(
  code: string,
  type: LoaderCommandType,
  state: LoaderState | null,
): "delivered" | "screen_not_mounted" {
  const entry = entries.get(code);
  if (!entry || entry.mounts === 0) return "screen_not_mounted";
  const previous = entry.state;
  if (type === "loader.hide" || state === null) {
    settle(entry, null);
    changed();
    open(code);
    return "delivered";
  }
  if (state.kind === "progress" && previous?.kind === "progress") {
    // The bar never moves backwards on screen; the message still updates.
    state = { ...state, progress: Math.max(previous.progress, state.progress) };
  }
  settle(entry, state);
  if (state.kind === "success") {
    entry.timer = setTimeout(() => {
      entry.timer = undefined;
      entry.state = null;
      changed();
      open(code);
    }, LOADER_SUCCESS_MS);
  }
  changed();
  return "delivered";
}

/** The current state of `code`'s loader, `null` when the plugin asked for none. */
export function loaderState(code: string): LoaderState | null {
  return entries.get(code)?.state ?? null;
}

export function usePluginLoader(code: string): LoaderState | null {
  return useSyncExternalStore(
    (onChange) => {
      subscribers.add(onChange);
      return () => subscribers.delete(onChange);
    },
    () => loaderState(code),
  );
}

/** Runs `listener` when the plugin opens its screen: `loader.hide`, or the end of `loader.success`. */
export function onLoaderOpen(code: string, listener: () => void): () => void {
  const set = openers.get(code) ?? new Set();
  set.add(listener);
  openers.set(code, set);
  return () => {
    set.delete(listener);
  };
}

/** For tests only. */
export function resetPluginLoaderForTest() {
  for (const entry of entries.values()) if (entry.timer !== undefined) clearTimeout(entry.timer);
  entries.clear();
  openers.clear();
  changed();
}
