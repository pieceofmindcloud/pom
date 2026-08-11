import { describe, expect, it } from "vitest";
import { isNearBottom } from "./chatScroll";

describe("chat scroll following", () => {
  it("does not follow a stream when the reader is above the bottom", () => {
    expect(isNearBottom({ scrollTop: 40, clientHeight: 300, scrollHeight: 800 }, 32)).toBe(false);
  });

  it("follows the stream when the reader is at the bottom", () => {
    expect(isNearBottom({ scrollTop: 468, clientHeight: 300, scrollHeight: 800 }, 32)).toBe(true);
  });
});
