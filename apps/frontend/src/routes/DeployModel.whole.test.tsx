// @vitest-environment jsdom
/**
 * Deploying a whole model through the same screen.
 *
 * What these tests guard is not a second screen - it is that there is **not** one.
 * The choice at the top changes the question that goes to the node (`kind`) and
 * does away with the questions that now have a single answer; everything else -
 * the plan, the validation, the adjustments, the button - remains the slices path.
 *
 * The two errors that would make the screen lie are covered here:
 *
 * * sending the wrong `kind` (or not sending it), and the node answering about
 *   slices while the screen shows a whole model;
 * * leaving the per-layer coverage validation on screen - the coverage ribbon
 *   and the "Entra" column, which on a whole model are a solid bar and a
 *   checkbox that never unchecks. Noise dressed up as choice.
 */
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DeployPlan, PeerCatalog, RuntimeParams } from "../lib/types";

const post = vi.fn();
/** What the live cluster answers; a test fills it to put machines in service. */
let livePeers: Array<Record<string, unknown>> = [];
vi.mock("../lib/api", () => ({
  api: {
    get: vi.fn((url: string) =>
      url === "/api/ui/jobs"
        ? Promise.resolve({ jobs: [] })
        : url === "/api/ui/cluster"
          ? Promise.resolve({ peers: [] })
          : url === "/api/ui/node"
            ? Promise.resolve({ connection_key: "", pom_dev_mode: true })
            : url === "/api/ui/cache-types"
              ? Promise.resolve({
                  cache_type_k: "f16",
                  cache_type_v: "f16",
                  choices: ["f32", "f16", "bf16", "q8_0", "q4_0", "q4_1", "iq4_nl", "q5_0", "q5_1"],
                })
              : undefined,
    ),
    post: (...args: unknown[]) => post(...args),
  },
  getConnectionKey: () => "",
  setConnectionKey: () => {},
  ApiError: class extends Error {},
}));

import { DeployModel } from "./DeployModel";
import { LocaleProvider } from "../i18n/provider";
import { keys } from "../lib/queries";
import { ToastProvider } from "../lib/toast";

const MACHINES = [
  { node_id: "ksia", label: "ksia" },
  { node_id: "mac", label: "mac" },
  { node_id: "salem", label: "salem" },
];

/** The plan of a whole model: one step, every layer, one machine. */
function wholePlan(target = "ksia", mmproj_path: string | null = null): DeployPlan {
  return {
    group: "qwen3.5-9b",
    kind: "whole",
    mmproj_path,
    total_layers: 32,
    total_layers_inferred: false,
    steps: [
      {
        position: 1,
        start_layer: 0,
        end_layer: 31,
        total_layers: 32,
        name: "qwen3.5-9b",
        size_bytes: 5_000_000_000,
        target_node_id: target,
        target_host: target,
        ready: target === "ksia",
        source_node_id: target === "ksia" ? null : "ksia",
        source_host: target === "ksia" ? null : "ksia",
        source_path: target === "ksia" ? null : "/lm/qwen3.5-9b.gguf",
        target_path: null,
        primary: true,
        n_gpu_layers: null,
        offload_kqv: null,
        options: MACHINES.map((m) => ({ ...m, has: m.node_id === "ksia", ok: true, note: null })),
        problem: null,
        note: null,
        merged: false,
        parts: [],
      },
    ],
    complete: true,
    transfer_bytes: 0,
    blocked: null,
    auto: true,
    merge: false,
    available: [
      {
        start_layer: 0,
        end_layer: 31,
        total_layers: 32,
        name: "qwen3.5-9b",
        size_bytes: 5_000_000_000,
        holders: ["ksia"],
        selected: true,
      },
    ],
    machines: MACHINES.map((m) => ({
      ...m,
      mem_free_mb: 100_000,
      disk_free_mb: 500_000,
      holds: [] as Array<[number, number]>,
      taking: null,
    })),
    missing: [],
    overlap: [],
    warnings: [],
    params: {
      n_ctx: 4096,
      n_batch: 512,
      max_sessions: 1,
      max_new_tokens: 256,
      prefill_chunk_tokens: 512,
      n_gpu_layers: 0,
      offload_kqv: true,
      fit_memory: true,
      fit_memory_margin_mb: 256,
      speculative_decoding_enabled: false,
      speculative_k: 4,
      speculative_lookup_window: 3,
      mtp_enabled: false,
      mtp_draft_tokens: 3,
      mtp_min_probability: 0,
    } as DeployPlan["params"],
    prompt_tokens: 3840,
    chunk_tokens: 512,
  };
}

function wholePlanWithName(name: string, target = "ksia"): DeployPlan {
  const plan = wholePlan(target);
  plan.group = name;
  plan.steps[0].name = name;
  plan.available[0].name = name;
  return plan;
}

/** The same plan, but over slices - what the screen asks for by default. */
function shardPlan(): DeployPlan {
  const p = wholePlan();
  p.kind = "shards";
  p.group = "qwen";
  p.available[0].name = "qwen_0_31.gguf";
  return p;
}

function tensorPlan(): DeployPlan {
  const p = wholePlan();
  p.kind = "tensor_parallel";
  return p;
}

function moePlan(kind: DeployPlan["kind"] = "whole"): DeployPlan {
  const p = wholePlan();
  p.kind = kind;
  p.is_moe = true;
  p.inference_mode_supported = kind !== "tensor_parallel";
  p.inference_mode_reason = kind === "tensor_parallel"
    ? "Tensor Parallelism não suporta MoE nesta versão."
    : null;
  p.params = { ...p.params, inference_mode: "auto" };
  return p;
}

/** A machine with the whole GGUF **and** a set of slices, so that both
 *  options exist and the switch between them is real. */
function mount(
  wholeModelName = "qwen3.5-9b",
  shardGroup = "qwen",
  shardLabel = shardGroup,
) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, refetchInterval: false } },
  });
  client.setQueryData(keys.jobs, { jobs: [] });
  client.setQueryData(keys.node, { connection_key: "", pom_dev_mode: true });
  if (livePeers.length > 0) client.setQueryData(keys.cluster, { peers: livePeers });
  client.setQueryData(keys.clusterModels, {
    peers: [
      {
        node_id: "ksia",
        hostname: "ksia",
        is_self: false,
        models: [
          {
            id: "1",
            name: wholeModelName,
            source: "lmstudio",
            source_label: "LM Studio",
            path: `/lm/${wholeModelName}`,
            size_bytes: 5_000_000_000,
            architecture: "qwen3",
            total_layers: 32,
            quant: "Q4_K_M",
            shard_range: null,
            shard_group: null,
          },
        ],
        shard_sets: [{
          group: shardGroup,
          model_label: shardLabel,
          pom_id: "pom-qwen",
          total_layers: 32,
          total_layers_inferred: false,
          shards: [],
          tp_parts: [
            {
              id: "tp-0",
              name: "qwen3.5-9b · L0-31 · rank 0",
              source: "local",
              source_label: "Local",
              path: "/shards/tp0.gguf",
              size_bytes: 2_500_000_000,
              architecture: "qwen3",
              total_layers: 32,
              quant: "Q4_K_M",
              shard_range: [0, 31],
              shard_group: shardGroup,
              sha256: "tp0",
              model_label: shardLabel,
              pom_id: "pom-qwen",
              tp_rank: 0,
              tp_size: 2,
            },
            {
              id: "tp-1",
              name: "qwen3.5-9b · L0-31 · rank 1",
              source: "local",
              source_label: "Local",
              path: "/shards/tp1.gguf",
              size_bytes: 2_500_000_000,
              architecture: "qwen3",
              total_layers: 32,
              quant: "Q4_K_M",
              shard_range: [0, 31],
              shard_group: shardGroup,
              sha256: "tp1",
              model_label: shardLabel,
              pom_id: "pom-qwen",
              tp_rank: 1,
              tp_size: 2,
            },
          ],
          complete: true,
        }],
      },
      ...["mac", "salem"].map((node_id, index) => ({
        node_id,
        hostname: node_id,
        is_self: false,
        models: [
          {
            id: String(index + 2),
            name: wholeModelName,
            source: "lmstudio",
            source_label: "LM Studio",
            path: `/lm/${wholeModelName}`,
            size_bytes: 5_000_000_000,
            architecture: "qwen3",
            total_layers: 32,
            quant: "Q4_K_M",
            shard_range: null,
            shard_group: null,
          },
        ],
        shard_sets: node_id === "mac" ? [{
          group: shardGroup,
          model_label: shardLabel,
          pom_id: "pom-qwen",
          total_layers: 32,
          total_layers_inferred: false,
          shards: [],
          tp_parts: [{
            id: "tp-1-mac",
            name: "qwen3.5-9b · L0-31 · rank 1",
            source: "local",
            source_label: "Local",
            path: "/shards/tp1.gguf",
            size_bytes: 2_500_000_000,
            architecture: "qwen3",
            total_layers: 32,
            quant: "Q4_K_M",
            shard_range: [0, 31],
            shard_group: shardGroup,
            sha256: "tp1",
            model_label: shardLabel,
            pom_id: "pom-qwen",
            tp_rank: 1,
            tp_size: 2,
          }],
          complete: false,
        }] : [],
      })),
    ],
  });
  const view = render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <ToastProvider>
          <MemoryRouter><DeployModel /></MemoryRouter>
        </ToastProvider>
      </LocaleProvider>
    </QueryClientProvider>,
  );
  return { ...view, client };
}

