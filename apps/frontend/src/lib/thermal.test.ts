import { describe, expect, it } from "vitest";
import { highestKnownTemperature, temperatureBand } from "./thermal";
import type { ThermalPolicy } from "./types";

const policy: ThermalPolicy = {
  limit_c: 85,
  hysteresis_c: 8,
  cooldown_secs: 30,
};

describe("thermal helpers", () => {
  it("keeps an unavailable temperature explicitly unknown", () => {
    expect(temperatureBand(null, policy)).toBe("unknown");
    expect(highestKnownTemperature([null, null])).toBeNull();
  });

  it("classifies normal, attention and critical temperatures", () => {
    expect(temperatureBand(76, policy)).toBe("normal");
    expect(temperatureBand(77, policy)).toBe("attention");
    expect(temperatureBand(85, policy)).toBe("critical");
  });

  it("summarizes a machine by the highest known GPU temperature", () => {
    expect(highestKnownTemperature([null, 73.5, 81])).toBe(81);
  });
});
