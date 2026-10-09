import { describe, expect, it } from "vitest";
import { rateSpeed } from "./peerSpeed";

describe("rateSpeed", () => {
  it("compares with the fastest slice of the same model", () => {
    const peers = [100, 80, 50, 20];
    expect(rateSpeed(100, peers)).toEqual({ tone: "fast", ratio: 1 });
    expect(rateSpeed(80, peers).tone).toBe("fast");
    expect(rateSpeed(50, peers).tone).toBe("medium");
    expect(rateSpeed(20, peers)).toEqual({ tone: "slow", ratio: 0.2 });
  });

  it("does not paint the only measured slice green", () => {
    expect(rateSpeed(42, [42, null, undefined])).toEqual({ tone: "solo", ratio: null });
  });

  it("invents no speed for whoever was not measured", () => {
    expect(rateSpeed(null, [10, 20]).tone).toBe("unknown");
    expect(rateSpeed(0, [10, 20]).tone).toBe("unknown");
    expect(rateSpeed(Number.NaN, [10, 20]).tone).toBe("unknown");
  });
});
