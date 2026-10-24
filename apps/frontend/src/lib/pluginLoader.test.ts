import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  applyLoaderCommand,
  LOADER_SUCCESS_MS,
  loaderState,
  mountPluginLoader,
  normalizeLoaderCommand,
  onLoaderOpen,
  resetPluginLoaderForTest,
} from "./pluginLoader";

afterEach(() => {
  resetPluginLoaderForTest();
  vi.useRealTimers();
});

describe("normalizeLoaderCommand", () => {
  it("rounds progress and trims the message", () => {
    expect(normalizeLoaderCommand("loader.progress", { progress: 42.6, message: " Installing Python packages " })).toEqual({
      type: "loader.progress",
      state: { kind: "progress", progress: 43, message: "Installing Python packages" },
    });
  });

  it("clamps progress to 0..100", () => {
    expect(normalizeLoaderCommand("loader.progress", { progress: 250 })?.state).toEqual({ kind: "progress", progress: 100 });
    expect(normalizeLoaderCommand("loader.progress", { progress: -5 })?.state).toEqual({ kind: "progress", progress: 0 });
  });

  it("refuses progress that is not a finite number", () => {
    for (const progress of [Number.NaN, "40", undefined, Number.POSITIVE_INFINITY]) {
      expect(normalizeLoaderCommand("loader.progress", { progress })).toBeNull();
    }
  });

  it("needs a message for an error and offers retry by default", () => {
    expect(normalizeLoaderCommand("loader.error", {})).toBeNull();
    expect(normalizeLoaderCommand("loader.error", { message: "x" })?.state).toEqual({ kind: "error", message: "x", retry: true });
    expect(normalizeLoaderCommand("loader.error", { message: "x", retry: false })?.state).toEqual({
      kind: "error",
      message: "x",
      retry: false,
    });
  });

  it("refuses oversized text and unknown types", () => {
    expect(normalizeLoaderCommand("loader.show", { title: "t".repeat(201) })).toBeNull();
    expect(normalizeLoaderCommand("loader.show", { message: "m".repeat(2049) })).toBeNull();
    expect(normalizeLoaderCommand("loader.spin", {})).toBeNull();
  });

  it("reads hide as clearing the state", () => {
    expect(normalizeLoaderCommand("loader.hide", {})).toEqual({ type: "loader.hide", state: null });
  });
});

describe("loader store", () => {
  const progress = (value: number, message?: string) =>
    applyLoaderCommand("hermes", "loader.progress", { kind: "progress", progress: value, message });

  it("refuses a plugin whose screen is not mounted", () => {
    expect(progress(10)).toBe("screen_not_mounted");
    expect(loaderState("hermes")).toBeNull();
  });

  describe("mounted", () => {
    let disarm: () => void;
    beforeEach(() => {
      disarm = mountPluginLoader("hermes");
    });

    it("never moves the bar backwards but takes the new message", () => {
      expect(progress(60, "a")).toBe("delivered");
      progress(40, "b");
      expect(loaderState("hermes")).toEqual({ kind: "progress", progress: 60, message: "b" });
    });

    it("starts the bar again after an error", () => {
      progress(60);
      applyLoaderCommand("hermes", "loader.error", { kind: "error", message: "x", retry: true });
      progress(5);
      expect(loaderState("hermes")).toEqual({ kind: "progress", progress: 5 });
    });

    it("opens the screen after the success state shows", () => {
      vi.useFakeTimers();
      const opened = vi.fn();
      onLoaderOpen("hermes", opened);
      applyLoaderCommand("hermes", "loader.success", { kind: "success" });
      expect(loaderState("hermes")).toEqual({ kind: "success" });
      vi.advanceTimersByTime(LOADER_SUCCESS_MS);
      expect(loaderState("hermes")).toBeNull();
      expect(opened).toHaveBeenCalledTimes(1);
    });

    it("opens the screen on hide", () => {
      const opened = vi.fn();
      onLoaderOpen("hermes", opened);
      progress(10);
      applyLoaderCommand("hermes", "loader.hide", null);
      expect(loaderState("hermes")).toBeNull();
      expect(opened).toHaveBeenCalledTimes(1);
    });

    it("forgets the state when the screen goes away", () => {
      progress(30);
      disarm();
      expect(loaderState("hermes")).toBeNull();
      mountPluginLoader("hermes");
      expect(loaderState("hermes")).toBeNull();
    });
  });
});