/** Answers each route with the plan that matches the requested `kind`. */
function answerPlans() {
  post.mockImplementation((url: string, body: { kind?: string }) => {
    if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
    return Promise.resolve(
      body?.kind === "whole"
        ? wholePlan()
        : body?.kind === "tensor_parallel"
          ? tensorPlan()
          : shardPlan(),
    );
  });
}

const planCalls = () =>
  post.mock.calls.filter(([url]) => url === "/api/ui/deploy/plan");

const chooseWhole = () =>
  fireEvent.click(screen.getByRole("radio", { name: /Um modelo inteiro/ }));

/** Slices are no longer the default: Pipeline (the whole model) is. */
async function chooseSlices() {
  fireEvent.click(await screen.findByRole("radio", { name: "Fatias (legado)" }));
  await waitFor(() =>
    expect(planCalls().some(([, body]) => (body as { kind?: string }).kind === "shards")).toBe(true),
  );
}

const chooseTensor = () => fireEvent.click(screen.getByRole("radio", { name: "Tensor" }));

async function ensureWholeMachine(node = "ksia") {
  const removeName = new RegExp(`Remover ${node} da implantação`);
  const addName = new RegExp(`${node} RTT`);
  await waitFor(() => {
    expect(
      screen.queryByRole("button", { name: removeName })
        ?? screen.queryByRole("button", { name: addName }),
    ).toBeTruthy();
  });
  if (!screen.queryByRole("button", { name: removeName })) {
    fireEvent.click(screen.getByRole("button", { name: addName }));
  }
  await waitFor(() =>
    expect(screen.getByRole("button", { name: removeName })).toBeTruthy(),
  );
}

