// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, within } from "@testing-library/react";
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

const { catalogData, peersData, linksData } = vi.hoisted(() => ({
  // The projector and the MTP draft live on another machine, in folders of
  // their own: the link is what ties them to the model.
  peersData: [
    {
      node_id: "peer-b",
      hostname: "salem",
      is_self: false,
      models: [],
      shard_sets: [],
      sidecars: [
        { kind: "mmproj", path: "/library/mmproj-gemma-3-4b-it-f16.gguf", file_name: "mmproj-gemma-3-4b-it-f16.gguf", size_bytes: 800_000_000, architecture: "clip", dim: 2560, general_name: null, quant: null },
        { kind: "mtp", path: "/other-quant/draft.gguf", file_name: "draft.gguf", size_bytes: 2_000_000_000, architecture: "gemma3", dim: 2560, general_name: "Ckpt", quant: null },
        { kind: "mmproj", path: "/library/mmproj-other.gguf", file_name: "mmproj-other.gguf", size_bytes: 1, architecture: "clip", dim: 1024, general_name: null, quant: null },
      ],
    },
  ],
  linksData: {
    stale: false,
    links: [
      { model_key: "gemma-3-4b-it", kind: "mmproj", mode: "file", file_name: "mmproj-gemma-3-4b-it-f16.gguf", size_bytes: 800_000_000 },
    ],
  },
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
        shards: [shard({
          id: "vision-shard",
          model_label: "gemma-3-4b-it",
          pom_id: "vision123",
          shard_group: "gemma-vision123",
          architecture: "gemma3",
          embedding_length: 2560,
        })],
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
    data: { peers: peersData },
    isFetching: false,
    refetch: vi.fn(),
  }),
  useSidecarLinks: () => ({ data: linksData }),
  keys: { sidecarLinks: ["model-sidecar-links"] },
}));

vi.mock("../lib/profile", () => ({ useProfile: () => ({ profile: "admin" }) }));

vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), fromError: vi.fn() }),
}));

vi.mock("../lib/api", () => ({ api: { post: vi.fn() } }));

afterEach(cleanup);

function renderScreen() {
  return render(
    <QueryClientProvider client={new QueryClient()}>
      <LocaleProvider initialLocale="en">
        <Shards />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

describe("Shards", () => {
  it("shows the linked projector with the machine and the path where it lives", () => {
    renderScreen();
    const [gemma] = screen.getAllByTestId("sidecar-link-editor");
    const vision = within(gemma).getByTestId("sidecar-picker-mmproj");
    expect(vision.textContent).toContain("mmproj-gemma-3-4b-it-f16.gguf");
    expect(vision.textContent).toContain("salem: /library/mmproj-gemma-3-4b-it-f16.gguf");
    // A projector of another width is never offered for this model.
    expect(vision.textContent).not.toContain("mmproj-other.gguf");
    // The MTP draft of the same architecture and width is offered.
    expect(within(gemma).getByTestId("sidecar-picker-mtp").textContent).toContain("draft.gguf");
  });

  it("says when no compatible file exists, instead of inventing a name", () => {
    renderScreen();
    const [, qwen] = screen.getAllByTestId("sidecar-link-editor");
    expect(within(qwen).getAllByText("No compatible file was found in the network.").length).toBe(2);
  });
});
