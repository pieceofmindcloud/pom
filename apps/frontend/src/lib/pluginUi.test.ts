import { afterEach, describe, expect, it } from "vitest";
import { pluginAssetUrl, pluginLabel, pluginMenuItems, pluginRouteProfiles, pluginUiSnapshot, resolvePluginRoute, setPluginUi, type ActivePluginUi } from "./pluginUi";
import { canAccessRoute } from "./access";
import { navForProfile } from "../components/nav";

const plugin: ActivePluginUi = {
  code: "acme",
  version: "2.0.0",
  manifest: {
    schema: "pom-plugin-ui/v1",
    plugin_code: "acme",
    icon_image: "ui/icon.png",
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
    assets: ["a.js", "b.js", "c.js", "ui/icon.png"],
  },
};

const basePlugin: ActivePluginUi = {
  code: "base",
  version: "1.0.0",
  manifest: {
    schema: "pom-plugin-ui/v1",
    plugin_code: "base",
    menu: [
      { id: "plugin", label: { en: "Plugin" }, to: "/plugin-de-base", profiles: ["admin"] },
      { id: "example", label: { en: "Example" }, to: "/example", group: "logs", profiles: ["admin", "node"] },
    ],
    routes: [
      { path: "/plugin-de-base", screen: "plugin", profiles: ["admin"] },
      { path: "/example", screen: "example", profiles: ["admin", "node"] },
    ],
    screens: {
      plugin: { module: "ui/plugin.js" },
      example: { module: "ui/example.js" },
    },
    assets: ["ui/plugin.js", "ui/example.js"],
  },
};

const otherPlugin: ActivePluginUi = {
  code: "analytics",
  version: "1.0.0",
  manifest: {
    schema: "pom-plugin-ui/v1",
    plugin_code: "analytics",
    menu: [
      { id: "example", label: { en: "Analytics Example" }, to: "//example//", profiles: ["infer"] },
    ],
    routes: [{ path: "//example//", screen: "example", profiles: ["infer"] }],
    screens: { example: { module: "ui/example.js" } },
    assets: ["ui/example.js"],
  },
};