describe("DeployModel · whole model", () => {
  beforeEach(() => {
    post.mockReset();
    localStorage.clear();
  });
  afterEach(cleanup);

  it("asks the node about the whole model by default, and about slices when chosen", async () => {
    answerPlans();
    mount();

    await waitFor(() => expect(planCalls().length).toBeGreaterThan(0));
    expect(planCalls()[0][1]).toMatchObject({ group: "qwen3.5-9b", kind: "whole" });

    await chooseSlices();
    expect(planCalls().at(-1)![1]).toMatchObject({ group: "qwen", kind: "shards" });
  });

  it("names the native studio fields for accessibility and password managers", async () => {
    answerPlans();
    mount();

    await waitFor(() => expect(planCalls().length).toBeGreaterThan(0));
    for (const field of document.querySelectorAll("input, select, textarea")) {
      expect(field.getAttribute("name") || field.getAttribute("id")).toBeTruthy();
    }
  });

  it("makes a single query when the model changes", async () => {
    post.mockImplementation((url: string, body: { group?: string; kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      const plan = body.kind === "whole" ? wholePlan() : shardPlan();
      plan.group = body.group ?? plan.group;
      return Promise.resolve(plan);
    });
    const { client } = mount();
    await waitFor(() => expect(planCalls().length).toBeGreaterThan(0));

    const catalog = client.getQueryData<{ peers: PeerCatalog[] }>(keys.clusterModels)!;
    const first = catalog.peers[0].models[0];
    client.setQueryData(keys.clusterModels, {
      ...catalog,
      peers: catalog.peers.map((peer, index) =>
        index === 0
          ? {
              ...peer,
              models: [
                ...peer.models,
                { ...first, id: "4", name: "zeta-2b", path: "/lm/zeta-2b.gguf" },
              ],
            }
          : peer,
      ),
    });

    chooseWhole();
    await waitFor(() => {
      expect(planCalls().at(-1)?.[1]).toMatchObject({
        group: "qwen3.5-9b",
        kind: "whole",
      });
    });
    const before = planCalls().length;
    const model = screen.getByRole("combobox", { name: "Modelo técnico" });
    fireEvent.change(model, { target: { value: "zeta-2b" } });

    await waitFor(() => {
      const requests = planCalls().filter(([, body]) => body.group === "zeta-2b");
      expect(requests).toHaveLength(1);
      expect(requests[0][1]).not.toHaveProperty("deployment_name");
      expect(requests[0][1]).not.toHaveProperty("params");
    });
    await new Promise((resolve) => setTimeout(resolve, 350));
    expect(planCalls()).toHaveLength(before + 1);
  });

  it("does not re-query the plan when adjusting the GPU layers of the machine", async () => {
    answerPlans();
    mount();
    await chooseSlices();
    await waitFor(() => expect(screen.getByRole("button", { name: /ksia na implantação/ })).toBeTruthy());

    const before = planCalls().length;
    const gpuLayers = screen.getByRole("spinbutton", { name: "Camadas GPU" });
    fireEvent.change(gpuLayers, { target: { value: "8" } });

    await waitFor(() => expect((gpuLayers as HTMLInputElement).value).toBe("8"));
    await new Promise((resolve) => setTimeout(resolve, 350));
    expect(planCalls()).toHaveLength(before);
  });

  it("keeps the indicator until the model query finishes", async () => {
    let resolveWhole: ((plan: DeployPlan) => void) | undefined;
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      if (body.kind === "whole") {
        return new Promise<DeployPlan>((resolve) => {
          resolveWhole = resolve;
        });
      }
      return Promise.resolve(shardPlan());
    });
    mount();
    await chooseSlices();

    chooseWhole();
    await waitFor(() => expect(screen.getByText("conferindo com o nó…")).toBeTruthy());
    expect(resolveWhole).toBeTypeOf("function");

    resolveWhole!(wholePlan());
    await waitFor(() => expect(screen.queryByText("conferindo com o nó…")).toBeNull());
  });

  it("does not show the whole model facts section in the inspector", async () => {
    answerPlans();
    mount();
    chooseWhole();

    await waitFor(() => {
      expect(planCalls().some(([, body]) => (body as { kind?: string }).kind === "whole")).toBe(true);
    });
    expect(screen.queryByText("Quantização")).toBeNull();
    expect(screen.queryByText("Camadas")).toBeNull();
    expect(screen.queryByText("Tamanho")).toBeNull();
    expect(screen.queryByText("Está em")).toBeNull();
    expect(screen.queryByRole("combobox", { name: /Máquina que serve o modelo inteiro/ })).toBeNull();
  });

  /**
   * The point of the reported defect: on a whole model there is no range to
   * check and no layer claimed twice, and showing those controls is
   * inventing a decision that does not exist.
   */
  it("shows neither the coverage strip nor the range marking column", async () => {
    answerPlans();
    mount();
    await chooseSlices();

    await waitFor(() => expect(screen.getByText(/Cobertura do modelo/)).toBeTruthy());
    expect(screen.getAllByText(/L0–31/).length).toBeGreaterThan(0);

    chooseWhole();

    await waitFor(() => {
      expect(screen.queryByText(/Cobertura do modelo/)).toBeNull();
    });
    expect(screen.queryByRole("checkbox", { name: /Incluir/ })).toBeNull();
    // The switch starts with the map empty; the range only shows after the
    // explicit choice of a machine.
    expect(screen.queryByText(/L0–31/)).toBeNull();
  });

  it("shows the model name without extension, keeping the technical value in the request", async () => {
    const technicalName = "Qwen3.8-27B-UD-IQ3_XXS.gguf";
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      return Promise.resolve(body.kind === "whole" ? wholePlanWithName(technicalName) : shardPlan());
    });
    mount(technicalName);
    chooseWhole();

    const model = await screen.findByRole("combobox", { name: "Modelo técnico" });
    expect(within(model).getByRole("option", { name: "Qwen3.8-27B-UD-IQ3_XXS" })).toBeTruthy();
    expect(within(model).queryByRole("option", { name: technicalName })).toBeNull();
    await waitFor(() => {
      expect(planCalls().at(-1)?.[1]).toMatchObject({ group: technicalName });
    });
  });

  it("preserves legitimate hyphens and digits in the displayed name", async () => {
    const technicalName = "Qwen3.8-27B-UD-IQ3_XXS-v2-2024.gguf";
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      return Promise.resolve(body.kind === "whole" ? wholePlanWithName(technicalName) : shardPlan());
    });
    mount(technicalName);
    chooseWhole();

    const model = await screen.findByRole("combobox", { name: "Modelo técnico" });
    expect(within(model).getByRole("option", { name: "Qwen3.8-27B-UD-IQ3_XXS-v2-2024" })).toBeTruthy();
  });

  it("hides the round hash in the slice options", async () => {
    const shardName = "Ornith-1.5-35B-Q4_K_M-42739874";
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      const plan = shardPlan();
      plan.group = shardName;
      return Promise.resolve(plan);
    });
    mount("qwen3.5-9b", shardName, shardName);
    fireEvent.click(await screen.findByRole("radio", { name: "Fatias (legado)" }));

    const model = await screen.findByRole("combobox", { name: "Modelo técnico" });
    expect(within(model).getByRole("option", { name: "Ornith-1.5-35B-Q4_K_M" })).toBeTruthy();
    expect(within(model).queryByRole("option", { name: shardName })).toBeNull();
  });

  it("hides the round hash in the Tensor option", async () => {
    const tensorName = "Ornith-1.5-35B-Q4_K_M-b3bdbf23";
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      const plan = tensorPlan();
      plan.group = tensorName;
      return Promise.resolve(plan);
    });
    mount("qwen3.5-9b", tensorName, tensorName);
    chooseTensor();
    const model = await screen.findByRole("combobox", { name: "Modelo técnico" });
    await waitFor(() => {
      expect(within(model).getByRole("option", { name: "Ornith-1.5-35B-Q4_K_M · TP 2" })).toBeTruthy();
    });
    expect(within(model).queryByRole("option", { name: `${tensorName} · TP 2` })).toBeNull();
  });

  it("empties the map when switching from slices to whole model", async () => {
    answerPlans();
    mount();
    await chooseSlices();

    await waitFor(() => expect(screen.getByRole("button", { name: /mac RTT/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: /mac RTT/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Remover ksia da implantação/ })).toBeTruthy());

    const whole = screen.getByRole("radio", { name: "Um modelo inteiro" });
    fireEvent.click(whole);

    expect(whole.getAttribute("aria-checked")).toBe("true");
    await waitFor(() => {
      expect(planCalls().some(([, body]) => (body as { kind?: string }).kind === "whole")).toBe(true);
    });
    expect(screen.queryByRole("button", { name: /Remover ksia da implantação/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /Remover mac da implantação/ })).toBeNull();
  });

  it("keeps the whole range on the only machine of the map", async () => {
    answerPlans();
    mount();
    chooseWhole();

    await waitFor(() => {
      expect(planCalls().some(([, body]) => (body as { kind?: string }).kind === "whole")).toBe(true);
    });
    fireEvent.click(await screen.findByRole("button", { name: /ksia RTT/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Remover ksia da implantação/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: /Remover ksia da implantação/ }));
    const mac = await screen.findByRole("button", { name: /mac RTT/ });
    fireEvent.click(mac);

    await waitFor(() => {
      const last = planCalls().at(-1)![1] as { selection?: unknown };
      expect(last.selection).toEqual([
        {
          start_layer: 0,
          end_layer: 31,
          target_node_id: "mac",
          n_gpu_layers: null,
          offload_kqv: null,
        },
      ]);
    });
  });

  it("adds a second machine to the whole model map, without a range", async () => {
    answerPlans();
    mount();
    chooseWhole();

    await waitFor(() => {
      expect(planCalls().some(([, body]) => (body as { kind?: string }).kind === "whole")).toBe(true);
    });
    const ksia = await screen.findByRole("button", { name: /ksia RTT/ });
    fireEvent.click(ksia);
    await waitFor(() => expect(screen.getByRole("button", { name: /Remover ksia da implantação/ })).toBeTruthy());
    fireEvent.click(await screen.findByRole("button", { name: /mac RTT/ }));

    await waitFor(() => expect(screen.getByRole("button", { name: /Remover mac da implantação/ })).toBeTruthy());
    expect(screen.getByRole("button", { name: /Remover ksia da implantação/ })).toBeTruthy();
    await waitFor(() => {
      const last = planCalls().at(-1)![1] as { preferred_node_ids?: string[] };
      expect(last.preferred_node_ids).toEqual(["ksia", "mac"]);
    });
    // The new machine only gets layers when the operator assigns a range.
    expect(planCalls().some(([, body]) => (body as { selection?: Array<{ target_node_id: string }> }).selection?.some((row) => row.target_node_id === "mac"))).toBe(false);
  });

  it("splits the whole model over machines that hold it and sends the ranges, not a selection", async () => {
    answerPlans();
    // The node answers a range request with one step per range, as it does.
    const plain = post.getMockImplementation()!;
    post.mockImplementation((url: string, body: {
      whole_ranges?: Array<{ node_id: string; start_layer: number; end_layer: number }>;
      selection?: Array<{ target_node_id: string; start_layer: number; end_layer: number }>;
    }) => {
      const ranges = body?.whole_ranges
        ?? (body?.selection && body.selection.length > 1
          ? body.selection.map((row) => ({ node_id: row.target_node_id, start_layer: row.start_layer, end_layer: row.end_layer }))
          : undefined);
      if (url !== "/api/ui/deploy/plan" || !ranges) return plain(url, body);
      const base = wholePlan();
      return Promise.resolve({
        ...base,
        // A plan for ranges the operator named is never the node's own choice.
        auto: false,
        steps: ranges.map((range, index) => ({
          ...base.steps[0],
          position: index + 1,
          start_layer: range.start_layer,
          end_layer: range.end_layer,
          target_node_id: range.node_id,
          target_host: range.node_id,
          primary: range.start_layer === 0,
        })),
      });
    });
    mount();
    chooseWhole();
    fireEvent.click(await screen.findByRole("button", { name: /ksia RTT/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Remover ksia da implantação/ })).toBeTruthy());
    fireEvent.click(await screen.findByRole("button", { name: /mac RTT/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Remover mac da implantação/ })).toBeTruthy());

    fireEvent.click(await screen.findByRole("button", { name: /Dividir as camadas entre 2 máquinas/ }));
    type RangesBody = { whole_ranges?: Array<{ node_id: string; start_layer: number; end_layer: number }>; selection?: unknown };
    await waitFor(() => {
      const last = planCalls().at(-1)![1] as RangesBody;
      expect(last.whole_ranges).toEqual([
        { node_id: "ksia", start_layer: 0, end_layer: 15 },
        { node_id: "mac", start_layer: 16, end_layer: 31 },
      ]);
      expect(last.selection).toBeUndefined();
    });

    // The last machine gives a layer back by moving the cut before it.
    fireEvent.click(await screen.findByRole("button", { name: /Menos camadas em mac/ }));
    await waitFor(() => {
      const last = planCalls().at(-1)![1] as RangesBody;
      expect(last.whole_ranges?.map((range) => [range.start_layer, range.end_layer])).toEqual([[0, 16], [17, 31]]);
    });

    const button = await screen.findByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(button);
    await waitFor(() => {
      const deploy = post.mock.calls.find(([url]) => url === "/api/ui/deploy")?.[1] as RangesBody | undefined;
      expect(deploy?.whole_ranges?.map((range) => range.node_id)).toEqual(["ksia", "mac"]);
      expect(deploy?.selection).toBeUndefined();
    });
  });

  it("locks a range and auto-balances the unlocked ones by free memory", async () => {
    answerPlans();
    const plain = post.getMockImplementation()!;
    const memory: Record<string, number> = { ksia: 3000, mac: 1000, salem: 1000 };
    post.mockImplementation((url: string, body: { whole_ranges?: Array<{ node_id: string; start_layer: number; end_layer: number }> }) => {
      if (url !== "/api/ui/deploy/plan" || !body?.whole_ranges) return plain(url, body);
      const base = wholePlan();
      return Promise.resolve({
        ...base,
        auto: false,
        // One byte per layer: every layer is a valid cut.
        layer_options: {
          source_node_id: "ksia",
          total_layers: 32,
          cuts: Array.from({ length: 31 }, (_, end) => ({ end_layer: end, layers: end + 1, size_bytes: end + 1, remainder_bytes: 31 - end })),
          granularity: 1,
          rule: "every layer",
          reason: null,
        },
        machines: base.machines.map((machine) => ({ ...machine, gpu_free_mb: memory[machine.node_id], ram_free_mb: 8000 })),
        steps: body.whole_ranges.map((range, index) => ({
          ...base.steps[0],
          position: index + 1,
          start_layer: range.start_layer,
          end_layer: range.end_layer,
          target_node_id: range.node_id,
          target_host: range.node_id,
          primary: range.start_layer === 0,
        })),
      });
    });
    mount();
    chooseWhole();
    fireEvent.click(await screen.findByRole("button", { name: /ksia RTT/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Remover ksia da implantação/ })).toBeTruthy());
    fireEvent.click(await screen.findByRole("button", { name: /mac RTT/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Remover mac da implantação/ })).toBeTruthy());
    fireEvent.click(await screen.findByRole("button", { name: /Dividir as camadas entre 2 máquinas/ }));

    type RangesBody = { whole_ranges?: Array<{ node_id: string; start_layer: number; end_layer: number }> };
    const lastRanges = () => (planCalls().at(-1)![1] as RangesBody).whole_ranges?.map((range) => [range.start_layer, range.end_layer]);
    await waitFor(() => expect(lastRanges()).toEqual([[0, 15], [16, 31]]));

    const balance = await screen.findByRole("button", { name: /Equilibrar pela memória livre/ });
    await waitFor(() => expect((balance as HTMLButtonElement).disabled).toBe(false));

    fireEvent.click(screen.getByRole("button", { name: /Travar a faixa de mac/ }));
    const fewer = screen.getByRole("button", { name: /Menos camadas em mac/ }) as HTMLButtonElement;
    expect(fewer.disabled).toBe(true);
    expect(fewer.title).toMatch(/travada/);
    expect((screen.getByRole("button", { name: /Equilibrar pela memória livre/ }) as HTMLButtonElement).disabled).toBe(true);

    fireEvent.click(screen.getByRole("button", { name: /Destravar a faixa de mac/ }));
    fireEvent.click(screen.getByRole("button", { name: /Equilibrar pela memória livre/ }));
    await waitFor(() => expect(lastRanges()).toEqual([[0, 23], [24, 31]]));
  });

  it("auto-balance counts the KV cache the node costs for each cut", async () => {
    answerPlans();
    const plain = post.getMockImplementation()!;
    // 1 byte of weights per layer; 3 bytes of KV per layer from layer 16 on.
    const kv = (end: number) => Math.max(0, end - 15) * 3;
    post.mockImplementation((url: string, body: { whole_ranges?: Array<{ node_id: string; start_layer: number; end_layer: number }> }) => {
      if (url !== "/api/ui/deploy/plan" || !body?.whole_ranges) return plain(url, body);
      const base = wholePlan();
      return Promise.resolve({
        ...base,
        auto: false,
        layer_options: {
          source_node_id: "ksia",
          total_layers: 32,
          cuts: Array.from({ length: 31 }, (_, end) => ({
            end_layer: end, layers: end + 1, size_bytes: end + 1, remainder_bytes: 31 - end, kv_bytes: kv(end), remainder_kv_bytes: kv(31) - kv(end),
          })),
          granularity: 1,
          rule: "every layer",
          reason: null,
        },
        machines: base.machines.map((machine) => ({ ...machine, gpu_free_mb: 1000, ram_free_mb: 8000 })),
        steps: body.whole_ranges.map((range, index) => ({
          ...base.steps[0],
          position: index + 1,
          start_layer: range.start_layer,
          end_layer: range.end_layer,
          target_node_id: range.node_id,
          target_host: range.node_id,
          primary: range.start_layer === 0,
        })),
      });
    });
    mount();
    chooseWhole();
    fireEvent.click(await screen.findByRole("button", { name: /ksia RTT/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Remover ksia da implantação/ })).toBeTruthy());
    fireEvent.click(await screen.findByRole("button", { name: /mac RTT/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Remover mac da implantação/ })).toBeTruthy());
    fireEvent.click(await screen.findByRole("button", { name: /Dividir as camadas entre 2 máquinas/ }));
    type RangesBody = { whole_ranges?: Array<{ start_layer: number; end_layer: number }> };
    const lastRanges = () => (planCalls().at(-1)![1] as RangesBody).whole_ranges?.map((range) => [range.start_layer, range.end_layer]);
    await waitFor(() => expect(lastRanges()).toEqual([[0, 15], [16, 31]]));
    const balance = await screen.findByRole("button", { name: /Equilibrar pela memória livre/ });
    await waitFor(() => expect((balance as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(balance);
    await waitFor(() => expect(lastRanges()).toEqual([[0, 21], [22, 31]]));
  });

  it("shows how many layers run on the GPU and how many on the CPU", async () => {
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url !== "/api/ui/deploy/plan" || body?.kind !== "whole") return Promise.resolve(body?.kind === "whole" ? wholePlan() : shardPlan());
      const plan = wholePlan();
      plan.steps[0].capacity = { fits: true, degraded: true, note: null, gpus: [], gpu_layers_fit: 18, cpu_layers: 6, cpu_ram_bytes: 3 * 1024 ** 3 };
      return Promise.resolve(plan);
    });
    mount();
    chooseWhole();
    await ensureWholeMachine();
    expect(await screen.findByText(/18 na GPU, 6 na CPU/)).toBeTruthy();
  });

  it("shows in the machine tab the note the node gives a step", async () => {
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url !== "/api/ui/deploy/plan" || body?.kind !== "whole") return Promise.resolve(body?.kind === "whole" ? wholePlan() : shardPlan());
      const plan = wholePlan();
      plan.steps[0].note = "ksia cuts this range from its own copy: 5 GB written to disk (it does not announce model/range-load-v1)";
      return Promise.resolve(plan);
    });
    mount();
    chooseWhole();
    await ensureWholeMachine();
    expect(await screen.findByText(/ksia cuts this range from its own copy/)).toBeTruthy();
  });

  it("redoes the split on the GGUF's real cuts when the node refuses the first guess", async () => {
    answerPlans();
    const plain = post.getMockImplementation()!;
    const cuts = [11, 23];
    post.mockImplementation((url: string, body: { whole_ranges?: Array<{ node_id: string; start_layer: number; end_layer: number }> }) => {
      if (url !== "/api/ui/deploy/plan" || !body?.whole_ranges) return plain(url, body);
      const base = wholePlan();
      const layerOptions = { source_node_id: "ksia", total_layers: 32, cuts: cuts.map((end) => ({ end_layer: end, layers: end + 1, size_bytes: 1, remainder_bytes: 1 })), granularity: null, rule: "real boundaries", reason: null };
      const bad = body.whole_ranges.find((range) => range.end_layer !== 31 && !cuts.includes(range.end_layer));
      if (bad) {
        return Promise.resolve({ ...base, auto: false, blocked: `the cut at layer L${bad.end_layer} does not respect the real rules of the GGUF`, layer_options: layerOptions });
      }
      return Promise.resolve({
        ...base,
        auto: false,
        layer_options: layerOptions,
        steps: body.whole_ranges.map((range, index) => ({
          ...base.steps[0],
          position: index + 1,
          start_layer: range.start_layer,
          end_layer: range.end_layer,
          target_node_id: range.node_id,
          target_host: range.node_id,
          primary: range.start_layer === 0,
        })),
      });
    });
    mount();
    chooseWhole();
    fireEvent.click(await screen.findByRole("button", { name: /ksia RTT/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Remover ksia da implantação/ })).toBeTruthy());
    fireEvent.click(await screen.findByRole("button", { name: /mac RTT/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: /Remover mac da implantação/ })).toBeTruthy());
    fireEvent.click(await screen.findByRole("button", { name: /Dividir as camadas entre 2 máquinas/ }));

    type RangesBody = { whole_ranges?: Array<{ node_id: string; start_layer: number; end_layer: number }> };
    await waitFor(() => {
      const sent = planCalls().map(([, body]) => (body as RangesBody).whole_ranges).filter(Boolean);
      // The first guess cut at every-layer granularity; the second is on a real cut.
      expect(sent[0]?.map((range) => [range.start_layer, range.end_layer])).toEqual([[0, 15], [16, 31]]);
      expect(sent.at(-1)?.map((range) => [range.start_layer, range.end_layer])).toEqual([[0, 11], [12, 31]]);
    });
  });

  describe("with machines already in service", () => {
    const serving = (nodeId: string, model: string) => ({
      node_id: nodeId,
      model,
      start_layer: 0,
      end_layer: 31,
      total_layers: 32,
      load: 0,
      node: null,
    });
    afterEach(() => {
      livePeers = [];
    });

    async function pressDeploy() {
      answerPlans();
      mount();
      chooseWhole();
      await ensureWholeMachine();
      const button = await screen.findByRole("button", { name: /^Implantar$/ });
      await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false));
      fireEvent.click(button);
    }
    const deploys = () => post.mock.calls.filter(([url]) => url === "/api/ui/deploy");

    it("asks before taking over a machine of the plan", async () => {
      livePeers = [serving("ksia", "llama-3-8b")];
      await pressDeploy();

      const dialog = await screen.findByRole("dialog", { name: "Já existe modelo em serviço" });
      expect(dialog.textContent).toContain("llama-3-8b");
      expect(deploys()).toHaveLength(0);
      expect(screen.queryByRole("button", { name: "Manter em paralelo" })).toBeNull();

      fireEvent.click(screen.getByRole("button", { name: "Substituir e implantar" }));
      await waitFor(() => expect(deploys()).toHaveLength(1));
      expect(deploys()[0][1]).not.toHaveProperty("keep_existing");
    });

    it("keeps in parallel a machine outside the plan that serves the same model", async () => {
      livePeers = [serving("mac", "qwen3.5-9b")];
      await pressDeploy();

      await screen.findByRole("dialog", { name: "Já existe modelo em serviço" });
      expect(screen.getByRole("button", { name: "Desimplantar e implantar" })).toBeTruthy();
      fireEvent.click(screen.getByRole("button", { name: "Manter em paralelo" }));
      await waitFor(() => expect(deploys()).toHaveLength(1));
      expect(deploys()[0][1]).toMatchObject({ kind: "whole", keep_existing: true });
    });

    it("does not ask about a machine outside the plan that serves another model", async () => {
      livePeers = [serving("mac", "llama-3-8b")];
      await pressDeploy();
      await waitFor(() => expect(deploys()).toHaveLength(1));
      expect(screen.queryByRole("dialog", { name: "Já existe modelo em serviço" })).toBeNull();
    });
  });

  it("deploys sending the same `kind` it planned", async () => {
    answerPlans();
    mount();
    chooseWhole();
    await ensureWholeMachine();

    const button = await screen.findByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(button);

    await waitFor(() => {
      const deploys = post.mock.calls.filter(([url]) => url === "/api/ui/deploy");
      expect(deploys).toHaveLength(1);
      expect(deploys[0][1]).toMatchObject({ group: "qwen3.5-9b", kind: "whole" });
    });
  });

  it("shows the vision option only when the plan has an mmproj", async () => {
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      return Promise.resolve(body?.kind === "whole" ? wholePlan("ksia", "/lm/mmproj.gguf") : shardPlan());
    });
    mount();
    chooseWhole();
    await waitFor(() => expect(planCalls().some(([, body]) => (body as { kind?: string }).kind === "whole")).toBe(true));
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));

    expect(screen.getByRole("checkbox", { name: /Ativar visão/ })).toBeTruthy();
    expect(screen.getByText("/lm/mmproj.gguf")).toBeTruthy();
  });

  it("shows why vision is not available when the plan has no mmproj", async () => {
    answerPlans();
    mount();
    chooseWhole();
    await waitFor(() => expect(planCalls().some(([, body]) => (body as { kind?: string }).kind === "whole")).toBe(true));
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));

    expect(screen.queryByRole("checkbox", { name: "Ativar visão" })).toBeNull();
    expect(screen.getByRole("alert").textContent).toContain(
      "Visão indisponível: não foi possível provar a que modelo pertence o projetor.",
    );
  });

  /**
   * The node's refusal has to show here as it shows on slices. Without this the
   * operator sees a grey button and no reason, which is the defect the sibling
   * suite guards for the other path.
   */
  it("shows the node refusal and keeps the button gray", async () => {
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      if (body?.kind !== "whole") return Promise.resolve(shardPlan());
      const p = wholePlan("");
      p.blocked = "o modelo qwen3.5-9b está sem máquina";
      p.steps[0].problem = "o modelo qwen3.5-9b está sem máquina";
      return Promise.resolve(p);
    });
    mount();
    chooseWhole();

    await waitFor(() => {
      expect(screen.getByText(/Não dá para implantar deste jeito/)).toBeTruthy();
    });
    expect(screen.getAllByText(/está sem máquina/).length).toBeGreaterThan(0);
    const button = screen.getByRole("button", { name: /^Implantar$/ });
    expect((button as HTMLButtonElement).disabled).toBe(true);
  });

  it("does not show duplicate facts when the chosen machine needs a copy", async () => {
    post.mockImplementation((url: string, body: { kind?: string }) =>
      Promise.resolve(body?.kind === "whole" ? wholePlan("mac") : shardPlan()),
    );
    mount();
    chooseWhole();

    await waitFor(() => {
      expect(planCalls().some(([, body]) => (body as { kind?: string }).kind === "whole")).toBe(true);
    });
    expect(screen.queryByText(/copiar de ksia/)).toBeNull();
  });

  /**
   * A node on an old binary returns no `kind` at all, and it only knows how to
   * answer about slices. Reading the absence as "slices" is what keeps the screen
   * from going mute during an update in progress.
   */
  it("refuses by name a machine without the model instead of offering a copy", async () => {
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      if (body.kind !== "whole") return Promise.resolve(shardPlan());
      const plan = wholePlan();
      plan.copies = [{
        kind: "slice",
        name: "qwen3.5-9b",
        size_bytes: 1,
        layers: [16, 31],
        source_node_id: "ksia",
        source_host: "ksia",
        target_node_id: "mac",
        target_host: "mac",
      }];
      return Promise.resolve(plan);
    });
    mount();
    await ensureWholeMachine();
    expect(await screen.findByText(/Sem o modelo: mac/)).toBeTruthy();
    const button = screen.getByRole("button", { name: /^Implantar$/ }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
  });

  it("reads a plan without `kind` as a slices plan", async () => {
    post.mockImplementation(() => {
      const p = shardPlan();
      delete (p as { kind?: string }).kind;
      return Promise.resolve(p);
    });
    mount();
    fireEvent.click(await screen.findByRole("radio", { name: "Fatias (legado)" }));

    await waitFor(() => {
      const button = screen.getByRole("button", { name: /^Implantar$/ });
      expect((button as HTMLButtonElement).disabled).toBe(false);
    });
    expect(screen.getAllByText(/L0–31/).length).toBeGreaterThan(0);
  });

  it("allows turning the automatic memory fit on and off", async () => {
    answerPlans();
    mount();
    chooseWhole();

    const toggle = await screen.findByRole("checkbox", {
      name: /Ajustar à memória/,
    });
    expect(toggle.getAttribute("aria-checked")).toBe("true");

    const before = planCalls().length;
    fireEvent.click(toggle);
    await waitFor(() => {
      expect(toggle.getAttribute("aria-checked")).toBe("false");
    });
    await new Promise((resolve) => setTimeout(resolve, 350));
    expect(planCalls()).toHaveLength(before);
    expect(planCalls().every(([, body]) => !Object.prototype.hasOwnProperty.call(body, "params"))).toBe(true);
    expect(screen.queryByText("conferindo com o nó…")).toBeNull();
  });

  it("keeps the engine selector hidden for a dense model", async () => {
    answerPlans();
    mount();
    chooseWhole();

    await waitFor(() => expect(planCalls().length).toBeGreaterThan(0));
    expect(screen.queryByRole("combobox", { name: "Motor de inferência" })).toBeNull();
  });

  it("shows and sends the engine choice in the whole model", async () => {
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      return Promise.resolve(moePlan(body.kind as DeployPlan["kind"]));
    });
    mount();
    chooseWhole();
    await ensureWholeMachine();
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));

    const selector = await screen.findByRole("combobox", { name: "Motor de inferência" });
    fireEvent.change(selector, { target: { value: "moe" } });
    expect((selector as HTMLSelectElement).value).toBe("moe");

    const button = screen.getByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(button);
    await waitFor(() => {
      const deploy = post.mock.calls.find(([url]) => url === "/api/ui/deploy");
      expect(deploy?.[1]).toMatchObject({
        kind: "whole",
        params: { inference_mode: "moe" },
      });
    });
  });

  it("carries the choice to slices and shows the Tensor Parallelism limitation", async () => {
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      return Promise.resolve(moePlan((body.kind ?? "shards") as DeployPlan["kind"]));
    });
    mount();

    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));
    const selector = await screen.findByRole("combobox", { name: "Motor de inferência" });
    fireEvent.change(selector, { target: { value: "dense" } });
    expect((selector as HTMLSelectElement).value).toBe("dense");

    chooseTensor();
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));
    await waitFor(() =>
      expect(screen.getByText("Tensor Parallelism não suporta MoE nesta versão.")).toBeTruthy(),
    );
    expect(screen.getByRole("combobox", { name: "Motor de inferência" })).toBeTruthy();
  });
});

