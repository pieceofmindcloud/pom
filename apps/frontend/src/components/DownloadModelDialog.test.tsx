// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { startRemoteModelDownload } from "../lib/api";
import { ToastProvider } from "../lib/toast";
import type { RemoteGgufFile, RemoteModelDetails } from "../lib/types";
import { DownloadModelDialog } from "./DownloadModelDialog";

vi.mock("../lib/queries", () => ({
  useServingMachines: () => [{ nodeId: "worker-1", name: "gpu-box" }],
}));
vi.mock("../lib/api", () => ({
  ApiError: class extends Error {},
  startRemoteModelDownload: vi.fn().mockResolvedValue({ download_id: "d1" }),
}));

afterEach(cleanup);

// Not enough space here: a download on this machine is blocked, one on
// another machine is not, since that machine checks its own disk.
const details = {
  repo_id: "org/model",
  revision: null,
  gated: false,
  license: { detected: true, name: "MIT", id: "mit" },
  storage: { destination_paths: ["/models"], available_bytes: 10, destination_available: true },
  gguf_files: [
    { filename: "model-Q4_K_M.gguf", size_bytes: 100 },
    { filename: "mmproj-model-f16.gguf", size_bytes: 5 },
  ],
} as unknown as RemoteModelDetails;
const file = { filename: "model-Q4_K_M.gguf", quantization: "Q4_K_M", size_bytes: 100 } as RemoteGgufFile;

function renderDialog() {
  return render(
    <LocaleProvider initialLocale="en">
      <ToastProvider>
        <DownloadModelDialog open details={details} file={file} onClose={() => {}} onStarted={() => {}} />
      </ToastProvider>
    </LocaleProvider>,
  );
}

describe("DownloadModelDialog", () => {
  it("downloads on another machine, which checks its own space", async () => {
    renderDialog();
    const confirm = screen.getByRole("button", { name: "Start download" }) as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    fireEvent.change(screen.getByLabelText("Download on"), { target: { value: "worker-1" } });
    expect(screen.getByText(/gpu-box downloads with its own Hugging Face token/)).toBeTruthy();
    expect(confirm.disabled).toBe(false);
    fireEvent.click(confirm);
    await waitFor(() =>
      expect(startRemoteModelDownload).toHaveBeenCalledWith(expect.objectContaining({ nodeId: "worker-1" })),
    );
  });

  it("adds a projector the operator ticks to the same download", async () => {
    vi.mocked(startRemoteModelDownload).mockClear();
    renderDialog();
    fireEvent.change(screen.getByLabelText("Download on"), { target: { value: "worker-1" } });
    fireEvent.click(screen.getByRole("checkbox"));
    fireEvent.click(screen.getByRole("button", { name: "Start download" }));
    await waitFor(() =>
      expect(startRemoteModelDownload).toHaveBeenCalledWith(
        expect.objectContaining({ extraFiles: ["mmproj-model-f16.gguf"] }),
      ),
    );
  });
});
