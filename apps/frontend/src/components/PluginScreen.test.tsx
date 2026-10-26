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

/*
 * The fixture module stands in for a plugin's own bundle: the real loader imports
 * the URL `pluginAssetUrl` returns, and this is the one URL vitest can resolve.
 */
vi.mock("../lib/pluginUi", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/pluginUi")>()),
  pluginAssetUrl: (_code: string, asset: string) => `./__fixtures__/${asset}`,
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

describe("PluginScreen opening", () => {
  const fixture = (exportName: string) =>
    ({
      code: "logs",
      route: { path: "/logs", full_bleed: true },
      screen: { module: "pluginScreenModule.tsx", export: exportName },
      version: null,
    }) as unknown as ResolvedPluginRoute;

  const renderFixture = (exportName: string) => {
    const client = new QueryClient();
    return render(
      <QueryClientProvider client={client}>
        <LocaleProvider initialLocale="en">
          <PluginScreen resolved={fixture(exportName)} />
        </LocaleProvider>
      </QueryClientProvider>,
    );
  };

  it("shows the pieces while the plugin screen opens, and only then the screen", async () => {
    renderFixture("silent");

    // The module is on its way: the puzzle is up, the plugin is not.
    const loading = await screen.findByTestId("plugin-loading-piece");
    expect(screen.getAllByTestId("puzzle-piece")).toHaveLength(3);
    expect(screen.getByTestId("loading-piece")).toBeTruthy();
    expect(loading.textContent).toContain("Opening logs");
    // The plugin gets mounted as soon as its module arrives, but out of sight:
    // it cannot be read or used until the animation is taken away.
    await waitFor(() => expect(document.querySelector(".pom-plugin-pending")).toBeTruthy());
    const pending = document.querySelector(".pom-plugin-pending");
    expect(pending?.textContent).toContain("plugin-screen:silent");
    expect(pending?.getAttribute("aria-hidden")).toBe("true");

    // Without the signal, the wait opens it anyway.
    await waitFor(() => expect(screen.queryByTestId("plugin-loading-piece")).toBeNull(), { timeout: 4000 });
    expect(document.querySelector(".pom-plugin-pending")).toBeNull();
    expect(screen.getByText("plugin-screen:silent")).toBeTruthy();
  });

  it("opens as soon as the plugin signals it finished rendering", async () => {
    renderFixture("signalling");

    // The signal arrives from the mounted screen, well before the 2 s the host
    // would give up waiting for it.
    await waitFor(() => expect(screen.getByText("plugin-screen:signalling")).toBeTruthy(), { timeout: 1000 });
    expect(screen.queryByTestId("plugin-loading-piece")).toBeNull();
  });
});

describe("PluginScreen with POM's loader", () => {
  const renderLoaderFixture = (exportName: string) => {
    const client = new QueryClient();
    return render(
      <QueryClientProvider client={client}>
        <LocaleProvider initialLocale="en">
          <PluginScreen
            resolved={{ code: "logs", route: { path: "/logs", full_bleed: true }, screen: { module: "pluginScreenModule.tsx", export: exportName }, version: null } as unknown as ResolvedPluginRoute}
          />
        </LocaleProvider>
      </QueryClientProvider>,
    );
  };

  it("keeps the installer up past the grace and past ready()", async () => {
    renderLoaderFixture("installing");
    const bar = await screen.findByRole("progressbar");
    expect(bar.getAttribute("aria-valuenow")).toBe("30");
    expect(screen.getByText("30%")).toBeTruthy();
    expect(screen.getByText("Installing Python packages")).toBeTruthy();
    await new Promise((resolve) => setTimeout(resolve, 2500));
    expect(screen.getByRole("progressbar")).toBeTruthy();
    expect(document.querySelector(".pom-plugin-pending")).toBeTruthy();
  }, 6000);

  it("covers an open screen again when the plugin reports an error", async () => {
    renderLoaderFixture("failsLater");
    await waitFor(() => expect(document.querySelector(".pom-plugin-open")).toBeTruthy());
    await screen.findByText("Hermes Agent stopped");
    expect(screen.getByRole("button", { name: "Try again" })).toBeTruthy();
    expect(document.querySelector(".pom-plugin-pending")).toBeTruthy();
  });

  it("shows success, then opens the screen", async () => {
    renderLoaderFixture("succeeds");
    await screen.findByTestId("plugin-loader-done");
    await waitFor(() => expect(document.querySelector(".pom-plugin-open")).toBeTruthy(), { timeout: 2500 });
    expect(screen.queryByTestId("plugin-loader-done")).toBeNull();
  });

  it("does not loop when a plugin repeats the same state on every render", async () => {
    renderLoaderFixture("repeats");
    await screen.findByText("Starting");
    await new Promise((resolve) => setTimeout(resolve, 300));
    expect(screen.getByText("Starting")).toBeTruthy();
    expect(document.querySelector(".pom-plugin-pending")?.textContent).toContain("plugin-screen:repeats");
  });

  it("starts a remount with no installer state", async () => {
    const first = renderLoaderFixture("installing");
    await screen.findByRole("progressbar");
    first.unmount();
    renderLoaderFixture("silent");
    await screen.findByTestId("plugin-loading-piece");
    expect(screen.queryByRole("progressbar")).toBeNull();
  });
});

describe("PluginScreen for an on-demand plugin", () => {
  it("shows the paused puzzle piece, says why, and starts the process again", async () => {
    pluginState.plugins = [{ code: "harness", on_demand: true, running: false }];
    api.openPlugin.mockResolvedValue({ code: "harness", running: true });
    renderScreen();

    const paused = screen.getByTestId("paused-piece");
    expect(screen.getByText("The harness piece is paused")).toBeTruthy();
    expect(screen.getAllByTestId("puzzle-piece")).toHaveLength(3);
    expect(screen.getByTestId("paused-piece-sign")).toBeTruthy();
    // Why it can be stopped, and that it can be closed again.
    expect(paused.textContent).toContain("save this machine's resources");
    expect(paused.textContent).toContain("close it again with the X");
    fireEvent.click(screen.getByRole("button", { name: "Open harness" }));

    await waitFor(() => expect(api.openPlugin).toHaveBeenCalledWith("harness"));
  });

  it("does not show the stopped card while the process runs", () => {
    pluginState.plugins = [{ code: "harness", on_demand: true, running: true }];
    renderScreen();

    expect(screen.queryByTestId("paused-piece")).toBeNull();
  });
});
