// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { WholeModels } from "./WholeModels";

const { catalogData } = vi.hoisted(() => ({
  catalogData: {
    hostname: "ksia",
    models: [
      {
        id: "model-vision",
        name: "gemma-3-4b-it.gguf",
        source: "folder",
        source_label: "Folder",
        path: "/models/gemma-3-4b-it.gguf",
        mmproj_path: "/models/mmproj-gemma-3-4b-it-f16.gguf",
        size_bytes: 4_000_000_000,
        architecture: "gemma3",
        total_layers: 34,
        quant: "Q4_K_M",
        shard_range: null,
        shard_group: null,
        versions: [],
        active_version: 0,
      },
      {
        id: "model-text",
        name: "qwen.gguf",
        source: "folder",
        source_label: "Folder",
        path: "/models/qwen.gguf",
        mmproj_path: null,
        size_bytes: 3_000_000_000,
        architecture: "qwen3",
        total_layers: 28,
        quant: "Q4_K_M",
        shard_range: null,
        shard_group: null,
        versions: [],
        active_version: 0,
      },
    ],
    scanned: [{ path: "/models", exists: true, found: 2, source: "folder" }],
    metadata_complete: true,
  },
}));

vi.mock("../lib/queries", () => ({
  useLocalCatalog: () => ({
    data: catalogData,
    isLoading: false,
    isError: false,
    error: null,
    isFetching: false,
    refetch: vi.fn(),
  }),
  useNode: () => ({ data: { pom_dev_mode: false } }),
}));

vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), fromError: vi.fn() }),
}));

afterEach(cleanup);

function renderScreen() {
  return render(
    <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
      <LocaleProvider initialLocale="en">
        <WholeModels />
      </LocaleProvider>
    </MemoryRouter>,
  );
}

describe("WholeModels", () => {
  it("shows the mmproj file name when the model has a vision projector", () => {
    renderScreen();
    expect(screen.getByText("mmproj-gemma-3-4b-it-f16.gguf")).toBeTruthy();
  });

  it("shows no projector for a model without one, instead of inventing a name", () => {
    renderScreen();
    const rows = screen.getAllByRole("row");
    const textRow = rows.find((row) => row.textContent?.includes("qwen.gguf"));
    expect(textRow?.textContent).toContain("No projector");
  });

  it("renders every model as a full-width table row with its actions", () => {
    renderScreen();
    const table = screen.getByRole("table");
    expect(table).toBeTruthy();
    expect(screen.getAllByRole("row")).toHaveLength(3); // header + 2 models
    // Pipelines serve ranges from the whole file; slicing is only for tensor
    // parallel, which is a development feature.
    expect(screen.queryByRole("button", { name: "Slice" })).toBeNull();
  });
});
