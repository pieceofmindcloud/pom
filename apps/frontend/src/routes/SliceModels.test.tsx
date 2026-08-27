// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";

const { post, jobsState } = vi.hoisted(() => ({
  post: vi.fn(),
  jobsState: { value: { jobs: [] as unknown[] } },
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
            mode: "pipeline",
            slices: [{ target_mb: 4096 }, {}],
          },
        ],
      ]),
    );
  });

  it("shows the real required head cost for intermediate slices", async () => {
    post.mockResolvedValue({
      total_layers: 10,
      model_bytes: 3_440_000_000,
      required_head_bytes: 440 * 1024 * 1024,
      ranges: [
        {
          index: 0,
          start_layer: 0,
          end_layer: 4,
          layers: 5,
          bytes: 1_940_000_000,
          target_mb: 1940,
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

    render(
      <MemoryRouter initialEntries={["/modelos/fatiar"]}>
        <LocaleProvider initialLocale="pt-BR">
          <SliceModels />
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(
      await screen.findByText(
        "Cada fatia intermediária repete 440 MB de pesos reais da cabeça obrigatória.",
      ),
    ).toBeTruthy();
  });

  it("shows the active slicing progress on the model slicing screen", async () => {
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
      <MemoryRouter initialEntries={["/modelos/fatiar"]}>
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
      <MemoryRouter initialEntries={["/modelos/fatiar"]}>
        <LocaleProvider initialLocale="pt-BR">
          <SliceModels />
        </LocaleProvider>
      </MemoryRouter>,
    );
    expect(await screen.findByText("falhou")).toBeTruthy();
    expect(screen.getByText("falha depois da primeira fatia")).toBeTruthy();
    expect(screen.getByText("/models_sliced/primeira.gguf")).toBeTruthy();
  });

  it("plans and starts one explicitly selected layer range", async () => {
    post.mockImplementation(async (url: string, body: Record<string, unknown>) => {
      if (url === "/api/ui/model-layer-options") {
        return {
          total_layers: 10,
          valid_starts: [0, 5],
          cuts: [],
          granularity: 5,
          rule: "modelo híbrido: cortes nas fronteiras reais",
        };
      }
      const range = body.range as { start_layer: number; end_layer: number } | undefined;
      return {
        total_layers: 10,
        model_bytes: 1_500_000_000,
        ranges: [{
          index: 0,
          start_layer: range?.start_layer ?? 0,
          end_layer: range?.end_layer ?? 4,
          layers: 5,
          bytes: 1_500_000_000,
          target_mb: null,
          is_rest: false,
          is_last: false,
          too_small: false,
          empty: false,
        }],
        covered_layers: 5,
        complete: false,
        missing_range: [5, 9],
        missing_bytes: 1,
        blocked: false,
        warnings: [],
      };
    });

    render(
      <MemoryRouter initialEntries={["/modelos/fatiar"]}>
        <LocaleProvider initialLocale="pt-BR">
          <SliceModels />
        </LocaleProvider>
      </MemoryRouter>,
    );

    fireEvent.click(await screen.findByLabelText("Fatiar uma faixa específica"));
    expect(await screen.findByLabelText("Primeira camada")).toBeTruthy();
    fireEvent.change(screen.getByLabelText("Primeira camada"), { target: { value: "5" } });
    fireEvent.change(screen.getByLabelText("Última camada"), { target: { value: "9" } });

    await waitFor(() => expect(post).toHaveBeenCalledWith(
      "/api/ui/slice/plan",
      expect.objectContaining({ range: { start_layer: 5, end_layer: 9 } }),
    ));
    fireEvent.click(screen.getByRole("button", { name: "Fatiar agora" }));
    await waitFor(() => expect(post).toHaveBeenCalledWith(
      "/api/ui/slice",
      expect.objectContaining({ range: { start_layer: 5, end_layer: 9 } }),
    ));
  });

  it("does not render the slicing activity card when there is no active job", async () => {
    jobsState.value = { jobs: [] };

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
    expect(screen.queryByText("Fatiamentos em andamento")).toBeNull();
  });

  it("plans tensor parallel shards from the mode checkbox and rank count", async () => {
    post.mockImplementation(async (_url: string, body: Record<string, unknown>) => {
      if (body.mode === "tensor_parallel") {
        return {
          mode: "tensor_parallel",
          model_bytes: 3_000_000_000,
          total_layers: 10,
          tp_size: body.tp_size,
          ranks: [
            { rank: 0, start_layer: 0, end_layer: 9 },
            { rank: 1, start_layer: 0, end_layer: 9 },
          ],
          complete: true,
          blocked: false,
          warnings: [],
        };
      }
      return {
        total_layers: 10,
        model_bytes: 3_000_000_000,
        ranges: [],
        warnings: [],
      };
    });

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

    fireEvent.click(await screen.findByLabelText("Fatiar por Tensor Parallelism"));
    fireEvent.change(screen.getByLabelText("Quantidade de ranks TP"), {
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
    expect(await screen.findByText("1of2")).toBeTruthy();
  });

  it("shows the summed size and every physical part of a multipart model", async () => {
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

    fireEvent.change(screen.getAllByRole("combobox")[0], {
      target: { value: "/models/multipart-00001-of-00004.gguf" },
    });

    expect(await screen.findByText("GGUF MULTIPARTE")).toBeTruthy();
    expect(screen.getByText("4 partes físicas neste conjunto")).toBeTruthy();
    expect(screen.getByText(/Mantenha todas as partes juntas/)).toBeTruthy();
    expect(screen.getByText("119 GB")).toBeTruthy();
  });

  it("blocks an incomplete multipart source with its concrete reason", async () => {
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

    fireEvent.change(screen.getAllByRole("combobox")[0], {
      target: { value: "/models/broken-00001-of-00004.gguf" },
    });

    expect((await screen.findAllByText(/split\.count=4/)).length).toBeGreaterThan(0);
    expect((screen.getByRole("button", { name: "Fatiar agora" }) as HTMLButtonElement).disabled).toBe(true);
  });
});