describe("DeployModel · MTP", () => {
  beforeEach(() => {
    post.mockReset();
    localStorage.clear();
  });
  afterEach(cleanup);

  /** Whole plan with MTP off, which is the honest default. */
  function mountWhole(mtp = false) {
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      const p = wholePlan();
      (p.params as RuntimeParams).mtp_enabled = mtp;
      return Promise.resolve(p);
    });
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false, refetchInterval: false } },
    });
    client.setQueryData(keys.jobs, { jobs: [] });
    client.setQueryData(keys.clusterModels, {
      peers: [
        {
          node_id: "ksia",
          hostname: "ksia",
          is_self: false,
          models: [
            {
              id: "1",
              name: "qwen3.5-9b",
              source: "lmstudio",
              source_label: "LM Studio",
              path: "/lm/qwen3.5-9b.gguf",
              size_bytes: 5_000_000_000,
              architecture: "qwen3",
              total_layers: 32,
              quant: "Q4_K_M",
              shard_range: null,
              shard_group: null,
            },
          ],
          shard_sets: [{ group: "qwen", shards: [], tp_parts: [] }],
        },
      ],
    });
    return render(
      <QueryClientProvider client={client}>
        <LocaleProvider initialLocale="pt-BR">
          <MemoryRouter><DeployModel /></MemoryRouter>
        </LocaleProvider>
      </QueryClientProvider>,
    );
  }

  /** Speculation is a single choice: none, MTP or lookup. */
  async function chooseSpeculation(mode: "none" | "mtp" | "lookup") {
    const select = await screen.findByRole("combobox", { name: "Especulação" });
    fireEvent.change(select, { target: { value: mode } });
  }

  it("turns MTP on, sees the numeric controls and sends them in the deployment", async () => {
    mountWhole();
    chooseWhole();
    await ensureWholeMachine();
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));

    await chooseSpeculation("mtp");

    const placement = (await screen.findByRole("combobox", {
      name: "Onde o MTP fica",
    })) as HTMLSelectElement;
    expect(placement.value).toBe("auto");
    fireEvent.change(placement, { target: { value: "vram" } });

    await waitFor(() => {
      expect(screen.getByText("Tokens rascunhados pelo MTP")).toBeTruthy();
      expect(
        screen.getByText("Probabilidade mínima do MTP"),
      ).toBeTruthy();
    });

    // Tune the draft token count.
    const draftInput = screen.getByRole("spinbutton", {
      name: /Tokens rascunhados pelo MTP/,
    });
    fireEvent.change(draftInput, { target: { value: "5" } });
    await waitFor(() => {
      expect((draftInput as HTMLInputElement).value).toBe("5");
    });

    // Fire the deploy and check the params.
    const button = screen.getByRole("button", { name: /^Implantar$/ });
    await waitFor(() =>
      expect((button as HTMLButtonElement).disabled).toBe(false),
    );
    fireEvent.click(button);

    await waitFor(() => {
      const deploys = post.mock.calls.filter(
        ([url]) => url === "/api/ui/deploy",
      );
      expect(deploys).toHaveLength(1);
      const body = deploys[0][1] as {
        kind: string;
        params: RuntimeParams;
      };
      expect(body.kind).toBe("whole");
      expect(body.params.mtp_enabled).toBe(true);
      expect(body.params.mtp_placement).toBe("vram");
      expect(body.params.mtp_draft_tokens).toBe(5);
    });
  });

  it("checking MTP fills in 0.75 and allows editing the minimum probability", async () => {
    mountWhole();
    chooseWhole();
    await ensureWholeMachine();
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));

    await chooseSpeculation("mtp");

    await waitFor(() => {
      expect(screen.getByText("Probabilidade mínima do MTP")).toBeTruthy();
      expect(
        (screen.getByRole("spinbutton", {
          name: /Probabilidade mínima do MTP/,
        }) as HTMLInputElement).value,
      ).toBe("0.75");
    });

    const probabilityInput = screen.getByRole("spinbutton", {
      name: /Probabilidade mínima do MTP/,
    });
    fireEvent.change(probabilityInput, { target: { value: "0.42" } });
    await waitFor(() =>
      expect((probabilityInput as HTMLInputElement).value).toBe("0.42"),
    );

    const button = screen.getByRole("button", { name: /^Implantar$/ });
    await waitFor(() =>
      expect((button as HTMLButtonElement).disabled).toBe(false),
    );
    fireEvent.click(button);

    await waitFor(() => {
      const deploys = post.mock.calls.filter(([url]) => url === "/api/ui/deploy");
      expect(deploys).toHaveLength(1);
      const body = deploys[0][1] as { params: RuntimeParams };
      expect(body.params.mtp_enabled).toBe(true);
      expect(body.params.mtp_min_probability).toBe(0.42);
    });
  });

  /// "Voltar ao plano do nó" has to undo what is half typed **as
  /// well**.
  ///
  /// The text being edited lives inside `DeploymentInspector` since it left
  /// this screen. What cleared the draft here was a `setDrafts({})` that no
  /// longer exists: without remounting the form, the button zeroed the parameters
  /// underneath and the field kept showing the old number - the screen saying one
  /// thing and the request carrying another.
  it("restoring the node plan also clears the half-typed field", async () => {
    post.mockImplementation((url: string) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      return Promise.resolve(wholePlan());
    });
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false, refetchInterval: false } },
    });
    client.setQueryData(keys.jobs, { jobs: [] });
    client.setQueryData(keys.clusterModels, {
      peers: [
        {
          node_id: "ksia",
          hostname: "ksia",
          is_self: false,
          models: [
            {
              id: "1",
              name: "qwen3.5-9b",
              source: "lmstudio",
              source_label: "LM Studio",
              path: "/lm/qwen3.5-9b.gguf",
              size_bytes: 5_000_000_000,
              architecture: "qwen3",
              total_layers: 32,
              quant: "Q4_K_M",
              shard_range: null,
              shard_group: null,
            },
          ],
        },
      ],
    });
    render(
      <QueryClientProvider client={client}>
        <LocaleProvider initialLocale="pt-BR">
          <MemoryRouter><DeployModel /></MemoryRouter>
        </LocaleProvider>
      </QueryClientProvider>,
    );

    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));

    const field = () =>
      document.querySelector('input[name="n_ctx"]') as HTMLInputElement | null;
    await waitFor(() => expect(field()).toBeTruthy());
    // The node's plan brings 4096 (see `wholePlan`).
    await waitFor(() => expect(field()!.value).toBe("4096"));

    fireEvent.change(field()!, { target: { value: "65536" } });
    await waitFor(() => expect(field()!.value).toBe("65536"));

    fireEvent.click(screen.getByRole("button", { name: /Voltar ao plano do nó/ }));

    await waitFor(() => expect(field()!.value).toBe("4096"));
  });

  it("in sliced mode MTP is offered in speculation and turns on the controls", async () => {
    post.mockImplementation((url: string) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      return Promise.resolve(shardPlan());
    });
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false, refetchInterval: false } },
    });
    client.setQueryData(keys.jobs, { jobs: [] });
    client.setQueryData(keys.clusterModels, {
      peers: [
        {
          node_id: "ksia",
          hostname: "ksia",
          is_self: false,
          models: [],
          shard_sets: [{ group: "qwen", shards: [], tp_parts: [] }],
        },
      ],
    });
    render(
      <QueryClientProvider client={client}>
        <LocaleProvider initialLocale="pt-BR">
          <MemoryRouter><DeployModel /></MemoryRouter>
        </LocaleProvider>
      </QueryClientProvider>,
    );

    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));
    const speculation = (await screen.findByRole("combobox", { name: "Especulação" })) as HTMLSelectElement;
    expect([...speculation.options].map((option) => option.value)).toContain("mtp");
    await chooseSpeculation("mtp");

    await waitFor(() => {
      expect(
        screen.getByText("Tokens rascunhados pelo MTP"),
      ).toBeTruthy();
    });
  });

  it("speculation follows the plan: MTP in the plan comes selected, without it it starts as None", async () => {
    mountWhole(true);
    chooseWhole();
    const speculation = (await screen.findByRole("combobox", { name: "Especulação" })) as HTMLSelectElement;
    await waitFor(() => expect(speculation.value).toBe("mtp"));
    expect(screen.getByText("Tokens rascunhados pelo MTP")).toBeTruthy();
    cleanup();

    mountWhole(false);
    chooseWhole();
    const none = (await screen.findByRole("combobox", { name: "Especulação" })) as HTMLSelectElement;
    await waitFor(() => expect(none.value).toBe("none"));
    expect(screen.queryByText("Tokens rascunhados pelo MTP")).toBeNull();
    expect(screen.queryByText("Probabilidade mínima do MTP")).toBeNull();
  });

  it("one speculation at a time: choosing lookup turns MTP off", async () => {
    mountWhole();
    chooseWhole();
    await ensureWholeMachine();
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));

    await chooseSpeculation("mtp");
    await waitFor(() => expect(screen.getByText("Tokens rascunhados pelo MTP")).toBeTruthy());

    await chooseSpeculation("lookup");
    await waitFor(() => expect(screen.queryByText("Tokens rascunhados pelo MTP")).toBeNull());
    expect(screen.queryByRole("combobox", { name: "Onde o MTP fica" })).toBeNull();

    const button = screen.getByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(button);
    await waitFor(() => {
      const deploys = post.mock.calls.filter(([url]) => url === "/api/ui/deploy");
      expect(deploys).toHaveLength(1);
      const body = deploys[0][1] as { params: RuntimeParams };
      expect(body.params.mtp_enabled).toBe(false);
      expect(body.params.speculative_decoding_enabled).toBe(true);
    });
  });
});

