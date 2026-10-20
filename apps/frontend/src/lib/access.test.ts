import { describe, expect, it } from "vitest";
import { canAccessRoute, canAccessEnterprise, canSeePeerDetails, homeRoute, routeProfiles } from "./access";

describe("canAccessRoute", () => {
  it("lets the administrator into the cluster operation screens", () => {
    for (const path of ["/deploy", "/deployments", "/logs"]) {
      expect(canAccessRoute(path, "admin")).toBe(true);
    }
  });

  it("opens a plugin's logs to a visitor, never the fallback or the deployment logs", () => {
    expect(canAccessRoute("/logs", "visitor")).toBe(false);
    expect(canAccessRoute("/statistics", "visitor")).toBe(false);
    expect(canAccessRoute("/deployments/logs", "visitor")).toBe(false);
  });

  it("closes deployment to the node profile, but keeps the Enterprise screens navigable", () => {
    for (const path of ["/deploy", "/deployments"]) {
      expect(canAccessRoute(path, "worker")).toBe(false);
    }
    expect(canAccessRoute("/logs", "worker")).toBe(true);
    expect(canAccessRoute("/statistics", "worker")).toBe(true);
  });

  it("lets through every route nobody restricted", () => {
    for (const path of ["/", "/chat", "/models", "/shards", "/config/network", "/inexistente"]) {
      expect(canAccessRoute(path, "worker")).toBe(true);
      expect(canAccessRoute(path, "admin")).toBe(true);
    }
  });

  /**
   * The restriction matches a whole segment. If it matched by raw `startsWith`, a future
   * route called `/deploy-draft` would be born closed with nobody having
   * decided that - and the opposite, a child of `/logs`, would stay open.
   */
  it("matches by segment, not by text prefix", () => {
    expect(canAccessRoute("/deployx", "worker")).toBe(true);
    expect(canAccessRoute("/logs-antigos", "worker")).toBe(true);
    expect(canAccessRoute("/logs/qualquer-filha", "worker")).toBe(true);
  });

  it("says a free route has no profile list", () => {
    expect(routeProfiles("/models")).toBeNull();
    expect(routeProfiles("/deploy")).toEqual(["admin"]);
  });

  it("sends each profile to a home screen it can open", () => {
    // The `/` is the Dashboard: infer would fall into the refusal, which is a dead end.
    expect(homeRoute("visitor")).toBe("/chat");
    expect(canAccessRoute(homeRoute("visitor"), "infer")).toBe(true);
    expect(homeRoute("admin")).toBe("/");
    expect(homeRoute("worker")).toBe("/");
  });

  /**
   * The trust-root screen is the work that precedes authority: an
   * `admin` node yet to be anchored has no admin capability yet, and whoever receives the
   * anchor is usually `node` or `infer`. Closing it to some profile would leave the
   * operator with no path precisely where they need it.
   */
  it("opens the trust root for the three profiles", () => {
    for (const profile of ["admin", "worker", "visitor"] as const) {
      expect(canAccessRoute("/config/trust", profile)).toBe(true);
    }
    expect(routeProfiles("/config/trust")).toBeNull();
  });

  it("keeps infer on the local network, conversation, documentation and its key", () => {
    expect(canAccessRoute("/chat", "visitor")).toBe(true);
    expect(canAccessRoute("/network-chat", "visitor")).toBe(true);
    expect(canAccessRoute("/config/keys/api", "visitor")).toBe(true);
    expect(canAccessRoute("/config/network", "visitor")).toBe(true);
    expect(canAccessRoute("/documentation", "visitor")).toBe(true);
    expect(canAccessRoute("/config/trust", "visitor")).toBe(true);
    for (const path of ["/", "/models", "/deploy", "/config/keys/connection", "/config/network/participants", "/config/preferences/admin"]) {
      expect(canAccessRoute(path, "visitor")).toBe(false);
    }
    // Logs (requests and deployment) and statistics are the network's
    // operation: never offered to an inference node.
    for (const path of ["/logs", "/logs/qualquer-filha", "/statistics", "/deployments/logs"]) {
      expect(canAccessRoute(path, "visitor"), path).toBe(false);
    }
  });

  it("shows the cluster key only to the admin", () => {
    expect(canAccessRoute("/config/keys/connection", "admin")).toBe(true);
    expect(canAccessRoute("/config/keys/connection", "worker")).toBe(false);
    expect(canAccessRoute("/config/keys/connection", "visitor")).toBe(false);
  });

  it("keeps participants and administrative preferences in the current profiles", () => {
    expect(canAccessRoute("/config/network/participants", "admin")).toBe(true);
    expect(canAccessRoute("/config/network/participants", "worker")).toBe(true);
    expect(canAccessRoute("/config/network/participants", "visitor")).toBe(false);
    expect(canAccessRoute("/config/preferences/admin", "admin")).toBe(true);
    expect(canAccessRoute("/config/preferences/admin", "worker")).toBe(true);
  });

  /**
   * The Dashboard's Details are cluster operation: each machine's internal
   * state, release a slice, remove from the pipeline, leave the network. The `node` opens
   * the Dashboard and gets the Flow - and where there is only one view, there is nothing to
   * choose.
   */
  it("reserves the machine-by-machine detail of the Dashboard for the administrator", () => {
    expect(canSeePeerDetails("admin")).toBe(true);
    expect(canSeePeerDetails("worker")).toBe(false);
    expect(canSeePeerDetails("visitor")).toBe(false);
    // The Dashboard itself stays open to `node`: what changes is the view.
    expect(canAccessRoute("/", "worker")).toBe(true);
  });
});

describe("canAccessEnterprise", () => {
  it("keeps request logs and statistics navigable when the lease is inactive", () => {
    expect(canAccessEnterprise("/logs", false)).toBe(true);
    expect(canAccessEnterprise("/statistics", false)).toBe(true);
  });

  it("keeps Enterprise routes available when the lease is active", () => {
    expect(canAccessEnterprise("/logs", true)).toBe(true);
    expect(canAccessEnterprise("/statistics", true)).toBe(true);
  });
});
