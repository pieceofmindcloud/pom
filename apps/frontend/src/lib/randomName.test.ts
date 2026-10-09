import { describe, expect, it } from "vitest";
import { randomSlug } from "./randomName";

describe("randomSlug", () => {
  it("is two words and a two digit number, whatever the random source gives", () => {
    for (const value of [0, 0.37, 0.999999, 1]) {
      expect(randomSlug(() => value)).toMatch(/^[a-z]+-[a-z]+-\d{2}$/);
    }
  });
});
