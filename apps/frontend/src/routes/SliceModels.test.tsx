// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";

const { post } = vi.hoisted(() => ({ post: vi.fn() }));

vi.mock("../lib/api", () => ({
  api: { post },
}));

vi.mock("../lib/queries", () => ({
  useLocalCatalog: () => ({
    data: {
      models: [
        {
          id: "model-1",
          name: "qwen.gguf",
          source: "folder",
          source_label: "Pasta",
          path: "/models/qwen.gguf",
          size_bytes: 3_000_000_000,
          architecture: "llama",
          total_layers: 10,
          quant: "Q4_K_M",
          shard_range: null,
          shard_group: null,
        },
      ],
      shard_sets: [],
    },
    isLoading: false,
    isError: false,
    error: null,
    refetch: vi.fn(),
  }),
  useInvalidate: () => vi.fn(),
}));

vi.mock("../lib/toast", () => ({
  useToast: () => ({
    success: vi.fn(),
    fromError: vi.fn(),
  }),
}));

import { SliceModels } from "./SliceModels";

describe("SliceModels", () => {
  beforeEach(() => {
    post.mockReset();
    post.mockResolvedValue({
      total_layers: 10,
      model_bytes: 3_000_000_000,
      ranges: [
        {
          index: 0,
          start_layer: 0,
          end_layer: 4,
          layers: 5,
          bytes: 1_500_000_000,
          target_mb: 1500,
          is_rest: false,
          is_last: false,
          too_small: false,
          empty: false,
        },
        {
          index: 1,
          start_layer: 5,
          end_layer: 9,
          layers: 5,
          bytes: 1_500_000_000,
          target_mb: null,
          is_rest: true,
          is_last: true,
          too_small: false,
          empty: false,
        },
      ],
      covered_layers: 10,
      complete: true,
      missing_range: null,
      missing_bytes: 0,
      blocked: false,
      warnings: [],
    });
  });

  afterEach(cleanup);

  it("shows the final shard as fixed remainder without a size input", async () => {
    render(
      <MemoryRouter
        initialEntries={["/modelos/fatiar"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <SliceModels />
        </LocaleProvider>
      </MemoryRouter>,
    );

    await waitFor(() => expect(screen.getAllByRole("textbox")).toHaveLength(1));
    await waitFor(() => expect(post).toHaveBeenCalledWith(
      "/api/ui/slice/plan",
      expect.any(Object),
    ));
    expect(screen.getAllByText("o resto").length).toBeGreaterThan(0);

    expect(post.mock.calls).toEqual(
      expect.arrayContaining([
        [
          "/api/ui/slice/plan",
          {
            model_path: "/models/qwen.gguf",
            slices: [{ target_mb: 4096 }, {}],
          },
        ],
      ]),
    );
  });
});
