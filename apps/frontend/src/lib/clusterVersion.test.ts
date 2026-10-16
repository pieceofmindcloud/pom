// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { formatClusterVersion } from "./clusterVersion";

describe("formatClusterVersion", () => {
  it("shows the counters and the start of the hash", () => {
    expect(formatClusterVersion("1.4.2.0-9f3c21ab77de0011")).toBe("1.4.2 · 9f3c21ab");
  });

  it("copes with a version from a node that sends no hash, or none at all", () => {
    expect(formatClusterVersion("0.1.0.0")).toBe("0.1.0");
    expect(formatClusterVersion(undefined)).toBe("");
  });
});
