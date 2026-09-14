import { describe, expect, it } from "vitest";
import {
  setupProfileHasModelAndFolders,
  setupProfileHasResources,
  setupProfileHasShardSlider,
  setupProfileUsesOnlyTrust,
  setupStepsFor,
} from "./setupWizardFlow";

const ids = (profile: "admin" | "node" | "infer") => setupStepsFor(profile).map((step) => step.id);

describe("first-access wizard flow", () => {
  it("keeps the node path focused on hardware, shared capacity, and trust", () => {
    expect(ids("node")).toEqual([
      "setup-profile",
      "setup-discovery",
      "setup-trust",
      "setup-review",
    ]);
    expect(setupProfileHasResources("node")).toBe(true);
    expect(setupProfileHasShardSlider("node")).toBe(true);
    expect(setupProfileHasModelAndFolders("node")).toBe(false);
  });

  it("makes infer a short trust-only path after the hardware overview", () => {
    expect(ids("infer")).toEqual([
      "setup-profile",
      "setup-discovery",
      "setup-trust",
      "setup-review",
    ]);
    expect(setupProfileUsesOnlyTrust("infer")).toBe(true);
    expect(setupProfileHasResources("infer")).toBe(false);
    expect(setupProfileHasModelAndFolders("infer")).toBe(false);
  });

  it("keeps the complete network-creation path for administrators", () => {
    expect(ids("admin")).toEqual([
      "setup-profile",
      "setup-location",
      "setup-network",
      "setup-invite",
      "setup-discovery",
      "setup-review",
    ]);
    expect(setupProfileHasResources("admin")).toBe(true);
    expect(setupProfileHasModelAndFolders("admin")).toBe(false);
    expect(setupProfileHasShardSlider("admin")).toBe(true);
  });

  it("does not expose model, storage, or security screens for any profile", () => {
    for (const profile of ["admin", "node", "infer"] as const) {
      expect(ids(profile)).not.toEqual(expect.arrayContaining(["setup-model", "setup-storage", "setup-security"]));
    }
  });

  it("labels the merged opening step as the machine overview", () => {
    expect(setupStepsFor("admin")[0]).toEqual({
      id: "setup-profile",
      labelKey: "setupWizard.stepHardware",
    });
  });
});
