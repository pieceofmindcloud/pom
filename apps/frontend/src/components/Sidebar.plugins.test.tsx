// @vitest-environment jsdom
import React from "react";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter, useLocation } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";
import { Sidebar } from "./Sidebar";

const plugins = vi.hoisted(() => ({
  list: [] as Array<{ code: string; on_demand?: boolean; running?: boolean }>,
}));
const api = vi.hoisted(() => ({
  openPlugin: vi.fn(() => Promise.resolve({ code: "harness", running: true })),
  closePlugin: vi.fn(() => Promise.resolve({ code: "harness", running: false })),
}));

vi.mock("../lib/queries", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/queries")>()),
  useNode: () => ({ data: null }),
  usePluginUi: () => ({ data: { plugins: plugins.list } }),
}));

vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  openPlugin: api.openPlugin,
  closePlugin: api.closePlugin,
}));

// One menu item from a plugin named "harness"; the registry is not what these tests exercise.
vi.mock("../lib/pluginUi", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/pluginUi")>()),
  pluginMenuItems: () => [
    { code: "harness", id: "harness", label: { en: "Harness" }, to: "/harness", roles: ["admin", "node", "infer"] },
  ],
  pluginLabel: (key: string) => (key === "plugin:harness:harness" ? "Harness" : null),
}));

vi.mock("../lib/networkChatUnread", () => ({
  useNetworkChatUnread: () => ({ unreadCount: 0, snapshot: null }),
  useLobbyUnread: () => 0,
}));

vi.mock("../lib/theme", () => ({
  useTheme: () => ({ theme: "dark", toggle: vi.fn() }),
}));

vi.mock("./Logo", () => ({
  LogoFull: () => <div aria-hidden="true" />,
  LogoMark: () => <div aria-hidden="true" />,
}));

function RouteProbe() {
  return <output data-testid="route">{useLocation().pathname}</output>;
}

function renderSidebar() {
  return render(
    <QueryClientProvider client={new QueryClient()}>
      <MemoryRouter initialEntries={["/"]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <LocaleProvider initialLocale="en">
          <Sidebar />
          <RouteProbe />
        </LocaleProvider>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  plugins.list = [];
  api.openPlugin.mockReset().mockResolvedValue({ code: "harness", running: true });
  api.closePlugin.mockReset().mockResolvedValue({ code: "harness", running: false });
  window.localStorage.clear();
});

describe("Sidebar plugin menu", () => {
  it("shows the stop badge on a running on-demand plugin and closes its process", async () => {
    plugins.list = [{ code: "harness", on_demand: true, running: true }];
    renderSidebar();

    fireEvent.click(screen.getByRole("button", { name: "Stop plugin process" }));

    await waitFor(() => expect(api.closePlugin).toHaveBeenCalledWith("harness"));
  });

  it("starts a stopped on-demand plugin when its menu item is clicked, then opens it", async () => {
    plugins.list = [{ code: "harness", on_demand: true, running: false }];
    renderSidebar();

    expect(screen.queryByRole("button", { name: "Stop plugin process" })).toBeNull();
    fireEvent.click(screen.getByRole("link", { name: "Harness" }));

    await waitFor(() => expect(screen.getByTestId("route").textContent).toBe("/harness"));
    expect(api.openPlugin).toHaveBeenCalledWith("harness");
  });

  it("opens a resident plugin directly, without touching its process", async () => {
    plugins.list = [{ code: "harness", on_demand: false, running: true }];
    renderSidebar();

    expect(screen.queryByRole("button", { name: "Stop plugin process" })).toBeNull();
    fireEvent.click(screen.getByRole("link", { name: "Harness" }));

    await waitFor(() => expect(screen.getByTestId("route").textContent).toBe("/harness"));
    expect(api.openPlugin).not.toHaveBeenCalled();
    expect(api.closePlugin).not.toHaveBeenCalled();
  });
});
