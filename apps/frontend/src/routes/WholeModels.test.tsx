// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { WholeModels } from "./WholeModels";

const { catalogData, deleteModelMock, catalogRefetch } = vi.hoisted(() => ({
  deleteModelMock: vi.fn(),
  catalogRefetch: vi.fn(),
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
        id: "model-multipart",
        name: "multipart.gguf",
        source: "folder",
        source_label: "Folder",
        path: "/models/multipart-00001-of-00002.gguf",
        mmproj_path: null,
        size_bytes: 3_000_000_000,
        architecture: "qwen3",
        total_layers: 28,
        quant: "Q4_K_M",
        shard_range: null,
        shard_group: null,
        multipart: true,
        multipart_parts: [
          { index: 0, path: "/models/multipart-00001-of-00002.gguf", bytes: 1_000_000, tensor_count: 0 },
          { index: 1, path: "/models/multipart-00002-of-00002.gguf", bytes: 2_999_000_000, tensor_count: 128 },
        ],
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
    sidecars: [
      {
        kind: "mmproj",
        path: "/models/mmproj-gemma-f16.gguf",
        file_name: "mmproj-gemma-f16.gguf",
        size_bytes: 2_000_000_000,
        architecture: "clip",
        dim: 4096,
        general_name: "Gemma vision projector",
        quant: "F16",
      },
      {
        kind: "mtp",
        path: "/models/mtp-gemma-q8.gguf",
        file_name: "mtp-gemma-q8.gguf",
        size_bytes: 1_000_000_000,
        architecture: "gemma3",
        dim: 4096,
        general_name: "Gemma MTP draft",
        quant: "Q8_0",
      },
    ],
    image_files: [
      {
        id: "folder:/models/pub/repo/ae.safetensors",
        name: "pub/repo/ae.safetensors",
        path: "/models/pub/repo/ae.safetensors",
        size_bytes: 335_304_388,
        source: "folder",
        source_label: "Folder",
        format: "safetensors",
        category: "vae",
        family: "flux.1-ae",
        dtype: "bf16",
        metadata_error: null,
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
    refetch: catalogRefetch,
  }),
  useNode: () => ({ data: { pom_dev_mode: false } }),
}));

vi.mock("../lib/deleteModel", () => ({
  deleteModel: deleteModelMock,
}));

vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), fromError: vi.fn() }),
}));

beforeEach(() => {
  deleteModelMock.mockReset();
  deleteModelMock.mockResolvedValue({ status: "deleted", freedBytes: 1_000_000 });
  catalogRefetch.mockReset();
});

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
    expect(screen.getAllByRole("row")).toHaveLength(5); // header + 3 models + one multipart card row
    // Pipelines serve ranges from the whole file; slicing is only for tensor
    // parallel, which is a development feature.
    expect(screen.queryByRole("button", { name: "Slice" })).toBeNull();
  });

  it("shows each physical multipart file and can delete one without splitting the model into rows", () => {
    renderScreen();
    expect(screen.getByText("multipart.gguf")).toBeTruthy();
    expect(screen.getByText("multipart-00001-of-00002.gguf")).toBeTruthy();
    expect(screen.getByText("multipart-00002-of-00002.gguf")).toBeTruthy();
    expect(screen.getByText("977 KB")).toBeTruthy();
    const secondPart = screen.getByText("multipart-00002-of-00002.gguf").closest("li");
    expect(secondPart?.textContent).toContain("2.8 GB");
    expect(screen.getByText("2 physical files in this multipart model")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Delete multipart-00002-of-00002.gguf from disk" }));
    expect(deleteModelMock).toHaveBeenCalledWith(
      "/models/multipart-00002-of-00002.gguf",
      "multipart-00002-of-00002.gguf",
      expect.any(Function),
    );
  });

  it("shows mmproj and MTP files in their own tabs with delete actions", () => {
    renderScreen();
    fireEvent.click(screen.getByRole("tab", { name: "Vision (mmproj)" }));
    expect(screen.getByText("mmproj-gemma-f16.gguf")).toBeTruthy();
    expect(screen.getByText("Gemma vision projector")).toBeTruthy();
    expect(screen.getByText("1.9 GB")).toBeTruthy();
    expect(screen.queryByText("mtp-gemma-q8.gguf")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Delete mmproj-gemma-f16.gguf from disk" }));
    expect(deleteModelMock).toHaveBeenCalledWith("/models/mmproj-gemma-f16.gguf", "mmproj-gemma-f16.gguf", expect.any(Function));

    fireEvent.click(screen.getByRole("tab", { name: "MTP drafts" }));
    expect(screen.getByText("mtp-gemma-q8.gguf")).toBeTruthy();
    expect(screen.getByText("Gemma MTP draft")).toBeTruthy();
    expect(screen.getByText("954 MB")).toBeTruthy();
    expect(screen.queryByText("mmproj-gemma-f16.gguf")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Delete mtp-gemma-q8.gguf from disk" }));
    expect(deleteModelMock).toHaveBeenLastCalledWith("/models/mtp-gemma-q8.gguf", "mtp-gemma-q8.gguf", expect.any(Function));
  });

  it("shows image files under the Image switch", () => {
    renderScreen();
    expect(screen.getByText("qwen.gguf")).toBeTruthy();
    expect(screen.queryByText("pub/repo/ae.safetensors")).toBeNull();
    fireEvent.click(screen.getByRole("tab", { name: "Image" }));
    expect(screen.getByText("pub/repo/ae.safetensors")).toBeTruthy();
    expect(screen.getByText("VAE")).toBeTruthy();
    expect(screen.getByText("bf16")).toBeTruthy();
    expect(screen.getByText("flux.1-ae")).toBeTruthy();
    expect(screen.queryByText("qwen.gguf")).toBeNull();
  });

  it("image rows offer delete and no slice", () => {
    renderScreen();
    fireEvent.click(screen.getByRole("tab", { name: "Image" }));
    expect(screen.getByRole("button", { name: /Delete pub\/repo\/ae.safetensors/ })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Slice" })).toBeNull();
  });
});
