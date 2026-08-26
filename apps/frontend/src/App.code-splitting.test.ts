// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";

const state = vi.hoisted(() => ({ dashboardLoaded: false }));

vi.mock("./routes/Dashboard", () => {
  state.dashboardLoaded = true;
  return { Dashboard: () => null };
});

describe("route code splitting", () => {
  it("does not load the dashboard module until its route loader runs", async () => {
    const { routeLoaders } = await import("./App");

    expect(state.dashboardLoaded).toBe(false);

    const Dashboard = await routeLoaders.dashboard();

    expect(state.dashboardLoaded).toBe(true);
    expect(Dashboard).toBeTypeOf("function");
  });
});
