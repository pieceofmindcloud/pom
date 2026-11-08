import { describe, expect, it } from "vitest";
import { NAV, navFooterForProfile, navForProfile } from "./nav";

describe("navigation structure", () => {
  it("puts Hugging Face model search first under Models", () => {
    const models = NAV.find((section) => section.labelKey === "nav.models");

    expect(models?.children?.map((child) => child.to)).toEqual([
      "/models/remote",
      "/models",
    ]);
    expect(models?.children?.[0]?.labelKey).toBe("nav.remoteModels");
    expect(NAV.some((section) => section.labelKey === "nav.shardedModels")).toBe(false);
    expect(NAV.flatMap((section) => section.children ?? []).some(
      (child) => child.to === "/shards/progress",
    )).toBe(false);
  });

  it("nests network deployment and history under Deployments", () => {
    const deployments = NAV.find((section) => section.labelKey === "nav.deployments");

    expect(deployments?.to).toBeUndefined();
    expect(deployments?.children?.map((child) => [child.labelKey, child.to])).toEqual([
      ["nav.deploy", "/deploy"],
      ["nav.history", "/deployments"],
    ]);
    expect(NAV.some((section) => section.labelKey === "nav.deploy")).toBe(false);
  });

  it("groups request and deployment logs under Logs", () => {
    const logs = NAV.find((section) => section.labelKey === "nav.logs");

    expect(logs?.to).toBeUndefined();
    expect(logs?.children?.map((child) => [child.labelKey, child.to])).toEqual([
      ["nav.requestLogs", "/logs"],
      ["nav.deploymentLogs", "/deployments/logs"],
    ]);
  });

  it("gives statistics its own top-level destination after logs", () => {
    const labels = NAV.map((section) => section.labelKey);
    expect(labels.indexOf("nav.statistics")).toBeGreaterThan(labels.indexOf("nav.logs"));
    expect(NAV.find((section) => section.labelKey === "nav.statistics")).toMatchObject({
      to: "/statistics",
    });
  });

  it("places Preferences under Settings", () => {
    const settings = NAV.find((section) => section.labelKey === "nav.settings");

    expect(settings?.children?.map((child) => [child.labelKey, child.to])).toContainEqual([
      "nav.preferences",
      "/config/preferences",
    ]);
  });

  it("places About as the final Settings item", () => {
    const settings = NAV.find((section) => section.labelKey === "nav.settings");

    expect(settings?.children?.at(-1)).toMatchObject({
      labelKey: "nav.about",
      to: "/config/about",
    });
  });

  it("gives the plugin catalog its own menu item, right before Documentation", () => {
    // The catalog is a storefront: it has a place of its own in the menu, outside
    // Settings, and the menus of the installed plugins come right after it.
    const labels = NAV.map((section) => section.labelKey);
    const plugins = NAV.find((section) => section.labelKey === "nav.plugins");
    expect(plugins?.to).toBe("/plugins");
    expect(labels.indexOf("nav.plugins")).toBe(labels.indexOf("nav.documentation") - 1);

    const settings = NAV.find((section) => section.labelKey === "nav.settings");
    const entries = settings?.children?.map((child) => [child.labelKey, child.to]);
    expect(entries).not.toContainEqual(["nav.plugins", "/plugins"]);
    expect(entries).toContainEqual(["nav.about", "/config/about"]);
  });

  it("keeps network chat separate and removes wizard and connection keys from the menu", () => {
    expect(NAV.map((section) => section.labelKey)).toContain("nav.networkChat");
    expect(NAV.flatMap((section) => section.children ?? []).map((child) => child.labelKey)).not.toContain("nav.setupWizard");
    expect(NAV.flatMap((section) => section.children ?? []).map((child) => child.labelKey)).not.toContain("nav.connectionKeys");
  });
});

