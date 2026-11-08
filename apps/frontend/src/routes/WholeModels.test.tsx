// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
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
    refetch: vi.fn(),
  }),
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
    expect(screen.getAllByRole("button", { name: "Slice" })).toHaveLength(2);
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
