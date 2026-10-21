import { describe, expect, it, vi } from "vitest";
import { navForProfile, USAGE_ROUTE } from "./nav";

// The logs plugin as it declares itself: Request logs and Statistics replace
// the native slots, Quota is a new entry.
vi.mock("../lib/pluginUi", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/pluginUi")>()),
  resolvePluginRoute: (path: string) => (path === "/logs" || path === "/statistics" ? { code: "logs" } : null),
  pluginMenuItems: () => [
    { code: "logs", id: "request-logs", label: { en: "Request logs" }, to: "/logs" },
    { code: "logs", id: "statistics", label: { en: "Statistics" }, to: "/statistics" },
    { code: "logs", id: "quota", label: { en: "Quota" }, to: "/logs/quota" },
    { code: "other", id: "tool", label: { en: "Tool" }, to: "/other/tool" },
  ],
}));

describe("plugin menu placement", () => {
  it("puts a plugin's new entry right after the entries it already holds", () => {
    const labels = navForProfile("admin").map((section) => section.labelKey);
    const statistics = labels.indexOf("plugin:logs:statistics");
    expect(statistics).toBeGreaterThan(-1);
    expect(labels[statistics + 1]).toBe("plugin:logs:quota");
  });

  it("puts an entry of a plugin with no other entry before Documentation", () => {
    const labels = navForProfile("admin").map((section) => section.labelKey);
    expect(labels[labels.indexOf("nav.documentation") - 1]).toBe("plugin:other:tool");
  });
});

describe("Usage", () => {
  const usage = (options?: { usage?: boolean }) =>
    navForProfile("admin", true, options)
      .flatMap((section) => section.children ?? [])
      .some((leaf) => leaf.to === USAGE_ROUTE);

  it("leaves Usage out when no machine runs the logs plugin", () => {
    expect(usage({ usage: false })).toBe(false);
  });

  it("keeps Usage when a machine runs it, or when nobody said", () => {
    expect(usage({ usage: true })).toBe(true);
    expect(usage()).toBe(true);
  });
});