describe("plugin UI registry", () => {
  afterEach(() => setPluginUi([]));

  it("is empty without plugins: no route, no profile rule", () => {
    expect(resolvePluginRoute("/logs")).toBeNull();
    expect(pluginRouteProfiles("/logs")).toBeNull();
  });

  it("resolves the most specific route by segment prefix and claims an unambiguous POM extension", () => {
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

  it("accepts route roles without requiring legacy profiles", () => {
    const rolesOnly: ActivePluginUi = {
      ...basePlugin,
      code: "roles",
      manifest: {
        ...basePlugin.manifest,
        plugin_code: "roles",
        menu: [{ ...basePlugin.manifest.menu[0], to: "/roles-only", roles: ["infer"], profiles: undefined }],
        routes: [{ path: "/roles-only", screen: "plugin", roles: ["infer"] }],
      },
    };
    setPluginUi([rolesOnly]);

    expect(pluginUiSnapshot()[0]?.manifest.routes[0]).toMatchObject({
      path: "/roles-only",
      roles: ["infer"],
    });
    expect(canAccessRoute("/roles/roles-only", "infer")).toBe(true);
    expect(canAccessRoute("/roles/roles-only", "admin")).toBe(false);
  });

  it("uses each plugin namespace for matching and its displayed menu links", () => {
    setPluginUi([basePlugin, otherPlugin]);

    expect(pluginMenuItems("admin").map(({ code, id, to }) => [code, id, to])).toEqual([
      ["base", "plugin", "/base/plugin-de-base"],
      ["base", "example", "/base/example"],
    ]);
    expect(pluginMenuItems("infer").map(({ code, id, to }) => [code, id, to])).toEqual([
      ["analytics", "example", "/analytics/example"],
    ]);
    expect(resolvePluginRoute("/base/plugin-de-base")).toMatchObject({
      code: "base",
      route: { path: "/base/plugin-de-base", screen: "plugin" },
    });
    expect(resolvePluginRoute("/analytics/example/")).toMatchObject({
      code: "analytics",
      route: { path: "/analytics/example", screen: "example" },
    });
    expect(resolvePluginRoute("/example")).toBeNull();
    expect(resolvePluginRoute("/base/example")?.code).toBe("base");
    expect(canAccessRoute("/base/example", "node")).toBe(true);
    expect(canAccessRoute("/base/example", "infer")).toBe(false);
    expect(canAccessRoute("/analytics/example", "infer")).toBe(true);
    expect(canAccessRoute("/analytics/example", "admin")).toBe(false);

    const adminLinks = navForProfile("admin").flatMap((section) => [
      ...(section.to ? [{ labelKey: section.labelKey, to: section.to }] : []),
      ...(section.children ?? []).map(({ labelKey, to }) => ({ labelKey, to })),
    ]);
    expect(adminLinks).toContainEqual({ labelKey: "plugin:base:plugin", to: "/base/plugin-de-base" });
    expect(adminLinks).toContainEqual({ labelKey: "plugin:base:example", to: "/base/example" });
  });

  it("keeps the plugin PNG available for the navigation fallback", () => {
    setPluginUi([plugin]);
    expect(pluginMenuItems("admin")[0]?.iconImage).toBe("ui/icon.png");
  });

  it("normalizes repeated slashes and trailing slashes without leaving the plugin namespace", () => {
    const slashPlugin: ActivePluginUi = {
      ...basePlugin,
      manifest: {
        ...basePlugin.manifest,
        menu: [{ ...basePlugin.manifest.menu[1], to: "//nested///example/" }],
        routes: [{ ...basePlugin.manifest.routes[1], path: "//nested///example/" }],
      },
    };
    setPluginUi([slashPlugin]);

    expect(pluginMenuItems("admin")[0]?.to).toBe("/base/nested/example");
    expect(resolvePluginRoute("/base//nested/example/")?.route.path).toBe("/base/nested/example");
  });

  it("preserves screen, layout, asset and i18n metadata", () => {
    const metadataPlugin: ActivePluginUi = {
      ...basePlugin,
      manifest: {
        ...basePlugin.manifest,
        routes: [{ ...basePlugin.manifest.routes[0], full_bleed: true }],
        screens: {
          plugin: { module: "ui/plugin.js", export: "PluginScreen", styles: ["ui/plugin.css"] },
        },
        assets: ["ui/plugin.js", "ui/plugin.css", "i18n/en.json"],
        i18n: { en: "i18n/en.json" },
        menu: [basePlugin.manifest.menu[0]],
      },
    };
    setPluginUi([metadataPlugin]);

    expect(resolvePluginRoute("/base/plugin-de-base")).toMatchObject({
      route: { full_bleed: true },
      screen: { module: "ui/plugin.js", export: "PluginScreen", styles: ["ui/plugin.css"] },
    });
    expect(pluginUiSnapshot()[0]?.manifest.assets).toEqual(["ui/plugin.js", "ui/plugin.css", "i18n/en.json"]);
    expect(pluginUiSnapshot()[0]?.manifest.i18n).toEqual({ en: "i18n/en.json" });
    expect(pluginAssetUrl("base", "ui/plugin.js")).toBe("/api/ui/plugins/base/assets/ui/plugin.js");
  });

  it("ignores manifests with namespace escape paths and keeps native paths available", () => {
    const deepEncodedDot = Array.from({ length: 9 }).reduce(
      (value) => value.replaceAll("%", "%25"),
      "%2e",
    );
    const escapedPath = `/${deepEncodedDot}/${deepEncodedDot}/chat`;
    const escaped: ActivePluginUi = {
      ...basePlugin,
      manifest: {
        ...basePlugin.manifest,
        menu: [{ ...basePlugin.manifest.menu[0], to: escapedPath }],
        routes: [{ ...basePlugin.manifest.routes[0], path: escapedPath }],
      },
    };
    setPluginUi([escaped]);

    expect(pluginMenuItems("admin")).toEqual([]);
    expect(resolvePluginRoute("/chat")).toBeNull();
    expect(canAccessRoute("/chat", "node")).toBe(true);
  });

  it("ignores mismatched or invalid plugin codes", () => {
    const mismatched: ActivePluginUi = {
      ...basePlugin,
      code: "other",
    };
    const invalidCode: ActivePluginUi = {
      ...basePlugin,
      code: "../chat",
      manifest: { ...basePlugin.manifest, plugin_code: "../chat" },
    };
    setPluginUi([mismatched, invalidCode]);

    expect(pluginMenuItems("admin")).toEqual([]);
    expect(resolvePluginRoute("/base/plugin-de-base")).toBeNull();
  });

  it("filters menu items by profile and localizes labels with an en floor", () => {
    setPluginUi([plugin]);
    expect(pluginMenuItems("infer").map((i) => i.id)).toEqual(["extra"]);
    expect(pluginLabel("plugin:acme:logs", "pt-BR")).toBe("Auditoria");
    expect(pluginLabel("plugin:acme:extra", "pt-BR")).toBe("Extra");
    expect(pluginLabel("plugin:acme:nope", "en")).toBeNull();
  });

  it("applies menu roles to both visibility and direct route access, defaulting missing roles to all profiles", () => {
    const rolesPlugin: ActivePluginUi = {
      code: "roles",
      manifest: {
        schema: "pom-plugin-ui/v1",
        plugin_code: "roles",
        menu: [
          { id: "admin", label: { en: "Admin" }, to: "/admin", roles: ["admin"] },
          { id: "node", label: { en: "Node" }, to: "/node", roles: ["node"] },
          { id: "infer", label: { en: "Infer" }, to: "/infer", roles: ["infer"] },
          { id: "all", label: { en: "All" }, to: "/all" },
        ],
        routes: [
          { path: "/admin", screen: "admin", profiles: ["admin", "node", "infer"] },
          { path: "/node", screen: "node", profiles: ["admin", "node", "infer"] },
          { path: "/infer", screen: "infer", profiles: ["admin", "node", "infer"] },
          { path: "/all", screen: "all", profiles: ["admin", "node", "infer"] },
        ],
        screens: {
          admin: { module: "admin.js" },
          node: { module: "node.js" },
          infer: { module: "infer.js" },
          all: { module: "all.js" },
        },
        assets: ["admin.js", "node.js", "infer.js", "all.js"],
      },
    };
    setPluginUi([rolesPlugin]);

    for (const [profile, ownMenu, ownRoute, otherRoutes] of [
      ["admin", "admin", "/roles/admin", ["/roles/node", "/roles/infer"]],
      ["node", "node", "/roles/node", ["/roles/admin", "/roles/infer"]],
      ["infer", "infer", "/roles/infer", ["/roles/admin", "/roles/node"]],
    ] as const) {
      expect(pluginMenuItems(profile).map(({ id }) => id)).toContain(ownMenu);
      expect(pluginMenuItems(profile).map(({ id }) => id)).toContain("all");
      const navKeys = navForProfile(profile).flatMap((section) => [
        ...(section.to ? [section.labelKey] : []),
        ...(section.children ?? []).map(({ labelKey }) => labelKey),
      ]);
      expect(navKeys).toContain(`plugin:roles:${ownMenu}`);
      expect(navKeys).toContain("plugin:roles:all");
      for (const otherMenu of ["admin", "node", "infer"] as const) {
        if (otherMenu !== ownMenu) expect(navKeys).not.toContain(`plugin:roles:${otherMenu}`);
      }
      expect(canAccessRoute(ownRoute, profile)).toBe(true);
      for (const path of otherRoutes) expect(canAccessRoute(path, profile)).toBe(false);
    }
  });

  it("replaces claimed native links and inserts new plugin menus before Documentation in manifest order", () => {
    setPluginUi([plugin]);
    const admin = navForProfile("admin");
    const logsGroup = admin.find((s) => s.labelKey === "nav.logs");
    expect(logsGroup?.children?.map((c) => c.labelKey)).toContain("plugin:acme:logs");
    expect(logsGroup?.children?.map((c) => c.labelKey)).not.toContain("nav.requestLogs");
    expect(logsGroup?.children?.find((c) => c.labelKey === "plugin:acme:logs")?.to).toBe("/logs");
    const docsIndex = admin.findIndex((s) => s.labelKey === "nav.documentation");
    expect(admin[docsIndex - 1]).toMatchObject({ labelKey: "plugin:acme:extra", to: "/acme/extra" });
    // Sem plugin de novo, o fallback do POM volta.
    setPluginUi([]);
    expect(navForProfile("admin").find((s) => s.labelKey === "nav.logs")?.children?.map((c) => c.labelKey)).toContain("nav.requestLogs");
  });

  it("replaces both native Enterprise menu destinations when claimed by plugin manifests", () => {
    const statisticsPlugin: ActivePluginUi = {
      code: "enterprise",
      manifest: {
        schema: "pom-plugin-ui/v1",
        plugin_code: "enterprise",
        menu: [{ id: "stats", label: { en: "Statistics" }, to: "/estatisticas", roles: ["admin"] }],
        routes: [{ path: "/estatisticas", screen: "stats", profiles: ["admin"] }],
        screens: { stats: { module: "stats.js" } },
        assets: ["stats.js"],
      },
    };
    setPluginUi([plugin, statisticsPlugin]);
    const navigation = navForProfile("admin");
    const logs = navigation.find((section) => section.labelKey === "nav.logs");
    const statistics = navigation.find((section) => section.to === "/estatisticas");

    expect(logs?.children?.map(({ labelKey }) => labelKey)).toContain("plugin:acme:logs");
    expect(logs?.children?.map(({ labelKey }) => labelKey)).not.toContain("nav.requestLogs");
    expect(statistics?.labelKey).toBe("plugin:enterprise:stats");
    expect(resolvePluginRoute("/estatisticas")?.code).toBe("enterprise");
  });

  it("does not let two plugins claim the same POM extension route", () => {
    const secondLogsPlugin: ActivePluginUi = {
      ...plugin,
      code: "other",
      manifest: {
        ...plugin.manifest,
        plugin_code: "other",
        menu: [{ ...plugin.manifest.menu[0], to: "/logs" }],
        routes: [{ ...plugin.manifest.routes[0], path: "/logs" }],
      },
    };
    setPluginUi([plugin, secondLogsPlugin]);

    expect(resolvePluginRoute("/logs")).toBeNull();
    expect(resolvePluginRoute("/acme/logs")?.code).toBe("acme");
    expect(resolvePluginRoute("/other/logs")?.code).toBe("other");
    expect(pluginMenuItems("admin").map(({ to }) => to)).toEqual([
      "/acme/logs",
      "/acme/extra",
      "/other/logs",
    ]);
  });

  it("places Base menus at the end of navigation in manifest order", () => {
    setPluginUi([basePlugin]);
    const navigation = navForProfile("admin");
    const docsIndex = navigation.findIndex((section) => section.labelKey === "nav.documentation");

    expect(navigation.slice(docsIndex - 2, docsIndex).map(({ labelKey, to }) => [labelKey, to])).toEqual([
      ["plugin:base:plugin", "/base/plugin-de-base"],
      ["plugin:base:example", "/base/example"],
    ]);
    expect(navigation.find((section) => section.labelKey === "nav.logs")?.children?.some(
      ({ labelKey }) => labelKey === "plugin:base:example",
    )).toBe(false);
  });

  it("never lets a plugin item show for a profile the manifest excludes", () => {
    setPluginUi([plugin]);
    const node = navForProfile("node");
    expect(JSON.stringify(node)).not.toContain("plugin:acme:extra");
  });
});
