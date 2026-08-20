import { describe, expect, it } from "vitest";
import { NAV } from "./nav";

describe("navigation structure", () => {
  it("keeps slicing destinations together under Models in operator order", () => {
    const models = NAV.find((section) => section.labelKey === "nav.models");

    expect(models?.children?.map((child) => child.to)).toEqual([
      "/modelos",
      "/modelos/fatiar",
      "/fatias",
    ]);
    expect(NAV.some((section) => section.labelKey === "nav.shardedModels")).toBe(false);
    expect(NAV.flatMap((section) => section.children ?? []).some(
      (child) => child.to === "/fatias/andamento",
    )).toBe(false);
  });
});
