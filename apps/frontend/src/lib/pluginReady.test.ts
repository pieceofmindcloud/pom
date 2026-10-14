// @vitest-environment jsdom
import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  armPluginReady,
  PLUGIN_READY_GRACE_MS,
  pluginReadyIsArmed,
  signalPluginReady,
  usePluginReady,
} from "./pluginReady";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  // A gate left armed by a failed assertion would leak into the next test.
  signalPluginReady();
  expect(pluginReadyIsArmed()).toBe(false);
});

describe("the plugin readiness gate", () => {
  it("keeps the screen closed until the plugin signals it finished rendering", () => {
    vi.useFakeTimers();
    const { result } = renderHook(() => usePluginReady(true));

    expect(result.current).toBe(false);
    expect(pluginReadyIsArmed()).toBe(true);

    act(() => signalPluginReady());
    expect(result.current).toBe(true);
    expect(pluginReadyIsArmed()).toBe(false);
  });

  it("opens the screen on its own when the plugin never signals", () => {
    vi.useFakeTimers();
    const { result } = renderHook(() => usePluginReady(true));

    expect(result.current).toBe(false);
    act(() => {
      vi.advanceTimersByTime(PLUGIN_READY_GRACE_MS);
    });
    expect(result.current).toBe(true);
  });

  it("waits for the signal even when the plugin answers before the wait ends", () => {
    vi.useFakeTimers();
    const { result } = renderHook(() => usePluginReady(true));

    act(() => {
      vi.advanceTimersByTime(400);
    });
    act(() => signalPluginReady());
    expect(result.current).toBe(true);
  });

  it("does not arm a gate when there is nothing to load", () => {
    const { result } = renderHook(() => usePluginReady(false));

    expect(result.current).toBe(false);
    expect(pluginReadyIsArmed()).toBe(false);
    // A stray signal cannot open a screen the host is not waiting for.
    signalPluginReady();
    expect(result.current).toBe(false);
  });

  it("disarms when the mount goes away", () => {
    const { result, unmount } = renderHook(() => usePluginReady(true));
    expect(pluginReadyIsArmed()).toBe(true);

    unmount();
    expect(pluginReadyIsArmed()).toBe(false);
    signalPluginReady();
    expect(result.current).toBe(false);
  });

  it("re-arms for a new mount of the same hook", () => {
    vi.useFakeTimers();
    const { result, rerender } = renderHook(({ active }) => usePluginReady(active), {
      initialProps: { active: true },
    });
    act(() => signalPluginReady());
    expect(result.current).toBe(true);

    rerender({ active: false });
    expect(result.current).toBe(false);
    expect(pluginReadyIsArmed()).toBe(false);

    rerender({ active: true });
    expect(result.current).toBe(false);
    expect(pluginReadyIsArmed()).toBe(true);
    act(() => {
      vi.advanceTimersByTime(PLUGIN_READY_GRACE_MS);
    });
    expect(result.current).toBe(true);
  });

  it("arms on its own too (the host-side helper)", () => {
    vi.useFakeTimers();
    let opened = false;
    const disarm = armPluginReady(() => {
      opened = true;
    });
    const wait = window.setTimeout(() => {
      opened = true;
    }, PLUGIN_READY_GRACE_MS);

    expect(opened).toBe(false);
    signalPluginReady();
    expect(opened).toBe(true);

    window.clearTimeout(wait);
    disarm();
  });
});