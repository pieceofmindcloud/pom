import { describe, expect, it } from "vitest";
import { IDLE_POLL_INTERVAL, jobsPollInterval, offersPollInterval } from "./polling";

describe("adaptive UI polling", () => {
  it("spaces jobs polling when no job is running", () => {
    expect(jobsPollInterval({ jobs: [] })).toBe(IDLE_POLL_INTERVAL);
    expect(
      jobsPollInterval({ jobs: [{ state: "done" }] }),
    ).toBe(IDLE_POLL_INTERVAL);
  });

  it("uses the fast cadence while a job is running", () => {
    expect(
      jobsPollInterval({ jobs: [{ state: "running" }] }),
    ).toBeLessThan(IDLE_POLL_INTERVAL);
  });

  it("uses the fast cadence while an offer is waiting", () => {
    expect(offersPollInterval({ offers: [] })).toBe(IDLE_POLL_INTERVAL);
    expect(
      offersPollInterval({ offers: [{ id: "offer-1" }] }),
    ).toBeLessThan(IDLE_POLL_INTERVAL);
  });
});
