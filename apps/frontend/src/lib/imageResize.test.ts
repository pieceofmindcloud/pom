import { describe, expect, it } from "vitest";
import { resizeDimensions } from "./imageResize";

describe("resizeDimensions", () => {
  it("caps a landscape image at the projector limit", () => {
    expect(resizeDimensions(4000, 2000)).toEqual({ width: 2048, height: 1024 });
  });

  it("preserves a portrait image that is already within the limit", () => {
    expect(resizeDimensions(1000, 2000)).toEqual({ width: 1000, height: 2000 });
  });
});
