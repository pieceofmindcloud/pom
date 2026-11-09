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
import type { DeployPlan, Job, PeerCatalog, RuntimeParams } from "../lib/types";

const post = vi.fn();
/** What the live cluster answers; a test fills it to put machines in service. */
let livePeers: Array<Record<string, unknown>> = [];
/** What `/api/ui/jobs` answers; a test fills it to follow a deploy. */
let jobsResponse: { jobs: Job[] } = { jobs: [] };
vi.mock("../lib/api", () => ({
  api: {
    get: vi.fn((url: string) =>
      url === "/api/ui/jobs"
        ? Promise.resolve(jobsResponse)
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
    getConditional: vi.fn((url: string) =>
      Promise.resolve(url === "/api/ui/jobs" ? jobsResponse : { peers: livePeers }),
    ),
    post: (...args: unknown[]) => post(...args),
  },
  getConnectionKey: () => "",
  setConnectionKey: () => {},
  ApiError: class extends Error {
    constructor(public status = 0, message = "", public body: unknown = undefined) {
      super(message);
    }
  },
}));

import { DeployModel } from "./DeployModel";
import { ApiError } from "../lib/api";
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
  client.setQueryData(keys.jobs, jobsResponse);
  client.setQueryData(keys.node, { connection_key: "", pom_dev_mode: true });
  if (livePeers.length > 0) client.setQueryData(keys.cluster, { peers: livePeers });
  client.setQueryData(keys.clusterModels, {
    peers: [
      {
        node_id: "ksia",
        hostname: "ksia",
        is_self: false,
        hf_sources: [{ path: "/lm/mmproj-qwen-f16.gguf", repo_id: "org/qwen-gguf", revision: null, filename: "mmproj-qwen-f16.gguf" }],
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

/** The map opens on the machines that hold the model: leaves only `nodes` on it. */
async function onlyOnMap(...nodes: string[]) {
  const all = ["ksia", "mac", "salem"];
  const removeOf = (node: string) => screen.queryByRole("button", { name: new RegExp(`Remover ${node} da implantação`) });
  const addOf = (node: string) => screen.queryByRole("button", { name: new RegExp(`${node} RTT`) });
  // the survey answered and the map opened on its machines
  await waitFor(() => expect(all.some((node) => removeOf(node))).toBe(true));
  for (const node of all) {
    if (!nodes.includes(node) && removeOf(node)) fireEvent.click(removeOf(node)!);
  }
  for (const node of nodes) {
    if (!removeOf(node)) {
      await waitFor(() => expect(addOf(node)).toBeTruthy());
      fireEvent.click(addOf(node)!);
    }
    await waitFor(() => expect(removeOf(node)).toBeTruthy());
  }
}

/** One machine on the map, the one most tests deploy on. */
async function ensureWholeMachine(node = "ksia") {
  await onlyOnMap(node);
}

describe("DeployModel · whole model", () => {
  beforeEach(() => {
    post.mockReset();
    localStorage.clear();
  });
  afterEach(cleanup);


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

  it("offers Download here on the card of a machine that lacks the projector the plan needs", async () => {
    post.mockImplementation((url: string, body: { kind?: string }) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      if (body.kind !== "whole") return Promise.resolve(shardPlan());
      const plan = wholePlan();
      plan.copies = [{
        kind: "mmproj",
        name: "mmproj-qwen-f16.gguf",
        size_bytes: 1,
        source_node_id: "ksia",
        source_host: "ksia",
        target_node_id: "ksia",
        target_host: "ksia",
      }];
      return Promise.resolve(plan);
    });
    mount();
    await ensureWholeMachine();
    expect(await screen.findByText(/precisa do projetor de visão mmproj-qwen-f16.gguf/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Baixar aqui" })).toBeTruthy();
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
    client.setQueryData(keys.jobs, jobsResponse);
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

  it("in sliced mode MTP is offered in speculation and turns on the controls", async () => {
    post.mockImplementation((url: string) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      return Promise.resolve(shardPlan());
    });
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false, refetchInterval: false } },
    });
    client.setQueryData(keys.jobs, jobsResponse);
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


describe("DeployModel · pipeline map", () => {
  beforeEach(() => {
    post.mockReset();
    localStorage.clear();
    livePeers = [];
  });
  afterEach(cleanup);

  /** The survey (no range named) gets the whole plan; the plan Deploy asks for
   *  gets `verdict`, else the same plan; the deploy gets a job. */
  function answerWith(verdict?: (body: Record<string, unknown>) => DeployPlan) {
    post.mockImplementation((url: string, body: Record<string, unknown>) => {
      if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
      const asksRows = (Array.isArray(body.whole_ranges) && body.whole_ranges.length > 0)
        || (Array.isArray(body.selection) && body.selection.length > 0);
      return Promise.resolve(asksRows && verdict ? verdict(body) : wholePlan());
    });
  }
  const deploys = () => post.mock.calls.filter(([url]) => url === "/api/ui/deploy");
  const pressDeploy = async () => {
    const button = await screen.findByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(button);
  };
  const machinesTab = () => fireEvent.click(screen.getByRole("tab", { name: /Máquinas/ }));

  it("offers Pipeline and Image model only", async () => {
    answerWith();
    mount();
    await waitFor(() => expect(planCalls().length).toBeGreaterThan(0));

    const radios = screen.getAllByRole("radio").map((radio) => radio.textContent);
    expect(radios).toEqual(["Pipeline", "Modelo de imagem"]);
    expect(screen.queryByRole("button", { name: "Simples" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Completa" })).toBeNull();
    expect(screen.queryByRole("button", { name: /plano do nó/ })).toBeNull();
  });

  it("surveys the model once, not on every map edit", async () => {
    answerWith();
    mount();
    await waitFor(() => expect(planCalls().length).toBeGreaterThan(0));
    expect(planCalls()[0][1]).toMatchObject({ group: "qwen3.5-9b", kind: "whole", whole_ranges: [] });
    const surveyed = planCalls().length;

    await onlyOnMap("ksia", "mac");
    machinesTab();
    fireEvent.click(screen.getByRole("button", { name: "Dividir as camadas entre 2 máquinas" }));
    fireEvent.change(screen.getByLabelText("Camadas de ksia: end"), { target: { value: "10" } });

    await new Promise((resolve) => setTimeout(resolve, 400));
    expect(planCalls()).toHaveLength(surveyed);
  });

  it("opens on the machines that hold the model and splits over the marked ones", async () => {
    answerWith();
    mount();
    // the map opens on the three machines that hold the model, the layers split over them
    await waitFor(() => expect(screen.getByRole("tab", { name: "Máquinas (3)" })).toBeTruthy());
    machinesTab();
    expect(screen.queryByText(/GPU uso/)).toBeNull();

    // opened as ksia 0-10, mac 11-20, salem 21-31; unmark salem: it keeps its range,
    // and Split shares the layers it does not serve (0-20) between ksia and mac
    fireEvent.click(screen.getByRole("checkbox", { name: "Marcar salem para dividir as camadas" }));
    fireEvent.click(screen.getByRole("button", { name: "Dividir as camadas entre 2 máquinas" }));
    expect(screen.queryByText("Nenhuma camada atribuída")).toBeNull();
    // ksia: 0-10 -> the end thumb dragged to layer 8 gives L0-7
    fireEvent.change(screen.getByLabelText("Camadas de ksia: end"), { target: { value: "8" } });
    await pressDeploy();

    await waitFor(() => expect(deploys()).toHaveLength(1));
    expect(deploys()[0][1]).toMatchObject({
      kind: "whole",
      whole_ranges: [
        { node_id: "ksia", start_layer: 0, end_layer: 7 },
        { node_id: "mac", start_layer: 11, end_layer: 20 },
        { node_id: "salem", start_layer: 21, end_layer: 31 },
      ],
    });
  });

  it("shows the measured RTT on the hop between two stages", async () => {
    post.mockImplementation((url: string) => {
      if (url === "/api/ui/chat/telemetry") {
        return Promise.resolve({ measurements: [{ from_node_id: "ksia", to_node_id: "mac", rtt_ms: 7.5, transport: "quic" }] });
      }
      return Promise.resolve(url === "/api/ui/deploy" ? { job_id: "j1" } : wholePlan());
    });
    mount();
    await onlyOnMap("ksia", "mac");
    machinesTab();
    fireEvent.click(screen.getByRole("button", { name: "Dividir as camadas entre 2 máquinas" }));

    expect(await screen.findByText("RTT 7.5 ms")).toBeTruthy();
  });

  it("validates the map when Deploy is pressed and says why it cannot run", async () => {
    answerWith(() => ({ ...wholePlan(), complete: false, blocked: "L10-15 has no machine" }));
    mount();
    await ensureWholeMachine("ksia");
    await pressDeploy();

    const dialog = await screen.findByRole("dialog", { name: "Esta implantação não pode rodar" });
    expect(within(dialog).getByTestId("deploy-refusal").textContent).toContain("L10-15 has no machine");
    expect(deploys()).toHaveLength(0);
  });

  it("names the machine without the model when Deploy is pressed", async () => {
    answerWith(() => ({
      ...wholePlan(),
      copies: [{ kind: "slice", target_node_id: "mac", target_host: "mac", source_node_id: "ksia", name: "qwen3.5-9b", size_bytes: 1 }],
    } as unknown as DeployPlan));
    mount();
    await ensureWholeMachine("mac");
    await pressDeploy();

    const dialog = await screen.findByRole("dialog", { name: "Esta implantação não pode rodar" });
    expect(dialog.textContent).toContain("Sem o modelo: mac");
    expect(deploys()).toHaveLength(0);
  });

  it("keeps the KV cache combos and applies the choice without a button", async () => {
    post.mockImplementation((url: string, body?: { cache_type_k?: string; cache_type_v?: string }) =>
      url === "/api/ui/cache-types"
        ? Promise.resolve({
            cache_type_k: body?.cache_type_k ?? "f16",
            cache_type_v: body?.cache_type_v ?? "f16",
            choices: ["f32", "f16", "bf16", "q8_0", "q4_0", "q4_1", "iq4_nl", "q5_0", "q5_1"],
          })
        : Promise.resolve(wholePlan()),
    );
    mount();
    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));

    const cacheK = await screen.findByRole("combobox", { name: "Cache K" });
    expect((cacheK as HTMLSelectElement).value).toBe("f16");
    expect(screen.queryByRole("button", { name: "Salvar cache KV" })).toBeNull();
    fireEvent.change(cacheK, { target: { value: "q8_0" } });
    await waitFor(() => {
      expect(post).toHaveBeenCalledWith("/api/ui/cache-types", { cache_type_k: "q8_0", cache_type_v: "f16" });
    });
  });

  it("offers f32 for the hidden state between machines and sends the choice", async () => {
    answerWith();
    mount();
    await onlyOnMap("ksia", "mac");
    machinesTab();
    fireEvent.click(screen.getByRole("button", { name: "Dividir as camadas entre 2 máquinas" }));
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));

    const precision = screen.getByRole("combobox", { name: "Precisão do estado oculto" });
    expect((precision as HTMLSelectElement).value).toBe("f16");
    fireEvent.change(precision, { target: { value: "f32" } });
    await pressDeploy();
    await waitFor(() => expect(deploys()).toHaveLength(1));
    expect(deploys()[0][1]).toMatchObject({ params: expect.objectContaining({ hidden_state_dtype: "f32" }) });
  });

  describe("following the deploy", () => {
    const running = {
      id: "job-running",
      state: "running",
      step: "carregando as fatias",
      message: "",
      progress: 40,
      outputs: [],
      started_at: new Date().toISOString(),
      finished_at: null,
    } as unknown as Job;
    afterEach(() => { jobsResponse = { jobs: [] }; });

    it("follows the deploy it started in a window, with Close and Switch to dashboard", async () => {
      jobsResponse = { jobs: [running] };
      post.mockImplementation((url: string) =>
        url === "/api/ui/deploy" ? Promise.resolve({ job_id: "job-running" }) : Promise.resolve(wholePlan()),
      );
      mount();
      await ensureWholeMachine("ksia");
      await pressDeploy();

      const progress = await screen.findByTestId("deploy-progress");
      expect(progress.textContent).toMatch(/carregando as fatias/);
      expect(screen.getByRole("button", { name: "Ir para o painel" })).toBeTruthy();
      fireEvent.click(screen.getByRole("button", { name: "Fechar" }));
      await waitFor(() => expect(screen.queryByTestId("deploy-progress")).toBeNull());
    });

    it("follows the deploy already running when the node refuses a second one", async () => {
      jobsResponse = { jobs: [running] };
      post.mockImplementation((url: string) =>
        url === "/api/ui/deploy"
          ? Promise.reject(new ApiError(409, "already being deployed", { code: "deploy_in_progress", job_id: "job-running" }))
          : Promise.resolve(wholePlan()),
      );
      mount();
      await ensureWholeMachine("ksia");
      await pressDeploy();
      expect((await screen.findByTestId("deploy-progress")).textContent).toMatch(/carregando as fatias/);
    });
  });

  it("asks before deploying a map that leaves layers without a machine", async () => {
    answerWith(() => ({ ...wholePlan(), complete: false, missing: [[16, 31]] }));
    mount();
    await ensureWholeMachine("ksia");
    await pressDeploy();

    await screen.findByText(/L16–31/);
    expect(deploys()).toHaveLength(0);
  });
});
