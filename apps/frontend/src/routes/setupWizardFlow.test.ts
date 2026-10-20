import { describe, expect, it } from "vitest";
import { setupProfileUsesOnlyTrust, setupStepsFor } from "./setupWizardFlow";

const ids = (profile: "admin" | "worker" | "visitor" | null) => setupStepsFor(profile).map((step) => step.id);

describe("first-access wizard flow", () => {
  it("creates a network in three steps: start, network and invite", () => {
    expect(ids("admin")).toEqual(["setup-profile", "setup-network", "setup-invite"]);
  });

  it("joins a network in two steps for node and infer", () => {
    expect(ids("worker")).toEqual(["setup-profile", "setup-join"]);
    expect(ids("visitor")).toEqual(["setup-profile", "setup-join"]);
    expect(setupProfileUsesOnlyTrust("worker")).toBe(true);
    expect(setupProfileUsesOnlyTrust("admin")).toBe(false);
  });

  it("starts with the single start step before a role is chosen", () => {
    expect(ids(null)).toEqual(["setup-profile"]);
  });

  it("never asks for location, review, password or resources as their own steps", () => {
    for (const profile of ["admin", "worker", "visitor"] as const) {
      const steps = ids(profile);
      expect(steps).not.toContain("setup-location");
      expect(steps).not.toContain("setup-review");
      expect(steps).not.toContain("setup-trust");
    }
  });
});
