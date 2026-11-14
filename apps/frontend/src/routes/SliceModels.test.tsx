// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";

const { post, jobsState, nodeState } = vi.hoisted(() => ({
  post: vi.fn(),
  jobsState: { value: { jobs: [] as unknown[] } },
  nodeState: { value: { pom_dev_mode: true } as { pom_dev_mode: boolean } | undefined },
}));

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
        {
          id: "model-multipart",
          name: "multipart.gguf",
          source: "folder",
          source_label: "Pasta",
          path: "/models/multipart-00001-of-00004.gguf",
          size_bytes: 128_206_729_792,
          architecture: "test_arch",
          total_layers: 43,
          quant: "Q3_K_XL",
          shard_range: null,
          shard_group: null,
          multipart: true,
          multipart_parts: [
            { index: 0, path: "/models/multipart-00001-of-00004.gguf", bytes: 5_257_664, tensor_count: 0 },
            { index: 1, path: "/models/multipart-00002-of-00004.gguf", bytes: 49_350_774_208, tensor_count: 504 },
            { index: 2, path: "/models/multipart-00003-of-00004.gguf", bytes: 49_533_038_144, tensor_count: 528 },
            { index: 3, path: "/models/multipart-00004-of-00004.gguf", bytes: 29_317_659_776, tensor_count: 296 },
          ],
        },
        {
          id: "model-multipart-broken",
          name: "broken-multipart.gguf",
          source: "folder",
          source_label: "Pasta",
          path: "/models/broken-00001-of-00004.gguf",
          size_bytes: 0,
          architecture: "test_arch",
          total_layers: 43,
          quant: "Q3_K_XL",
          shard_range: null,
          shard_group: null,
          multipart: true,
          multipart_parts: [],
          multipart_error: "split.count=4, mas a parte split.no=2 está ausente",
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
  useJobs: () => ({
    data: jobsState.value,
    isLoading: false,
    isError: false,
    error: null,
    refetch: vi.fn(),
  }),
  useNode: () => ({
    data: nodeState.value,
    isLoading: false,
    isError: false,
    error: null,
    refetch: vi.fn(),
  }),
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
    nodeState.value = { pom_dev_mode: true };
    jobsState.value = {
      jobs: [
        {
          id: "job-slice-1",
          state: "running",
          step: "writing",
          message: "writing shard 1 of 2",
          progress: 42,
          outputs: [],
          started_at: "2026-08-20T12:00:00Z",
          finished_at: null,
          kind: "slice",
          title: "qwen.gguf",
        },
      ],
    };
    post.mockResolvedValue({
      mode: "tensor_parallel",
      tp_size: 2,
      ranks: [
        { rank: 0, start_layer: 0, end_layer: 9 },
        { rank: 1, start_layer: 0, end_layer: 9 },
      ],
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

  it("shows the active slicing progress on the model slicing screen", async () => {
    render(
      <MemoryRouter
        initialEntries={["/models/slice"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <SliceModels />
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(await screen.findByText("Fatiamentos em andamento")).toBeTruthy();
    expect(screen.getByText("42%")).toBeTruthy();
    expect(screen.getByRole("progressbar").getAttribute("aria-valuenow")).toBe("42");
  });

  it("shows the read phase heartbeat and a partial failure", async () => {
    jobsState.value = {
      jobs: [
        {
          id: "job-slice-read",
          state: "running",
          step: "lendo modelo",
          message: "lendo o modelo de origem para identificar a operação",
          progress: null,
          bytes_processed: 50 * 1024 * 1024,
          total_bytes: 100 * 1024 * 1024,
          speed_bytes_per_sec: 2 * 1024 * 1024,
          eta_seconds: 25,
          current_file: "/models/deepseek.gguf",
          phase_started_at: "2026-08-20T12:00:00Z",
          outputs: [],
          started_at: "2026-08-20T12:00:00Z",
          finished_at: null,
          kind: "slice",
          title: "deepseek.gguf",
        },
      ],
    };

    render(
      <MemoryRouter
        initialEntries={["/models/slice"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <SliceModels />
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(await screen.findByText("lendo o modelo de origem para identificar a operação")).toBeTruthy();
    expect(screen.getByText("50 MB de 100 MB")).toBeTruthy();
    expect(screen.getByText("/models/deepseek.gguf")).toBeTruthy();
    expect(screen.getByText("25s restantes")).toBeTruthy();
    expect(screen.getByRole("progressbar").getAttribute("aria-valuenow")).toBe("50");

    jobsState.value = {
      jobs: [
        {
          id: "job-slice-failed",
          state: "failed",
          step: "erro",
          message: "falha depois da primeira fatia",
          error: "falha depois da primeira fatia",
          progress: 42,
          outputs: ["/models_sliced/primeira.gguf"],
          started_at: "2026-08-20T12:00:00Z",
          finished_at: "2026-08-20T12:03:00Z",
          kind: "slice",
          title: "deepseek.gguf",
        },
      ],
    };
    // The mock hook reads mutable state on render; remount to model the next poll.
    cleanup();
    render(
      <MemoryRouter
        initialEntries={["/models/slice"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <SliceModels />
        </LocaleProvider>
      </MemoryRouter>,
    );
    expect(await screen.findByText("falhou")).toBeTruthy();
    expect(screen.getByText("falha depois da primeira fatia")).toBeTruthy();
    expect(screen.getByText("/models_sliced/primeira.gguf")).toBeTruthy();
  });

  it("plans and starts tensor parallel shards from the rank count", async () => {
    render(
      <MemoryRouter
        initialEntries={["/models/slice"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <SliceModels />
        </LocaleProvider>
      </MemoryRouter>,
    );

    fireEvent.change(await screen.findByLabelText("Quantidade de ranks TP"), {
      target: { value: "2" },
    });

    await waitFor(() =>
      expect(post).toHaveBeenCalledWith(
        "/api/ui/slice/plan",
        expect.objectContaining({
          model_path: "/models/qwen.gguf",
          mode: "tensor_parallel",
          tp_size: 2,
        }),
      ),
    );
    expect(await screen.findByText("rank 0")).toBeTruthy();
    expect(screen.getByText("1of2")).toBeTruthy();
    expect(screen.queryByRole("tab", { name: "Pipeline" })).toBeNull();

    post.mockResolvedValueOnce({ job_id: "abcdef1234" });
    fireEvent.click(screen.getByRole("button", { name: "Fatiar agora" }));
    await waitFor(() =>
      expect(post).toHaveBeenCalledWith(
        "/api/ui/slice",
        expect.objectContaining({ mode: "tensor_parallel", tp_size: 2 }),
      ),
    );
  });

  it("offers nothing to slice outside development mode", async () => {
    nodeState.value = { pom_dev_mode: false };
    render(
      <MemoryRouter
        initialEntries={["/models/slice"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <SliceModels />
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(await screen.findByText("Partes tensor-parallel exigem o modo de desenvolvimento")).toBeTruthy();
    expect(screen.queryByLabelText("Quantidade de ranks TP")).toBeNull();
    await new Promise((resolve) => setTimeout(resolve, 300));
    expect(post).not.toHaveBeenCalled();
  });

  it("blocks an incomplete multipart source with its concrete reason", async () => {
    render(
      <MemoryRouter
        initialEntries={["/models/slice"]}
        future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
      >
        <LocaleProvider initialLocale="pt-BR">
          <SliceModels />
        </LocaleProvider>
      </MemoryRouter>,
    );

    fireEvent.change(screen.getAllByRole("combobox")[0], {
      target: { value: "/models/broken-00001-of-00004.gguf" },
    });

    expect((await screen.findAllByText(/split\.count=4/)).length).toBeGreaterThan(0);
    expect((screen.getByRole("button", { name: "Fatiar agora" }) as HTMLButtonElement).disabled).toBe(true);
  });
});
