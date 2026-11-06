// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { cancelRemoteModelDownload, dismissRemoteModelDownload, retryRemoteModelDownload } from "../lib/api";
import { useModelDownloads } from "../lib/queries";
import { ToastProvider } from "../lib/toast";
import type { Job } from "../lib/types";
import { ModelDownloadsPanel } from "./ModelDownloadsPanel";

vi.mock("../lib/queries", () => ({
  useModelDownloads: vi.fn(),
}));
vi.mock("../lib/api", () => ({
  cancelRemoteModelDownload: vi.fn(),
  retryRemoteModelDownload: vi.fn(),
  dismissRemoteModelDownload: vi.fn(),
}));

afterEach(cleanup);

const job: Job = {
  id: "download-1",
  state: "running",
  kind: "model_download",
  title: "Qwen/model · model-Q5_K_M.gguf",
  step: "downloading",
  message: "",
  progress: 40,
  outputs: [],
  started_at: "2026-08-24T10:00:00Z",
  finished_at: null,
  repo_id: "Qwen/model",
  revision: "main",
  filename: "model-Q5_K_M.gguf",
  quantization: "Q5_K_M",
  download_status: "downloading",
  bytes_downloaded: 40,
  total_bytes: 100,
  speed_bytes_per_sec: 10,
  eta_seconds: 6,
  local_path: "/models/model-Q5_K_M.gguf",
  error: null,
};

function renderPanel(current: Job = job) {
  vi.mocked(useModelDownloads).mockReturnValue({
    data: [current],
    isError: false,
    isLoading: false,
    error: null,
    refetch: vi.fn(),
  } as never);
  return render(
    <LocaleProvider initialLocale="en">
      <ToastProvider>
        <ModelDownloadsPanel />
      </ToastProvider>
    </LocaleProvider>,
  );
}

describe("ModelDownloadsPanel", () => {
  it("renders backend progress and offers cancellation for an active job", async () => {
    renderPanel();
    expect(screen.getByText("Qwen/model")).toBeTruthy();
    expect(screen.getByText("40%")).toBeTruthy();
    expect(screen.getByText("/models/model-Q5_K_M.gguf")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(cancelRemoteModelDownload).toHaveBeenCalledWith("download-1");
  });

  it("keeps retry as a backend action for a failed job", () => {
    renderPanel({
      ...job,
      state: "failed",
      step: "failed",
      download_status: "failed",
      error: "The provider could not complete this download",
    });
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(retryRemoteModelDownload).toHaveBeenCalledWith("download-1");
  });

  it("lets a failed job be dismissed", () => {
    renderPanel({ ...job, state: "failed", step: "failed", download_status: "failed" });
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(dismissRemoteModelDownload).toHaveBeenCalledWith("download-1");
  });

  it("offers no Dismiss while the download runs", () => {
    renderPanel();
    expect(screen.queryByRole("button", { name: "Dismiss" })).toBeNull();
  });
});
