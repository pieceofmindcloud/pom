// @vitest-environment jsdom
import React from "react";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { resetPluginEventsForTest, subscribePluginEvents, type PluginEvent } from "../lib/pluginEvents";
import type { LoaderState } from "../lib/pluginLoader";
import { PluginLoader } from "./PluginLoader";

afterEach(() => {
  cleanup();
  resetPluginEventsForTest();
});

const show = (state: LoaderState | null) =>
  render(
    <LocaleProvider initialLocale="en">
      <PluginLoader code="harness" state={state} />
    </LocaleProvider>,
  );

describe("PluginLoader", () => {
  it("draws the bar, the percentage and the message", () => {
    show({ kind: "progress", progress: 42, message: "Installing Python packages" });
    const bar = screen.getByRole("progressbar");
    expect(bar.getAttribute("aria-valuenow")).toBe("42");
    expect(bar.getAttribute("aria-valuemin")).toBe("0");
    expect(bar.getAttribute("aria-valuemax")).toBe("100");
    expect(screen.getByText("42%")).toBeTruthy();
    expect(screen.getByText("Installing Python packages")).toBeTruthy();
    expect(screen.getByText("Installing harness")).toBeTruthy();
  });

  it("offers Try again on an error and tells only that plugin", () => {
    const seen: PluginEvent[] = [];
    subscribePluginEvents("loader.retry", (event) => seen.push(event), { plugin: "harness" });
    const other: PluginEvent[] = [];
    subscribePluginEvents("loader.retry", (event) => other.push(event), { plugin: "logs" });
    show({ kind: "error", message: "Checksum mismatch", retry: true });
    expect(screen.getByText("Could not install harness")).toBeTruthy();
    expect(screen.getByText("Checksum mismatch")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(seen).toHaveLength(1);
    expect(seen[0].target).toBe("harness");
    expect(other).toHaveLength(0);
  });

  it("offers no retry when the plugin says so", () => {
    show({ kind: "error", message: "Unsupported platform", retry: false });
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
  });

  it("draws the generic pieces and the success state with their default titles", () => {
    show(null);
    expect(screen.getByText("Opening harness")).toBeTruthy();
    cleanup();
    show({ kind: "success" });
    expect(screen.getByTestId("plugin-loader-done")).toBeTruthy();
    expect(screen.getByText("harness installed")).toBeTruthy();
  });

  it("uses the plugin's own title when it sends one", () => {
    show({ kind: "progress", progress: 10, title: "Updating Hermes Agent" });
    expect(screen.getByText("Updating Hermes Agent")).toBeTruthy();
  });
});
