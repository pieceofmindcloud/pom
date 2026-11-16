// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { storageKey } from "./brand";
import { usePanZoom } from "./usePanZoom";

describe("usePanZoom", () => {
  afterEach(() => { window.localStorage.clear(); vi.useRealTimers(); });

  it("reopens at the view saved in this browser", () => {
    window.localStorage.setItem(storageKey("map-view.test"), JSON.stringify({ x: -120, y: 40, zoom: 1.2 }));
    const { result } = renderHook(() => usePanZoom(0.4, 1.6, "map-view.test"));
    expect(result.current.view).toEqual({ x: -120, y: 40, zoom: 1.2 });
  });

  it("saves the view after it changes and clamps a saved zoom", () => {
    vi.useFakeTimers();
    window.localStorage.setItem(storageKey("map-view.test"), JSON.stringify({ x: 0, y: 0, zoom: 9 }));
    const { result } = renderHook(() => usePanZoom(0.4, 1.6, "map-view.test"));
    expect(result.current.zoom).toBe(1.6);
    act(() => result.current.setZoom(1));
    act(() => { vi.advanceTimersByTime(300); });
    expect(JSON.parse(window.localStorage.getItem(storageKey("map-view.test"))!).zoom).toBe(1);
  });

  it("ignores a broken saved value and keeps nothing without a key", () => {
    window.localStorage.setItem(storageKey("map-view.test"), "{not json");
    const { result } = renderHook(() => usePanZoom(0.4, 1.6, "map-view.test"));
    expect(result.current.view).toEqual({ x: 0, y: 0, zoom: 1 });
    const { result: plain } = renderHook(() => usePanZoom(0.4, 1.6));
    expect(plain.current.view).toEqual({ x: 0, y: 0, zoom: 1 });
  });
});
