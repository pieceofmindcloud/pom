import { afterEach, describe, expect, it } from "vitest";
import { pluginAssetUrl, pluginDisplayName, pluginSlot, pluginLabel, pluginMenuItems, pluginRouteProfiles, pluginUiSnapshot, resolvePluginRoute, setPluginUi, type ActivePluginUi } from "./pluginUi";
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
      { id: "extra", label: { en: "Extra" }, icon: "unknown", to: "/acme/extra", profiles: ["admin", "visitor"] },
    ],
    routes: [
      { path: "/logs", screen: "logs", profiles: ["admin"] },
      { path: "/acme/extra", screen: "extra", profiles: ["admin", "visitor"] },
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
      { id: "example", label: { en: "Example" }, to: "/example", group: "logs", profiles: ["admin", "worker"] },
    ],
    routes: [
      { path: "/plugin-de-base", screen: "plugin", profiles: ["admin"] },
      { path: "/example", screen: "example", profiles: ["admin", "worker"] },
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
      { id: "example", label: { en: "Analytics Example" }, to: "//example//", profiles: ["visitor"] },
    ],
    routes: [{ path: "//example//", screen: "example", profiles: ["visitor"] }],
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

  it("makes plugin routes available to every profile regardless of manifest roles", () => {
    setPluginUi([plugin]);
    expect(canAccessRoute("/acme/extra", "admin")).toBe(true);
    expect(canAccessRoute("/acme/extra", "worker")).toBe(true);
    expect(canAccessRoute("/acme/extra", "visitor")).toBe(true);
    expect(canAccessRoute("/acme", "visitor")).toBe(true);
    expect(canAccessRoute("/logs", "worker")).toBe(true);
    expect(canAccessRoute("/logs", "admin")).toBe(true);
    expect(canAccessRoute("/logs", "visitor")).toBe(false, "the POM still denies native logs to infer");
  });

  it("accepts route roles without requiring legacy profiles", () => {
    const rolesOnly: ActivePluginUi = {
      ...basePlugin,
      code: "roles",
      manifest: {
        ...basePlugin.manifest,
        plugin_code: "roles",
        menu: [{ ...basePlugin.manifest.menu[0], to: "/roles-only", roles: ["visitor"], profiles: undefined }],
        routes: [{ path: "/roles-only", screen: "plugin", roles: ["visitor"] }],
      },
    };
    setPluginUi([rolesOnly]);

    expect(pluginUiSnapshot()[0]?.manifest.routes[0]).toMatchObject({
      path: "/roles-only",
      roles: ["visitor"],
    });
    expect(canAccessRoute("/roles/roles-only", "visitor")).toBe(true);
    expect(canAccessRoute("/roles/roles-only", "worker")).toBe(true);
    expect(canAccessRoute("/roles/roles-only", "admin")).toBe(true);
  });

  it("uses each plugin namespace for matching and its displayed menu links", () => {
    setPluginUi([basePlugin, otherPlugin]);

    const allPluginMenus = [
      ["base", "plugin", "/base/plugin-de-base"],
      ["base", "example", "/base/example"],
      ["analytics", "example", "/analytics/example"],
    ];
    for (const profile of ["admin", "worker", "visitor"] as const) {
      expect(pluginMenuItems(profile).map(({ code, id, to }) => [code, id, to])).toEqual(allPluginMenus);
    }
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
    expect(canAccessRoute("/base/example", "worker")).toBe(true);
    expect(canAccessRoute("/base/example", "visitor")).toBe(true);
    expect(canAccessRoute("/analytics/example", "visitor")).toBe(true);
    expect(canAccessRoute("/analytics/example", "admin")).toBe(true);

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
    expect(canAccessRoute("/chat", "worker")).toBe(true);
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

  it("exposes plugin menus across profiles while retaining native infer restrictions", () => {
    setPluginUi([plugin]);
    expect(pluginMenuItems("worker").map((i) => i.id)).toEqual(["logs", "extra"]);
    expect(pluginMenuItems("visitor").map((i) => i.id)).toEqual(["logs", "extra"]);
    const inferNavigation = navForProfile("visitor").flatMap((section) => [
      ...(section.to ? [section.labelKey] : []),
      ...(section.children ?? []).map((child) => child.labelKey),
    ]);
    expect(inferNavigation).toContain("plugin:acme:extra");
    expect(inferNavigation).not.toContain("plugin:acme:logs");
    expect(pluginLabel("plugin:acme:logs", "pt-BR")).toBe("Auditoria");
    expect(pluginLabel("plugin:acme:extra", "pt-BR")).toBe("Extra");
    expect(pluginLabel("plugin:acme:nope", "en")).toBeNull();
  });

  it("keeps manifest role metadata but exposes plugin menus and routes to every profile", () => {
    const rolesPlugin: ActivePluginUi = {
      code: "roles",
      manifest: {
        schema: "pom-plugin-ui/v1",
        plugin_code: "roles",
        menu: [
          { id: "admin", label: { en: "Admin" }, to: "/admin", roles: ["admin"] },
          { id: "node", label: { en: "Node" }, to: "/node", roles: ["worker"] },
          { id: "infer", label: { en: "Infer" }, to: "/infer", roles: ["visitor"] },
          { id: "all", label: { en: "All" }, to: "/all" },
        ],
        routes: [
          { path: "/admin", screen: "admin", profiles: ["admin", "worker", "visitor"] },
          { path: "/node", screen: "node", profiles: ["admin", "worker", "visitor"] },
          { path: "/infer", screen: "infer", profiles: ["admin", "worker", "visitor"] },
          { path: "/all", screen: "all", profiles: ["admin", "worker", "visitor"] },
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

    for (const profile of ["admin", "worker", "visitor"] as const) {
      const menuIds = pluginMenuItems(profile).map(({ id }) => id);
      expect(menuIds).toEqual(["admin", "node", "infer", "all"]);
      const navKeys = navForProfile(profile).flatMap((section) => [
        ...(section.to ? [section.labelKey] : []),
        ...(section.children ?? []).map(({ labelKey }) => labelKey),
      ]);
      for (const menu of ["admin", "node", "infer", "all"] as const) {
        expect(navKeys).toContain(`plugin:roles:${menu}`);
        expect(canAccessRoute(`/roles/${menu}`, profile)).toBe(true);
      }
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
    // Without a plugin again, POM's fallback comes back.
    setPluginUi([]);
    expect(navForProfile("admin").find((s) => s.labelKey === "nav.logs")?.children?.map((c) => c.labelKey)).toContain("nav.requestLogs");
  });

  it("replaces both native Enterprise menu destinations when claimed by plugin manifests", () => {
    const statisticsPlugin: ActivePluginUi = {
      code: "logs",
      manifest: {
        schema: "pom-plugin-ui/v1",
        plugin_code: "logs",
        menu: [{ id: "stats", label: { en: "Statistics" }, to: "/statistics", roles: ["admin"] }],
        routes: [{ path: "/statistics", screen: "stats", profiles: ["admin"] }],
        screens: { stats: { module: "stats.js" } },
        assets: ["stats.js"],
      },
    };
    setPluginUi([plugin, statisticsPlugin]);
    const navigation = navForProfile("admin");
    const logs = navigation.find((section) => section.labelKey === "nav.logs");
    const statistics = navigation.find((section) => section.to === "/statistics");

    expect(logs?.children?.map(({ labelKey }) => labelKey)).toContain("plugin:acme:logs");
    expect(logs?.children?.map(({ labelKey }) => labelKey)).not.toContain("nav.requestLogs");
    expect(statistics?.labelKey).toBe("plugin:logs:stats");
    expect(resolvePluginRoute("/statistics")?.code).toBe("logs");
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

  it("shows plugin items to every profile even when the manifest lists only admin", () => {
    setPluginUi([plugin]);
    const node = navForProfile("worker");
    expect(JSON.stringify(node)).toContain("plugin:acme:extra");
    expect(canAccessRoute("/acme/extra", "worker")).toBe(true);
  });

  it("registers menu children as a sidebar section available to every profile", () => {
    setPluginUi([{
      code: "base",
      version: "0.2.0",
      manifest: {
        schema: "pom-plugin-ui/v1",
        plugin_code: "base",
        icon_image: "ui/icon.png",
        description: { en: "Tutorial", "pt-BR": "Tutorial" },
        menu: [{
          id: "base",
          label: { en: "POM - Plugins" },
          icon: "blocks",
          to: "/plugin",
          children: [
            { id: "plugin", label: { en: "Plugin", "pt-BR": "Plugin" }, icon: "book-open", to: "/plugin", roles: ["admin"] },
            { id: "projects", label: { en: "Projects", "pt-BR": "Projetos" }, icon: "folder-tree", to: "/projects" },
          ],
        }],
        routes: [
          { path: "/plugin", screen: "plugin", roles: ["admin"] },
          { path: "/projects", screen: "projects" },
        ],
        screens: { plugin: { module: "ui/screens.js" }, projects: { module: "ui/screens.js" } },
        assets: ["ui/screens.js", "ui/icon.png"],
      },
    }]);

    const [admin] = pluginMenuItems("admin");
    expect(admin.children?.map((child) => child.to)).toEqual(["/base/plugin", "/base/projects"]);
    const [node] = pluginMenuItems("worker");
    expect(node.children?.map((child) => child.id)).toEqual(["plugin", "projects"]);
    expect(pluginLabel("plugin:base:projects", "pt-BR")).toBe("Projetos");
    expect(pluginDisplayName("base", "en")).toBe("POM - Plugins");

    const section = navForProfile("admin").find((entry) => entry.labelKey === "plugin:base:base");
    expect(section?.to).toBeUndefined();
    expect(section?.children?.map((child) => child.to)).toEqual(["/base/plugin", "/base/projects"]);
    expect(canAccessRoute("/base/projects", "worker")).toBe(true);
    expect(canAccessRoute("/base/plugin", "worker")).toBe(true);
  });

  it("rejects a manifest whose child id repeats or points outside the declared routes", () => {
    const manifest = (children: unknown[]) => ({
      code: "base",
      manifest: {
        schema: "pom-plugin-ui/v1",
        plugin_code: "base",
        menu: [{ id: "base", label: { en: "Base" }, to: "/plugin", children }],
        routes: [{ path: "/plugin", screen: "plugin" }],
        screens: { plugin: { module: "ui/screens.js" } },
        assets: ["ui/screens.js"],
      },
    }) as unknown as ActivePluginUi;
    setPluginUi([manifest([{ id: "base", label: { en: "Again" }, to: "/plugin" }])]);
    expect(pluginUiSnapshot()).toHaveLength(0);
    setPluginUi([manifest([{ id: "other", label: { en: "Other" }, to: "/missing" }])]);
    expect(pluginUiSnapshot()).toHaveLength(0);
  });
});

describe("plugin slots", () => {
  afterEach(() => setPluginUi([]));

  it("resolves a slot to the declared screen and keeps it in the snapshot", () => {
    setPluginUi([{ ...plugin, manifest: { ...plugin.manifest, slots: { "chat.telemetry": "extra" } } }]);

    expect(pluginSlot("chat.telemetry")).toEqual({
      code: "acme",
      slot: "chat.telemetry",
      screen: { module: "b.js" },
      version: "2.0.0",
    });
    expect(pluginSlot("dashboard.card")).toBeNull();
    expect(pluginUiSnapshot()[0].manifest.slots).toEqual({ "chat.telemetry": "extra" });
  });

  it("drops a manifest whose slot points at an undeclared screen or has a bad id", () => {
    for (const slots of [{ "chat.telemetry": "missing" }, { "Chat Telemetry": "extra" }]) {
      setPluginUi([{ ...plugin, manifest: { ...plugin.manifest, slots } }]);
      expect(pluginSlot("chat.telemetry")).toBeNull();
      expect(pluginUiSnapshot()).toHaveLength(0);
    }
  });

  it("gives the slot to the first active plugin that claims it", () => {
    setPluginUi([
      { ...basePlugin, manifest: { ...basePlugin.manifest, slots: { "chat.telemetry": "plugin" } } },
      { ...plugin, manifest: { ...plugin.manifest, slots: { "chat.telemetry": "extra" } } },
    ]);

    expect(pluginSlot("chat.telemetry")?.code).toBe("base");
  });
});
