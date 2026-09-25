// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { Shards } from "./Shards";

function shard(overrides: Partial<Record<string, unknown>> = {}) {
  return {
    id: "shard-1",
    name: "qwen-0-13.gguf",
    source: "folder",
    source_label: "Folder",
    path: "/shards/qwen-0-13.gguf",
    size_bytes: 1_500_000_000,
    shard_range: [0, 13],
    shard_group: "qwen-abc123",
    model_label: "qwen",
    pom_id: "abc123",
    ...overrides,
  };
}

const { catalogData } = vi.hoisted(() => ({
  catalogData: {
    hostname: "ksia",
    models: [],
    scanned: [],
    metadata_complete: true,
    shard_sets: [
      {
        model_label: "gemma-3-4b-it",
        mmproj_path: "/models/mmproj-gemma-3-4b-it-f16.gguf",
        pom_id: "vision123",
        group: "gemma-vision123",
        total_layers: 14,
        total_layers_inferred: false,
        shards: [shard({ id: "vision-shard", model_label: "gemma-3-4b-it", pom_id: "vision123", shard_group: "gemma-vision123" })],
        complete: true,
      },
      {
        model_label: "qwen",
        mmproj_path: null,
        pom_id: "abc123",
        group: "qwen-abc123",
        total_layers: 28,
        total_layers_inferred: false,
        shards: [shard()],
        complete: false,
      },
    ],
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
  useCluster: () => ({ data: { self_id: "ksia" }, isLoading: false }),
  useClusterCatalog: () => ({
    data: { peers: [] },
    isFetching: false,
    refetch: vi.fn(),
  }),
}));

vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), fromError: vi.fn() }),
}));

vi.mock("../lib/api", () => ({ api: { post: vi.fn() } }));

afterEach(cleanup);

function renderScreen() {
  return render(
    <LocaleProvider initialLocale="en">
      <Shards />
    </LocaleProvider>,
  );
}

describe("Shards", () => {
  it("shows the mmproj file name for a shard set that has one", () => {
    renderScreen();
    expect(screen.getByText("mmproj-gemma-3-4b-it-f16.gguf")).toBeTruthy();
  });

  it("shows no projector for a shard set without one, instead of inventing a name", () => {
    renderScreen();
    expect(screen.getAllByText("No projector").length).toBeGreaterThan(0);
  });
});
