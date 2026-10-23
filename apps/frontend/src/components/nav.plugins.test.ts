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
  // Usage left the menu: it is the button beside the bell, drawn only for a
  // node with a budget set (`UsageButton`).
  it("is not a menu entry", () => {
    const leaves = navForProfile("admin", true).flatMap((section) => section.children ?? []);
    expect(leaves.some((leaf) => leaf.to === USAGE_ROUTE)).toBe(false);
  });
});
