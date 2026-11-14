// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { rescanModels, startRemoteModelDownload } from "../lib/api";
import { useMachineDownloads } from "../lib/queries";
import { ToastProvider } from "../lib/toast";
import type { WholeModelEntry } from "../lib/wholeModels";
import { ModelPresence } from "./ModelPresence";

vi.mock("../lib/queries", () => ({ keys: { clusterModels: ["cluster-models"] }, useMachineDownloads: vi.fn() }));
vi.mock("../lib/api", () => ({
  startRemoteModelDownload: vi.fn().mockResolvedValue({ download_id: "d" }),
  rescanModels: vi.fn().mockResolvedValue({}),
}));

afterEach(cleanup);

const model = {
  name: "M",
  holders: ["a"],
  source: { repo_id: "org/m", revision: "main", filename: "M-00001-of-00002.gguf", extra_files: ["M-00002-of-00002.gguf"] },
} as unknown as WholeModelEntry;

function renderCard(downloads: unknown[] = [], entry = model) {
  vi.mocked(useMachineDownloads).mockReturnValue({ data: downloads, refetch: vi.fn() } as never);
  return render(
    <QueryClientProvider client={new QueryClient()}>
      <LocaleProvider initialLocale="en">
        <ToastProvider>
          <ModelPresence nodeId="b" machine="box-b" model={entry} />
        </ToastProvider>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

describe("ModelPresence", () => {
  it("downloads the holder's repository and every part on the machine that lacks it", async () => {
    renderCard();
    fireEvent.click(screen.getByRole("button", { name: "Download here" }));
    await waitFor(() =>
      expect(startRemoteModelDownload).toHaveBeenCalledWith({
        repoId: "org/m",
        revision: "main",
        filename: "M-00001-of-00002.gguf",
        nodeId: "b",
        extraFiles: ["M-00002-of-00002.gguf"],
      }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Rescan" }));
    await waitFor(() => expect(rescanModels).toHaveBeenCalledWith("b"));
  });

  it("offers only Rescan for a model placed by hand", () => {
    renderCard([], { ...model, source: null });
    expect(screen.queryByRole("button", { name: "Download here" })).toBeNull();
    expect(screen.getByRole("button", { name: "Rescan" })).toBeTruthy();
  });

  it("shows the download running there", () => {
    renderCard([{ id: "d", kind: "model_download", state: "running", download_status: "downloading", repo_id: "org/m", filename: "M-00001-of-00002.gguf", bytes_downloaded: 50, total_bytes: 100 }]);
    expect(screen.getByTestId("model-presence-downloading").textContent).toContain("50%");
  });
});
