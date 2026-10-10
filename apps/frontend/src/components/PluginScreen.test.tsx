// @vitest-environment jsdom
import React from "react";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { ResolvedPluginRoute } from "../lib/pluginUi";
import { PluginScreen } from "./PluginScreen";

const pluginState = vi.hoisted(() => ({
  plugins: [] as Array<{ code: string; on_demand?: boolean; running?: boolean }>,
}));
const api = vi.hoisted(() => ({ openPlugin: vi.fn() }));

vi.mock("../lib/queries", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/queries")>()),
  usePluginUi: () => ({ data: { plugins: pluginState.plugins } }),
}));

vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  openPlugin: api.openPlugin,
}));

const resolved = {
  code: "harness",
  route: { path: "/harness", full_bleed: true },
  screen: { module: "ui/screens.js", export: "harness" },
  version: "1.0.0",
} as unknown as ResolvedPluginRoute;

function renderScreen() {
  const client = new QueryClient();
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="en">
        <PluginScreen resolved={resolved} />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  pluginState.plugins = [];
  api.openPlugin.mockReset();
});

describe("PluginScreen for an on-demand plugin", () => {
  it("shows a stopped card with an action to start the process again", async () => {
    pluginState.plugins = [{ code: "harness", on_demand: true, running: false }];
    api.openPlugin.mockResolvedValue({ code: "harness", running: true });
    renderScreen();

    expect(screen.getByText("This plugin process is stopped")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Open plugin" }));

    await waitFor(() => expect(api.openPlugin).toHaveBeenCalledWith("harness"));
  });

  it("does not show the stopped card while the process runs", () => {
    pluginState.plugins = [{ code: "harness", on_demand: true, running: true }];
    renderScreen();

    expect(screen.queryByText("This plugin process is stopped")).toBeNull();
  });
});
