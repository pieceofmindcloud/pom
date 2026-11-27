// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

const state = vi.hoisted(() => ({ served: true, demo: false, profile: "admin" }));
vi.mock("../lib/pluginUi", () => ({ resolvePluginRoute: () => (state.served ? { code: "logs" } : null) }));
vi.mock("../lib/session", () => ({ isDemoUser: () => state.demo }));
vi.mock("../i18n/provider", () => ({ useI18n: () => ({ t: (key: string) => key }) }));
vi.mock("../lib/profile", () => ({ useProfile: () => ({ profile: state.profile }) }));
const self = { node_id: "a", hostname: "machine-a", profile: "admin", plugin_services: [] };
const other = {
  node_id: "b",
  hostname: "machine-b",
  profile: "worker",
  plugin_services: [
    { plugin_code: "vllm", id: "vllm", kind: "inference", name: "vLLM", status: "ready" },
    { plugin_code: "qdrant", id: "rag", kind: "rag", name: "Qdrant", status: "ready", logs: true },
    { plugin_code: "silent", id: "x", kind: "rag", name: "Silent", status: "ready" },
  ],
};
vi.mock("../lib/queries", () => ({
  useNode: () => ({ data: state.profile === "admin" ? self : { ...other, profile: "worker" }, isLoading: false }),
  useCluster: () => ({ data: { peers: [{ node_id: "a", node: self }, { node_id: "b", node: other }] } }),
}));

import { LogsTabs } from "./LogsTabs";

const renderAt = (path: string) =>
  render(<MemoryRouter initialEntries={[path]}><LogsTabs><p>content</p></LogsTabs></MemoryRouter>);
const tabNames = () => screen.queryAllByRole("tab").map((tab) => tab.textContent);

describe("LogsTabs", () => {
  afterEach(() => { cleanup(); state.served = true; state.demo = false; state.profile = "admin"; });

  it("gives the admin the console, every machine's log sources and the plugin's tabs", () => {
    renderAt("/deployments/logs");
    expect(tabNames()).toEqual(["nav.requestLogs", "logs.console", "llama.cpp", "vLLM", "Qdrant", "nav.deploymentLogs"]);
    expect(screen.getByRole("tab", { name: "nav.deploymentLogs" }).getAttribute("aria-selected")).toBe("true");
    expect(screen.getByText("content")).toBeTruthy();
  });

  it("keeps the POM's tabs without the logs plugin", () => {
    state.served = false;
    renderAt("/logs/console");
    expect(tabNames()).toEqual(["logs.console", "llama.cpp", "vLLM", "Qdrant"]);
  });

  it("shows a worker its own machine's sources and no console", () => {
    state.served = false;
    state.profile = "worker";
    renderAt("/logs/source/llama.cpp");
    expect(tabNames()).toEqual(["llama.cpp", "vLLM", "Qdrant"]);
  });

  it("draws no tabs for a demonstration visitor", () => {
    state.demo = true;
    renderAt("/logs");
    expect(screen.queryByRole("tab")).toBeNull();
  });
});
