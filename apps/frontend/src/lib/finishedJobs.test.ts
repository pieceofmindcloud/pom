import { describe, expect, it } from "vitest";
import { recentlyFinishedJobs } from "./finishedJobs";
import { RECENTLY_FINISHED_MS } from "./transferStatus";
import type { Job } from "./types";

const now = Date.parse("2026-11-04T12:00:00Z");
const job = (over: Partial<Job>): Job => ({
  id: "j",
  kind: "apply",
  state: "done",
  step: "",
  message: "",
  progress: null,
  outputs: [],
  started_at: new Date(now - 60_000).toISOString(),
  finished_at: new Date(now - 1_000).toISOString(),
  ...over,
} as Job);

describe("recentlyFinishedJobs", () => {
  it("keeps a job that ended within the window, done or failed", () => {
    const ids = recentlyFinishedJobs([job({ id: "a" }), job({ id: "b", state: "failed" })], now).map((j) => j.id);
    expect(ids).toEqual(["a", "b"]);
  });

  it("drops running jobs, old outcomes and model downloads", () => {
    const list = [
      job({ id: "running", state: "running", finished_at: null }),
      job({ id: "old", finished_at: new Date(now - RECENTLY_FINISHED_MS - 1).toISOString() }),
      job({ id: "download", kind: "model_download" } as Partial<Job>),
    ];
    expect(recentlyFinishedJobs(list, now)).toEqual([]);
  });
});
