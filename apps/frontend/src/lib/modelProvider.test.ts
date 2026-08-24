import { describe, expect, it } from "vitest";
import {
  modelDownloadJobs,
  modelDownloadPollInterval,
  modelSearchPath,
  percentOfDownload,
} from "./modelProvider";
import type { Job } from "./types";

const job = (over: Partial<Job>): Job => ({
  id: "job-1",
  state: "running",
  step: "downloading",
  message: "",
  progress: 42,
  outputs: [],
  started_at: "2026-08-24T10:00:00Z",
  finished_at: null,
  kind: "model_download",
  title: "Qwen/model · model-Q5_K_M.gguf",
  repo_id: "Qwen/model",
  revision: "main",
  filename: "model-Q5_K_M.gguf",
  quantization: "Q5_K_M",
  download_status: "downloading",
  bytes_downloaded: 42,
  total_bytes: 100,
  speed_bytes_per_sec: 10,
  eta_seconds: 6,
  local_path: null,
  error: null,
  ...over,
});

describe("model provider browser contract", () => {
  it("serializes search controls without exposing a CLI command", () => {
    expect(
      modelSearchPath({ query: "Qwen 3", ggufOnly: true, page: 2, pageSize: 10 }),
    ).toBe(
      "/api/ui/model-provider/search?q=Qwen+3&page=2&page_size=10&gguf_only=true",
    );
  });

  it("serializes supported remote ordering", () => {
    expect(modelSearchPath({ query: "Qwen", ggufOnly: true, sort: "downloads" })).toBe(
      "/api/ui/model-provider/search?q=Qwen&gguf_only=true&sort=downloads",
    );
  });

  it("keeps only backend model download jobs", () => {
    expect(
      modelDownloadJobs([
        job({}),
        job({ id: "slice", kind: "slice", download_status: undefined }),
        job({ id: "done", state: "done", download_status: "completed" }),
      ]).map((item) => item.id),
    ).toEqual(["job-1", "done"]);
  });

  it("polls quickly while any model download is active", () => {
    expect(modelDownloadPollInterval([job({ download_status: "queued" })])).toBe(2000);
    expect(modelDownloadPollInterval([job({ state: "done", download_status: "completed" })])).toBe(10000);
    expect(modelDownloadPollInterval([])).toBe(10000);
  });

  it("does not invent progress when the backend cannot determine total bytes", () => {
    expect(percentOfDownload({ bytes_downloaded: 10, total_bytes: null })).toBeNull();
    expect(percentOfDownload({ bytes_downloaded: 10, total_bytes: 40 })).toBe(25);
  });
});
