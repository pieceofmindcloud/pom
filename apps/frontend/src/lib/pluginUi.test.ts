import { afterEach, describe, expect, it } from "vitest";
import { pluginLabel, pluginMenuItems, pluginRouteProfiles, resolvePluginRoute, setPluginUi, type ActivePluginUi } from "./pluginUi";
import { canAccessRoute } from "./access";
import { navForProfile } from "../components/nav";

const plugin: ActivePluginUi = {
  code: "acme",
  version: "2.0.0",
  manifest: {
    schema: "pom-plugin-ui/v1",
    plugin_code: "acme",
    menu: [
      { id: "logs", label: { en: "Audit", "pt-BR": "Auditoria" }, icon: "list-tree", to: "/logs", group: "logs", profiles: ["admin"] },
      { id: "extra", label: { en: "Extra" }, icon: "unknown", to: "/acme/extra", profiles: ["admin", "infer"] },
    ],
    routes: [
      { path: "/logs", screen: "logs", profiles: ["admin"] },
      { path: "/acme/extra", screen: "extra", profiles: ["admin", "infer"] },
      { path: "/acme", screen: "root", profiles: ["admin"] },
    ],
    screens: { logs: { module: "a.js" }, extra: { module: "b.js" }, root: { module: "c.js" } },
    assets: ["a.js", "b.js", "c.js"],
  },
};

describe("plugin UI registry", () => {
  afterEach(() => setPluginUi([]));

  it("is empty without plugins: no route, no profile rule", () => {
    expect(resolvePluginRoute("/logs")).toBeNull();
    expect(pluginRouteProfiles("/logs")).toBeNull();
  });

  it("resolves the most specific route by segment prefix", () => {
    setPluginUi([plugin]);
    expect(resolvePluginRoute("/acme/extra/deep")?.route.screen).toBe("extra");
    expect(resolvePluginRoute("/acme")?.route.screen).toBe("root");
    expect(resolvePluginRoute("/acmex")).toBeNull();
    expect(resolvePluginRoute("/logs")?.version).toBe("2.0.0");
  });

  it("lets the manifest decide profiles, including infer", () => {
    setPluginUi([plugin]);
    expect(canAccessRoute("/acme/extra", "infer")).toBe(true);
    expect(canAccessRoute("/acme", "infer")).toBe(false);
    expect(canAccessRoute("/logs", "node")).toBe(false);
    expect(canAccessRoute("/logs", "admin")).toBe(true);
  });

  it("filters menu items by profile and localizes labels with an en floor", () => {
    setPluginUi([plugin]);
    expect(pluginMenuItems("infer").map((i) => i.id)).toEqual(["extra"]);
    expect(pluginLabel("plugin:acme:logs", "pt-BR")).toBe("Auditoria");
    expect(pluginLabel("plugin:acme:extra", "pt-BR")).toBe("Extra");
    expect(pluginLabel("plugin:acme:nope", "en")).toBeNull();
  });

  it("merges the menu: replaces the fallback leaf and appends new entries", () => {
    setPluginUi([plugin]);
    const admin = navForProfile("admin");
    const logsGroup = admin.find((s) => s.labelKey === "nav.logs");
    expect(logsGroup?.children?.map((c) => c.labelKey)).toContain("plugin:acme:logs");
    expect(logsGroup?.children?.map((c) => c.labelKey)).not.toContain("nav.requestLogs");
    expect(admin.some((s) => s.labelKey === "plugin:acme:extra" && s.to === "/acme/extra")).toBe(true);
    // Sem plugin de novo, o fallback do POM volta.
    setPluginUi([]);
    expect(navForProfile("admin").find((s) => s.labelKey === "nav.logs")?.children?.map((c) => c.labelKey)).toContain("nav.requestLogs");
  });

  it("never lets a plugin item show for a profile the manifest excludes", () => {
    setPluginUi([plugin]);
    const node = navForProfile("node");
    expect(JSON.stringify(node)).not.toContain("plugin:acme:extra");
  });
});
