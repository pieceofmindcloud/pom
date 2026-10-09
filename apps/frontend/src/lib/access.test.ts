import { describe, expect, it } from "vitest";
import { canAccessRoute, canAccessEnterprise, canSeePeerDetails, homeRoute, routeProfiles } from "./access";

describe("canAccessRoute", () => {
  it("lets the administrator into the cluster operation screens", () => {
    for (const path of ["/deploy", "/deployments", "/logs"]) {
      expect(canAccessRoute(path, "admin")).toBe(true);
    }
  });

  it("closes deployment to the node profile, but keeps the Enterprise screens navigable", () => {
    for (const path of ["/deploy", "/deployments"]) {
      expect(canAccessRoute(path, "node")).toBe(false);
    }
    expect(canAccessRoute("/logs", "node")).toBe(true);
    expect(canAccessRoute("/statistics", "node")).toBe(true);
  });

  it("lets through every route nobody restricted", () => {
    for (const path of ["/", "/chat", "/models", "/shards", "/config/network", "/inexistente"]) {
      expect(canAccessRoute(path, "node")).toBe(true);
      expect(canAccessRoute(path, "admin")).toBe(true);
    }
  });

  /**
   * The restriction matches a whole segment. If it matched by raw `startsWith`, a future
   * route called `/deploy-draft` would be born closed with nobody having
   * decided that - and the opposite, a child of `/logs`, would stay open.
   */
  it("matches by segment, not by text prefix", () => {
    expect(canAccessRoute("/deployx", "node")).toBe(true);
    expect(canAccessRoute("/logs-antigos", "node")).toBe(true);
    expect(canAccessRoute("/logs/qualquer-filha", "node")).toBe(true);
  });

  it("says a free route has no profile list", () => {
    expect(routeProfiles("/models")).toBeNull();
    expect(routeProfiles("/deploy")).toEqual(["admin"]);
  });

  it("sends each profile to a home screen it can open", () => {
    // The `/` is the Dashboard: infer would fall into the refusal, which is a dead end.
    expect(homeRoute("infer")).toBe("/chat");
    expect(canAccessRoute(homeRoute("infer"), "infer")).toBe(true);
    expect(homeRoute("admin")).toBe("/");
    expect(homeRoute("node")).toBe("/");
  });

  /**
   * The trust-root screen is the work that precedes authority: an
   * `admin` node yet to be anchored has no admin capability yet, and whoever receives the
   * anchor is usually `node` or `infer`. Closing it to some profile would leave the
   * operator with no path precisely where they need it.
   */
  it("opens the trust root for the three profiles", () => {
    for (const profile of ["admin", "node", "infer"] as const) {
      expect(canAccessRoute("/config/trust", profile)).toBe(true);
    }
    expect(routeProfiles("/config/trust")).toBeNull();
  });

  it("keeps infer on the local network, conversation, documentation and its key", () => {
    expect(canAccessRoute("/chat", "infer")).toBe(true);
    expect(canAccessRoute("/network-chat", "infer")).toBe(true);
    expect(canAccessRoute("/config/keys/api", "infer")).toBe(true);
    expect(canAccessRoute("/config/network", "infer")).toBe(true);
    expect(canAccessRoute("/documentation", "infer")).toBe(true);
    expect(canAccessRoute("/config/trust", "infer")).toBe(true);
    for (const path of ["/", "/models", "/deploy", "/config/keys/connection", "/config/network/participants", "/config/preferences/admin"]) {
      expect(canAccessRoute(path, "infer")).toBe(false);
    }
    // Logs (requests and deployment) and statistics are the network's
    // operation: never offered to an inference node.
    for (const path of ["/logs", "/logs/qualquer-filha", "/statistics", "/deployments/logs"]) {
      expect(canAccessRoute(path, "infer"), path).toBe(false);
    }
  });

  it("shows the cluster key only to the admin", () => {
    expect(canAccessRoute("/config/keys/connection", "admin")).toBe(true);
    expect(canAccessRoute("/config/keys/connection", "node")).toBe(false);
    expect(canAccessRoute("/config/keys/connection", "infer")).toBe(false);
  });

  it("keeps participants and administrative preferences in the current profiles", () => {
    expect(canAccessRoute("/config/network/participants", "admin")).toBe(true);
    expect(canAccessRoute("/config/network/participants", "node")).toBe(true);
    expect(canAccessRoute("/config/network/participants", "infer")).toBe(false);
    expect(canAccessRoute("/config/preferences/admin", "admin")).toBe(true);
    expect(canAccessRoute("/config/preferences/admin", "node")).toBe(true);
  });

  /**
   * The Dashboard's Details are cluster operation: each machine's internal
   * state, release a slice, remove from the pipeline, leave the network. The `node` opens
   * the Dashboard and gets the Flow - and where there is only one view, there is nothing to
   * choose.
   */
  it("reserves the machine-by-machine detail of the Dashboard for the administrator", () => {
    expect(canSeePeerDetails("admin")).toBe(true);
    expect(canSeePeerDetails("node")).toBe(false);
    expect(canSeePeerDetails("infer")).toBe(false);
    // The Dashboard itself stays open to `node`: what changes is the view.
    expect(canAccessRoute("/", "node")).toBe(true);
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
