// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

const plugin = vi.hoisted(() => ({ served: true, demo: false }));
vi.mock("../lib/pluginUi", () => ({ resolvePluginRoute: () => (plugin.served ? { code: "logs" } : null) }));
vi.mock("../lib/session", () => ({ isDemoUser: () => plugin.demo }));
vi.mock("../i18n/provider", () => ({ useI18n: () => ({ t: (key: string) => key }) }));

import { LogsTabs } from "./LogsTabs";

const renderAt = (path: string) =>
  render(<MemoryRouter initialEntries={[path]}><LogsTabs><p>content</p></LogsTabs></MemoryRouter>);

describe("LogsTabs", () => {
  afterEach(() => { cleanup(); plugin.served = true; plugin.demo = false; });

  it("puts the deployment logs in a tab beside the request logs", () => {
    renderAt("/deployments/logs");
    expect(screen.getByRole("tab", { name: "nav.requestLogs" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "nav.deploymentLogs" }).getAttribute("aria-selected")).toBe("true");
    expect(screen.getByText("content")).toBeTruthy();
  });

  it("draws no tabs for a demonstration visitor or without the logs plugin", () => {
    plugin.demo = true;
    renderAt("/logs");
    expect(screen.queryByRole("tab")).toBeNull();
    cleanup();
    plugin.demo = false;
    plugin.served = false;
    renderAt("/logs");
    expect(screen.queryByRole("tab")).toBeNull();
  });
});
