// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { RemoteModelDetails } from "./RemoteModelDetails";

vi.mock("../lib/queries", () => ({
  useRemoteModelDetails: () => ({
    data: {
      repo_id: "Qwen/Qwen3-GGUF",
      description: "A gated GGUF model",
      last_modified: null,
      revision: "main",
      sha: "abc123",
      downloads: 1234,
      likes: 56,
      private: false,
      gated: true,
      tags: ["gguf"],
      license: {
        id: "apache-2.0",
        name: "Apache License 2.0",
        url: "https://www.apache.org/licenses/LICENSE-2.0",
        raw_value: "apache-2.0",
        detected: true,
      },
      files: [],
      gguf_files: [
        {
          filename: "Qwen3-Q5_K_M.gguf",
          size_bytes: 1024,
          quantization: "Q5_K_M",
          parameter_size: "8B",
          base_name: "Qwen3",
          installed: false,
          downloading: false,
          download_id: null,
        },
      ],
      storage: { available_bytes: 10_000, destination_available: true },
    },
    isLoading: false,
    isError: false,
    refetch: vi.fn(),
  }),
}));

afterEach(cleanup);

describe("RemoteModelDetails", () => {
  it("shows detected license and gated state before choosing a download", () => {
    const onChoose = vi.fn();

    render(
      <LocaleProvider initialLocale="en">
        <RemoteModelDetails
          repoId="Qwen/Qwen3-GGUF"
          open
          onClose={() => {}}
          onChoose={onChoose}
        />
      </LocaleProvider>,
    );

    expect(screen.getByText("Gated")).toBeTruthy();
    expect(screen.getByText("Apache License 2.0")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Download" }));
    expect(onChoose).toHaveBeenCalledTimes(1);
  });
});
