/**
 * Mounts the component an active plugin declares for a native extension slot
 * (`slots` in `pom-plugin-ui/v1`), passing the host's props through.
 *
 * Without a claimant, while the module loads, or when it fails to load or
 * render, the slot shows `fallback` (usually nothing): a broken plugin never
 * takes the native screen down with it.
 */
import { Component, useEffect, useState, type ComponentType, type ReactNode } from "react";
import { loadPluginScreen } from "./PluginScreen";
import { pluginSlot, type ResolvedPluginSlot } from "../lib/pluginUi";
import { usePluginUi } from "../lib/queries";

class SlotBoundary extends Component<{ fallback: ReactNode; children: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  componentDidCatch(error: unknown) {
    console.warn("[plugin-slot] render failed", error);
  }
  render() {
    return this.state.failed ? this.props.fallback : this.props.children;
  }
}

/** The plugin claiming `slot`, refreshed with the active plugin list. */
export function usePluginSlot(slot: string): ResolvedPluginSlot | null {
  // Subscribing keeps the caller in step with activations; the registry itself
  // is refreshed by App on the same query.
  usePluginUi();
  return pluginSlot(slot);
}

/** `resolved` comes from `usePluginSlot`, called by the owning screen. */
export function PluginSlot<P extends object>({
  resolved: target,
  props,
  fallback = null,
}: {
  resolved: ResolvedPluginSlot | null;
  props: P;
  fallback?: ReactNode;
}) {
  const [state, setState] = useState<{ key?: string; Slot?: ComponentType<P> }>({});
  const key = target ? `${target.code}:${target.version ?? ""}:${target.screen.module}#${target.screen.export ?? "default"}` : "";

  useEffect(() => {
    if (!target) return;
    let cancelled = false;
    loadPluginScreen(target).then(
      (Loaded) => !cancelled && setState({ key, Slot: Loaded as ComponentType<P> }),
      (error: unknown) => {
        console.warn("[plugin-slot] load failed", error);
        if (!cancelled) setState({ key });
      },
    );
    return () => {
      cancelled = true;
    };
    // `key` captures every field of `target` that changes the module.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  if (!target || state.key !== key || !state.Slot) return <>{fallback}</>;
  const { Slot } = state;
  return (
    <SlotBoundary key={key} fallback={fallback}>
      <Slot {...props} />
    </SlotBoundary>
  );
}