describe("DeployModel · simple Tensor", () => {
  beforeEach(() => {
    post.mockReset();
    localStorage.clear();
  });
  afterEach(cleanup);

  it("builds the ranks only with the machines that are on the map", async () => {
    answerPlans();
    mount();
    chooseTensor();

    const workerSelect = await screen.findByRole("listbox", {
      name: /Workers do Tensor Parallelism/,
    });
    const availablePeer = await screen.findByRole("button", { name: /mac RTT/ });
    fireEvent.click(availablePeer);

    await waitFor(() => {
      const last = planCalls().at(-1)![1] as {
        tp?: { size: number; primary_node_id: string; worker_node_ids: string[] };
      };
      expect(last.tp).toMatchObject({
        size: 2,
        primary_node_id: "ksia",
        worker_node_ids: ["mac"],
      });
    });
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));
    const currentWorkerSelect = screen.getByRole("listbox", {
      name: /Workers do Tensor Parallelism/,
    });
    expect([...currentWorkerSelect.querySelectorAll("option")].map((option) => option.textContent)).toEqual(["mac"]);
  });

  it("chooses the sliced TP group and shows the ranks, not pipeline layers", async () => {
    answerPlans();
    mount();
    chooseTensor();

    const model = await screen.findByRole("combobox", { name: "Modelo técnico" });
    expect((model as HTMLSelectElement).value).toBe("qwen:2");
    expect(screen.getByText(/1of2/)).toBeTruthy();
    expect(screen.getByText(/2of2/)).toBeTruthy();
    expect(screen.queryByText("Camadas disponíveis neste peer")).toBeNull();
  });

  it("keeps primary and workers restricted to the cards that are on the map", async () => {
    answerPlans();
    mount();
    chooseTensor();

    fireEvent.click(await screen.findByRole("button", { name: /^salem RTT -/ }));
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));

    await waitFor(() => {
      expect(
        [...screen.getByRole("combobox", { name: /Máquina principal/ }).options].filter((option) => option.value).map((option) => option.textContent),
      ).toEqual(["ksia", "salem"]);
      expect(
        [...screen.getByRole("listbox", { name: /Workers/ }).options].map((option) => option.textContent),
      ).toEqual(["salem"]);
    });
  });

  it("preserves a pre-sliced worker without a local rank while the plan answers", async () => {
    answerPlans();
    mount();
    chooseTensor();
    fireEvent.click(await screen.findByRole("button", { name: /^salem RTT -/ }));
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));

    let releasePlan: ((plan: DeployPlan) => void) | undefined;
    post.mockImplementation((url: string, body: { kind?: string; tp?: { worker_node_ids?: string[] } }) => {
      if (url === "/api/ui/deploy/plan" && body.tp?.worker_node_ids?.includes("salem")) {
        return new Promise<DeployPlan>((resolve) => { releasePlan = resolve; });
      }
      return body.kind === "tensor_parallel" ? Promise.resolve(tensorPlan()) : Promise.resolve(shardPlan());
    });

    const workerSelect = screen.getByRole("listbox", { name: /Workers/ }) as HTMLSelectElement;
    [...workerSelect.options].forEach((option) => { option.selected = option.value === "salem"; });
    fireEvent.change(workerSelect);

    await waitFor(() => {
      expect((planCalls().at(-1)![1] as { tp: { worker_node_ids: string[] } }).tp.worker_node_ids).toEqual(["salem"]);
      expect([...workerSelect.selectedOptions].map((option) => option.value)).toEqual(["salem"]);
    });
    releasePlan?.(tensorPlan());
  });

  it("keeps on the canvas a machine without a GGUF, but does not offer it as a worker in the whole model", async () => {
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      return body.kind === "tensor_parallel"
        ? Promise.resolve({ ...tensorPlan(), group: "qwen3.5-9b" })
        : Promise.resolve(shardPlan());
    });
    const { client } = mount();
    await waitFor(() => expect(planCalls().length).toBeGreaterThan(0));
    client.setQueryData(keys.clusterModels, {
      peers: [
        {
          node_id: "ksia",
          hostname: "ksia",
          is_self: false,
          models: [{
            id: "1",
            name: "qwen3.5-9b",
            source: "local",
            source_label: "Local",
            path: "/lm/qwen3.5-9b.gguf",
            size_bytes: 5_000_000_000,
            architecture: "qwen3",
            total_layers: 32,
            quant: "Q4_K_M",
            shard_range: null,
            shard_group: null,
          }],
          shard_sets: [],
        },
        { node_id: "salem", hostname: "salem", is_self: false, models: [], shard_sets: [] },
      ],
    });
    chooseTensor();

    fireEvent.click(await screen.findByRole("button", { name: /^salem RTT -/ }));
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));

    await waitFor(() => expect(screen.getByRole("button", { name: /salem disponível/ })).toBeTruthy());
    expect([...screen.getByRole("combobox", { name: /Máquina principal/ }).options].filter((option) => option.value).map((option) => option.textContent)).toEqual(["ksia"]);
    expect([...screen.getByRole("listbox", { name: /Workers/ }).options]).toHaveLength(0);
  });

  it("uses in the combos the same machine population presented by the complete strategy", async () => {
    answerPlans();
    mount();
    chooseTensor();
    fireEvent.click(screen.getByRole("button", { name: "Completa" }));

    await waitFor(() => expect(screen.getByRole("button", { name: /salem disponível/ })).toBeTruthy());
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));
    await waitFor(() => {
      expect([...screen.getByRole("combobox", { name: /Máquina principal/ }).options].filter((option) => option.value).map((option) => option.textContent)).toEqual(["ksia", "mac", "salem"]);
      expect([...screen.getByRole("listbox", { name: /Workers/ }).options].map((option) => option.textContent)).toEqual(["mac", "salem"]);
    });
  });

  it("marks primary on the card and keeps the mutual exclusion with workers when switching cards", async () => {
    answerPlans();
    mount();
    chooseTensor();
    fireEvent.click(await screen.findByRole("button", { name: /^salem RTT -/ }));

    fireEvent.click(await screen.findByRole("button", { name: /Marcar salem como primary/ }));
    await waitFor(() => expect((planCalls().at(-1)![1] as { tp: { primary_node_id: string } }).tp.primary_node_id).toBe("salem"));

    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));
    const workerSelect = screen.getByRole("listbox", { name: /Workers/ }) as HTMLSelectElement;
    [...workerSelect.options].forEach((option) => { option.selected = option.value === "ksia"; });
    fireEvent.change(workerSelect);
    fireEvent.click(await screen.findByRole("button", { name: /Marcar ksia como primary/ }));

    await waitFor(() => {
      expect(planCalls().some(([, body]) => JSON.stringify((body as { tp?: unknown }).tp) === JSON.stringify({
        size: 2,
        primary_node_id: "ksia",
        worker_node_ids: ["salem"],
      }))).toBe(true);
    });
  });

  it("clears primary and worker when the corresponding cards are removed from the map", async () => {
    answerPlans();
    mount();
    chooseTensor();
    fireEvent.click(await screen.findByRole("button", { name: /^salem RTT -/ }));
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));

    const workerSelect = screen.getByRole("listbox", { name: /Workers/ }) as HTMLSelectElement;
    [...workerSelect.options].forEach((option) => { option.selected = option.value === "salem"; });
    fireEvent.change(workerSelect);
    expect((screen.getByRole("combobox", { name: /Máquina principal/ }) as HTMLSelectElement).value).toBe("ksia");

    fireEvent.click(await screen.findByRole("button", { name: "Remover ksia da implantação" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: /ksia na implantação/ })).toBeNull());
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));
    expect((screen.getByRole("combobox", { name: /Máquina principal/ }) as HTMLSelectElement).value).toBe("");
    expect([...screen.getByRole("listbox", { name: /Workers/ }).options].map((option) => option.textContent)).toEqual(["salem"]);

    fireEvent.click(screen.getByRole("button", { name: "Remover salem da implantação" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: /salem na implantação/ })).toBeNull());
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));
    expect((screen.getByRole("combobox", { name: /Máquina principal/ }) as HTMLSelectElement).value).toBe("");
    expect([...screen.getByRole("listbox", { name: /Workers/ }).options]).toHaveLength(0);
  });

  it("preserves size, primary and the worker order in the plan and in the deploy", async () => {
    answerPlans();
    mount();
    chooseTensor();
    fireEvent.click(await screen.findByRole("button", { name: /^salem RTT -/ }));
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));

    const workerSelect = screen.getByRole("listbox", { name: /Workers/ }) as HTMLSelectElement;
    [...workerSelect.options].forEach((option) => { option.selected = option.value === "salem"; });
    fireEvent.change(workerSelect);

    await waitFor(() => expect((planCalls().at(-1)![1] as { tp: unknown }).tp).toEqual({
      size: 2,
      primary_node_id: "ksia",
      worker_node_ids: ["salem"],
    }));
    fireEvent.click(screen.getByRole("button", { name: /^Implantar$/ }));

    await waitFor(() => {
      const deploy = post.mock.calls.find(([url]) => url === "/api/ui/deploy");
      expect(deploy?.[1]).toMatchObject({
        tp: {
          size: 2,
          primary_node_id: "ksia",
          worker_node_ids: ["salem"],
        },
      });
    });
  });
});