describe("navForProfile", () => {
  it("shows the administrator the whole menu", () => {
    // Without the Business Pack plugin, Logs and Statistics fold into its one entry.
    const labels = navForProfile("admin").map((s) => s.labelKey);
    expect(labels).toEqual(NAV.map((s) => s.labelKey)
      .filter((label) => label !== "nav.statistics")
      .map((label) => label === "nav.logs" ? "nav.businessPack" : label));
    expect(navFooterForProfile("admin")).toEqual([]);
    expect(navFooterForProfile("worker")).toEqual([]);
  });

  it("removes deployment from the node, but preserves the Enterprise entries for discovery", () => {
    const labels = navForProfile("worker").map((s) => s.labelKey);
    expect(labels).not.toContain("nav.deploy");
    expect(labels).not.toContain("nav.deployments");
    expect(labels).toContain("nav.businessPack");
  });

  it("keeps the Enterprise surfaces for admin and node when the license is inactive", () => {
    expect(navForProfile("visitor", false).map((s) => s.labelKey)).not.toContain("nav.businessPack");
    expect(navForProfile("visitor", false).map((s) => s.labelKey)).not.toContain("nav.statistics");
    for (const profile of ["admin", "worker"] as const) {
      const sections = navForProfile(profile, false);
      const labels = sections.map((s) => s.labelKey);
      // One entry for the one purchase: logs, statistics and quota.
      expect(labels, profile).not.toContain("nav.logs");
      expect(labels, profile).not.toContain("nav.statistics");
      expect(sections.find((s) => s.labelKey === "nav.businessPack"), profile)
        .toMatchObject({ to: "/logs" });
    }
  });

  it("preserves on the node everything nobody restricted", () => {
    const sections = navForProfile("worker");
    const labels = sections.map((s) => s.labelKey);
    expect(labels).toContain("nav.dashboard");
    expect(labels).toContain("nav.chat");
    expect(labels).toContain("nav.models");
    expect(labels).toContain("nav.settings");
    const models = sections.find((s) => s.labelKey === "nav.models");
    expect(models?.children?.map((c) => c.to)).toEqual([
      "/models/remote",
      "/models",
    ]);
  });

  it("keeps Network and Settings as main menus", () => {
    const settings = navForProfile("worker").find((section) => section.labelKey === "nav.settings");
    const network = navForProfile("worker").find((section) => section.labelKey === "nav.network");
    expect(network?.children?.map((child) => child.to)).toEqual([
      "/config/network",
      "/config/network/participants",
      "/config/my-machine",
      "/config/administration",
      "/config/sharing",
      "/config/keys/api",
    ]);
    expect(network?.children?.map((child) => child.labelKey)).toEqual([
      "nav.networkInfo",
      "nav.networkParticipants",
      "nav.myMachine",
      "nav.administrators",
      "nav.sharing",
      "nav.apiKeys",
    ]);
    expect(settings?.children?.map((child) => child.to)).toEqual([
      "/config/preferences",
      "/config/storage",
      "/config/preferences/admin",
      "/config/about",
    ]);

    const inferSettings = navForProfile("visitor");
    expect(inferSettings.some((section) => section.labelKey === "nav.network")).toBe(true);
    expect(inferSettings.flatMap((section) => section.children ?? []).some((child) => child.to === "/config/network/participants")).toBe(false);
  });

  it("leaves the network in the main menu of infer and moves docs/config to the footer", () => {
    expect(navForProfile("visitor").map((section) => section.labelKey)).toEqual([
      "nav.publicNetworks",
      "nav.networkChat",
      "nav.plugins",
      "nav.network",
    ]);
    expect(navFooterForProfile("visitor").map((item) => [item.labelKey, item.to])).toEqual([
      ["nav.documentation", "/documentation"],
      ["nav.settings", "/config/preferences"],
    ]);
  });

  it("offers sharing to the three profiles", () => {
    for (const profile of ["admin", "worker", "visitor"] as const) {
      const destinations = navForProfile(profile).flatMap((section) => [
        section.to,
        ...(section.children ?? []).map((child) => child.to),
      ]);
      expect(destinations).toContain("/config/sharing");
    }
  });

  /** Filtering must not rewrite `NAV`: the other profile's menu would come out pruned. */
  it("does not modify the source table", () => {
    const before = JSON.stringify(NAV.map((s) => (s.children ?? []).map((c) => c.to)));
    navForProfile("worker");
    expect(JSON.stringify(NAV.map((s) => (s.children ?? []).map((c) => c.to)))).toBe(before);
  });
});
