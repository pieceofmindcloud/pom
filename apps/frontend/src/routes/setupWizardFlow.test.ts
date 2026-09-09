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
  it("keeps the node path focused on hardware, resources, storage capacity, and trust", () => {
    expect(ids("node")).toEqual([
      "setup-profile",
      "setup-hardware",
      "setup-resources",
      "setup-trust",
      "setup-health",
      "setup-review",
    ]);
    expect(setupProfileHasResources("node")).toBe(true);
    expect(setupProfileHasShardSlider("node")).toBe(true);
    expect(setupProfileHasModelAndFolders("node")).toBe(false);
  });

  it("makes infer a short trust-only path after the hardware overview", () => {
    expect(ids("infer")).toEqual([
      "setup-profile",
      "setup-hardware",
      "setup-trust",
      "setup-health",
      "setup-review",
    ]);
    expect(setupProfileUsesOnlyTrust("infer")).toBe(true);
    expect(setupProfileHasResources("infer")).toBe(false);
    expect(setupProfileHasModelAndFolders("infer")).toBe(false);
  });

  it("keeps the complete network-creation path for administrators", () => {
    expect(ids("admin")).toEqual([
      "setup-profile",
      "setup-hardware",
      "setup-resources",
      "setup-model",
      "setup-storage",
      "setup-network",
      "setup-security",
      "setup-invite",
      "setup-health",
      "setup-review",
    ]);
    expect(setupProfileHasResources("admin")).toBe(true);
    expect(setupProfileHasModelAndFolders("admin")).toBe(true);
    expect(setupProfileHasShardSlider("admin")).toBe(false);
  });
});
