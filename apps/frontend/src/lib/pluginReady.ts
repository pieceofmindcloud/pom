/**
 * The "finished rendering" signal a plugin screen gives to POM.
 *
 * A plugin screen is an ES module the host imports and mounts. Importing and
 * mounting is not the same as having something on the screen: the screen can
 * still be waiting for its own data, and showing it that early is a page that
 * fills in front of the person. So `PluginScreen` keeps the pieces animation up
 * and opens it when the plugin says it is done - `__POM_HOST__.ready()` - or
 * after `PLUGIN_READY_GRACE_MS`, whichever comes first. A plugin that never
 * calls `ready()` is therefore shown anyway: the signal only makes the opening
 * nicer, it is never required to use a plugin.
 *
 * The gate is armed by the host mount and disarmed when it goes away, so a
 * `ready()` from a component the host is not waiting on (a slot inside another
 * screen, for example) resolves nothing.
 */
import { useEffect, useState } from "react";
import { onLoaderOpen } from "./pluginLoader";

export const PLUGIN_READY_GRACE_MS = 2000;

type ReadyListener = () => void;

const armed = new Set<ReadyListener>();

/** Holds the host's opening of a plugin screen until `onReady` runs. Returns the disarm. */
export function armPluginReady(onReady: ReadyListener): () => void {
  armed.add(onReady);
  return () => {
    armed.delete(onReady);
  };
}

/** Resolves every armed gate. This is what `__POM_HOST__.ready()` calls. */
export function signalPluginReady(): void {
  if (armed.size === 0) return;
  const listeners = [...armed];
  armed.clear();
  for (const listener of listeners) listener();
}

/** A host mount is waiting for the signal. */
export function pluginReadyIsArmed(): boolean {
  return armed.size > 0;
}

/**
 * The host side of the gate: `true` once the plugin screen may be shown.
 * Arming and the short wait live here so the mount only asks "may I open?".
 * `active` is false while there is nothing to wait for (a stopped on-demand
 * plugin), and a change of it re-arms the gate for the new mount. With `code`,
 * the plugin opening its screen through its loader (`loader.hide`, or the end
 * of `loader.success`) opens the gate too.
 */
export function usePluginReady(active: boolean, code?: string): boolean {
  const [open, setOpen] = useState(false);
  useEffect(() => {
    setOpen(false);
    if (!active) return;
    const disarm = armPluginReady(() => setOpen(true));
    const opened = code ? onLoaderOpen(code, () => setOpen(true)) : () => {};
    const wait = window.setTimeout(() => setOpen(true), PLUGIN_READY_GRACE_MS);
    return () => {
      window.clearTimeout(wait);
      disarm();
      opened();
    };
  }, [active, code]);
  return open;
}