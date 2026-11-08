// @vitest-environment jsdom
/**
 * The deploy screen has to say why the button is grey.
 *
 * The defect these tests guard: `validated` compared the rows of the screen
 * - which the screen keeps in layer order - against `plan.steps` position by
 * position, and the node returned the steps grouped by target machine. It took
 * just one machine holding two ranges for the order to diverge. And since *all*
 * the explanation panels rendered under
 * `validated`, the operator was left with exactly what the captain reported: a
 * disabled button and no explanation on screen.
 *
 * Both sides come in here: the comparison stopped depending on the order, and
 * the explanation stopped depending on the comparison.
 */
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DeployPlan, DeployStep, Job } from "../lib/types";

const post = vi.fn();
let pomDevMode = false;
let jobsResponse: { jobs: Job[] } = { jobs: [] };
vi.mock("../lib/api", () => ({
  api: {
    get: vi.fn((url: string) =>
      url === "/api/ui/node"
        ? Promise.resolve({ connection_key: "", pom_dev_mode: pomDevMode })
        : url === "/api/ui/jobs"
        ? Promise.resolve(jobsResponse)
        : url === "/api/ui/cluster"
          ? Promise.resolve({ peers: [] })
          : url === "/api/ui/cache-types"
            ? Promise.resolve({
                cache_type_k: "f16",
                cache_type_v: "f16",
                choices: ["f32", "f16", "bf16", "q8_0", "q4_0", "q4_1", "iq4_nl", "q5_0", "q5_1"],
              })
          : undefined,
    ),
    getConditional: vi.fn((url: string) =>
      Promise.resolve(url === "/api/ui/jobs" ? jobsResponse : { peers: [] }),
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

const MACHINES = ["ksia", "salem", "mac"];

function step(
  start: number,
  end: number,
  target: string,
  position: number,
): DeployStep {
  return {
    position,
    start_layer: start,
    end_layer: end,
    total_layers: 32,
    name: `m_${start}_${end}.gguf`,
    size_bytes: 1_000_000,
    target_node_id: target,
    target_host: target,
    ready: true,
    source_node_id: null,
    source_host: null,
    source_path: null,
    target_path: null,
    primary: start === 0,
    n_gpu_layers: null,
    offload_kqv: null,
    options: MACHINES.map((id) => ({ node_id: id, label: id, has: true, ok: true, note: null })),
    problem: target ? null : `a fatia L${start}-${end} está sem máquina`,
    note: null,
    merged: false,
    parts: [],
  };
}

/**
 * The plan as the node returned it: grouped by machine, and therefore out of
 * layer order. `ksia` holds two ranges, so the response arrives out of
 * pipeline order.
 */
function planOutOfLayerOrder(): DeployPlan {
  const ranges: Array<[number, number]> = [
    [0, 7],
    [8, 15],
    [16, 23],
    [24, 31],
  ];
  return {
    group: "qwen",
    total_layers: 32,
    total_layers_inferred: false,
    steps: [
      step(0, 7, "ksia", 1),
      step(16, 23, "ksia", 2),
      step(8, 15, "salem", 3),
      step(24, 31, "mac", 4),
    ],
    complete: true,
    transfer_bytes: 0,
    blocked: null,
    auto: true,
    merge: false,
    available: ranges.map(([a, b]) => ({
      start_layer: a,
      end_layer: b,
      total_layers: 32,
      name: `m_${a}_${b}.gguf`,
      size_bytes: 1_000_000,
      holders: MACHINES,
      selected: true,
    })),
    machines: MACHINES.map((id) => ({
      node_id: id,
      label: id,
      mem_free_mb: 100_000,
      disk_free_mb: 500_000,
      load: 0,
      holds: ranges,
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
    } as DeployPlan["params"],
    prompt_tokens: 3840,
    chunk_tokens: 512,
  };
}

function planWithUnassignedMiddleRange(): DeployPlan {
  const plan = planOutOfLayerOrder();
  const ranges: Array<[number, number]> = [
    [0, 22],
    [23, 26],
    [27, 43],
    [44, 64],
  ];
  plan.total_layers = 65;
  plan.steps = [
    step(0, 22, "ksia", 1),
    step(23, 26, "", 2),
    step(27, 43, "salem", 3),
    step(44, 64, "mac", 4),
  ];
  plan.available = ranges.map(([start_layer, end_layer]) => ({
    start_layer,
    end_layer,
    total_layers: 65,
    name: `m_${start_layer}_${end_layer}.gguf`,
    size_bytes: 1_000_000,
    holders: ["ksia", "ksia-2", "salem", "mac"],
    selected: true,
  }));
  plan.machines = ["ksia", "ksia-2", "salem", "mac"].map((node_id) => ({
    node_id,
    label: node_id,
    mem_free_mb: 100_000,
    disk_free_mb: 500_000,
    holds: ranges,
    taking: null,
  }));
  plan.blocked = "a fatia L23-26 está sem máquina";
  plan.complete = false;
  plan.missing = [[23, 26]];
  return plan;
}

function mount(metadata_complete: boolean | undefined = true, dev = false) {
  pomDevMode = dev;
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
        ...(metadata_complete === undefined ? {} : { metadata_complete }),
        models: [],
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

describe("DeployModel", () => {
  beforeEach(() => {
    post.mockReset();
    jobsResponse = { jobs: [] };
    pomDevMode = false;
    localStorage.clear();
  });
  afterEach(cleanup);

  it("shows a single warning when the selection is still waiting for metadata", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = "a seleção da fatia L0-7 ainda aguarda os metadados do GGUF";
    post.mockResolvedValue(p);
    mount(false);

    await waitFor(() => {
      expect(post).toHaveBeenCalledWith("/api/ui/deploy/plan", expect.anything());
    });
    const button = await screen.findByRole("button", { name: "Implantar" });
    expect(button.hasAttribute("disabled")).toBe(true);
    expect(screen.queryByText(/A lista de modelos já está disponível/)).toBeNull();
    expect(screen.getAllByText(/metadados do GGUF/)).toHaveLength(1);
  });

  it("allows a complete selection while another model is still being enriched", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockResolvedValue(p);
    mount(false);

    await waitFor(() => {
      const button = screen.getByRole("button", { name: /^Implantar$/ });
      expect((button as HTMLButtonElement).disabled).toBe(false);
    });
    expect(screen.queryByText(/metadados do GGUF/i)).toBeNull();
  });

  it("does not block a complete selection when an old peer does not report the state", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockResolvedValue(p);
    mount(undefined);

    await waitFor(() => {
      const button = screen.getByRole("button", { name: /^Implantar$/ });
      expect((button as HTMLButtonElement).disabled).toBe(false);
    });
  });

  it("keeps the incomplete selection blocked with the missing range in the warning", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = "a seleção da fatia L8-15 ainda aguarda os metadados do GGUF";
    p.complete = false;
    post.mockResolvedValue(p);
    mount(false);

    await waitFor(() => {
      expect(screen.getByText(/Não dá para implantar deste jeito/)).toBeTruthy();
    });
    expect(screen.getByText(/L8-15/)).toBeTruthy();
    expect((screen.getByRole("button", { name: /^Implantar$/ }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("keeps the KV cache combos and applies the choice without a button", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockImplementation((url: string, body?: { cache_type_k?: string; cache_type_v?: string }) =>
      url === "/api/ui/cache-types"
        ? Promise.resolve({
            cache_type_k: body?.cache_type_k ?? "f16",
            cache_type_v: body?.cache_type_v ?? "f16",
            choices: ["f32", "f16", "bf16", "q8_0", "q4_0", "q4_1", "iq4_nl", "q5_0", "q5_1"],
          })
        : Promise.resolve(p),
    );
    mount();

    await waitFor(() => {
      expect(screen.getByRole("tab", { name: "Globais" })).toBeTruthy();
    });
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));

    // The KV cache combos live in the "Configurações padrões" category.
    expect(screen.getByText("Configurações padrões")).toBeTruthy();
    const cacheK = screen.getByRole("combobox", { name: "Cache K" });
    const cacheV = screen.getByRole("combobox", { name: "Cache V" });
    expect((cacheK as HTMLSelectElement).value).toBe("f16");
    expect((cacheV as HTMLSelectElement).value).toBe("f16");
    expect(screen.queryByText("global", { exact: true })).toBeNull();
    expect(screen.queryByText("Tipo aplicado a todos os contextos deste nó no próximo carregamento.")).toBeNull();
    expect(screen.queryByRole("button", { name: "Salvar cache KV" })).toBeNull();

    fireEvent.change(cacheK, { target: { value: "q8_0" } });
    await waitFor(() => {
      expect(post).toHaveBeenCalledWith("/api/ui/cache-types", {
        cache_type_k: "q8_0",
        cache_type_v: "f16",
      });
    });
  });

  it("offers f32 in the precision selector used by the Deploy Studio and sends the choice", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockImplementation((url: string) =>
      url === "/api/ui/deploy" ? Promise.resolve({ job_id: "f32-job" }) : Promise.resolve(p),
    );
    mount();

    await waitFor(() => expect(screen.getByRole("tab", { name: "Globais" })).toBeTruthy());
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));

    const precision = screen.getByRole("combobox", { name: "Precisão do estado oculto" });
    expect((precision as HTMLSelectElement).value).toBe("f16");
    expect(within(precision).getByRole("option", { name: "f32 - 4 bytes por valor" })).toBeTruthy();
    fireEvent.change(precision, { target: { value: "f32" } });

    const deploy = await screen.findByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((deploy as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(deploy);
    await waitFor(() => {
      expect(post).toHaveBeenCalledWith("/api/ui/deploy", expect.objectContaining({
        params: expect.objectContaining({ hidden_state_dtype: "f32" }),
      }));
    });
  });

  it("shows and sends vision when the sliced plan finds an mmproj", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    p.mmproj_path = "/models/mmproj-qwen-f16.gguf";
    p.mmproj_candidates = [
      "/models/mmproj-qwen-f16.gguf",
      "/models/mmproj-qwen-q8_0.gguf",
    ];
    post.mockImplementation((url: string) =>
      url === "/api/ui/deploy" ? Promise.resolve({ job_id: "vision-job" }) : Promise.resolve(p),
    );
    mount();

    await waitFor(() => expect(screen.getByRole("tab", { name: "Globais" })).toBeTruthy());
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));

    const vision = screen.getByRole("checkbox", { name: "Ativar visão" });
    expect(vision).toBeTruthy();
    const projector = screen.getByRole("combobox", { name: "Projetor de visão" });
    expect((projector as HTMLSelectElement).value).toBe("/models/mmproj-qwen-f16.gguf");
    fireEvent.change(projector, { target: { value: "/models/mmproj-qwen-q8_0.gguf" } });
    fireEvent.click(vision);

    const deploy = await screen.findByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((deploy as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(deploy);
    await waitFor(() => {
      expect(post).toHaveBeenCalledWith("/api/ui/deploy", expect.objectContaining({
        group: "qwen",
        kind: "shards",
        vision: true,
        mmproj_path: "/models/mmproj-qwen-q8_0.gguf",
      }));
    });
  });

  it("without vision sends no projector, neither in the plan nor in the deployment", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    p.mmproj_path = "/models/mmproj-qwen-f16.gguf";
    p.mmproj_candidates = ["/models/mmproj-qwen-f16.gguf"];
    post.mockImplementation((url: string) =>
      url === "/api/ui/deploy" ? Promise.resolve({ job_id: "plain-job" }) : Promise.resolve(p),
    );
    mount();

    const deploy = await screen.findByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((deploy as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(deploy);
    await waitFor(() => {
      expect(post).toHaveBeenCalledWith("/api/ui/deploy", expect.objectContaining({ vision: false }));
    });
    const deployBody = post.mock.calls.find(([url]) => url === "/api/ui/deploy")?.[1] as Record<string, unknown>;
    expect(deployBody).not.toHaveProperty("mmproj_path");
    for (const [url, body] of post.mock.calls) {
      if (url === "/api/ui/deploy/plan") expect(body).not.toHaveProperty("mmproj_path");
    }
  });

  it("once the plan is in, only offers the head machine's projectors", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    // The head is another machine: the paths are its own.
    p.mmproj_path = "/peer/models/mmproj-qwen-f16.gguf";
    p.mmproj_candidates = ["/peer/models/mmproj-qwen-f16.gguf"];
    post.mockImplementation((url: string) =>
      url === "/api/ui/deploy" ? Promise.resolve({ job_id: "vision-job" }) : Promise.resolve(p),
    );
    mount();

    await waitFor(() => expect(screen.getByRole("tab", { name: "Globais" })).toBeTruthy());
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "Ativar visão" }));

    const deploy = await screen.findByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((deploy as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(deploy);
    await waitFor(() => {
      expect(post).toHaveBeenCalledWith("/api/ui/deploy", expect.objectContaining({
        vision: true,
        mmproj_path: "/peer/models/mmproj-qwen-f16.gguf",
      }));
    });
  });

  it("explains when the sliced plan has no mmproj", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    p.mmproj_path = null;
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByRole("tab", { name: "Globais" })).toBeTruthy());
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));

    expect(screen.getByRole("alert").textContent).toContain(
      "Visão indisponível: não foi possível provar a que modelo pertence o projetor.",
    );
    expect(screen.queryByRole("checkbox", { name: "Ativar visão" })).toBeNull();
  });

  it("explains the refusal of a gap even when the steps arrive out of order", async () => {
    post.mockResolvedValue(planWithUnassignedMiddleRange());
    mount();

    // The reason for the refusal shows - it was exactly what went missing.
    await waitFor(() => {
      expect(screen.getByText(/Não dá para implantar deste jeito/)).toBeTruthy();
    });
    expect(screen.getByText(/L23-26/)).toBeTruthy();

    const button = screen.getByRole("button", { name: /Implantar/ });
    expect((button as HTMLButtonElement).disabled).toBe(true);
  });

  it("keeps the overlap refusal on the button path", async () => {
    const invalid = planOutOfLayerOrder();
    invalid.blocked = "as camadas L5-10 estão em duas fatias escolhidas ao mesmo tempo";
    invalid.complete = false;
    invalid.overlap = [[5, 10]];
    post.mockResolvedValue(invalid);
    mount();

    await waitFor(() => {
      expect(screen.getByText(/Não dá para implantar deste jeito/)).toBeTruthy();
    });
    expect(screen.getByText(/L5-10/)).toBeTruthy();
    expect((screen.getByRole("button", { name: /^Implantar$/ }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("enables the button when the plan describes the same rows in another order", async () => {
    const ok = planOutOfLayerOrder();
    ok.blocked = null;
    ok.steps = [
      step(0, 7, "ksia", 1),
      step(16, 23, "mac", 2),
      step(8, 15, "salem", 3),
      step(24, 31, "mac2", 4),
    ];
    post.mockResolvedValue(ok);
    mount();

    await waitFor(() => {
      const button = screen.getByRole("button", { name: /^Implantar$/ });
      expect((button as HTMLButtonElement).disabled).toBe(false);
    });
  });

  it("allows a machine with non-contiguous ranges when the coverage is valid", async () => {
    const ok = planOutOfLayerOrder();
    ok.blocked = null;
    ok.steps = [
      step(0, 7, "ksia", 1),
      step(8, 15, "salem", 2),
      step(16, 23, "ksia", 3),
      step(24, 31, "salem", 4),
    ];
    post.mockResolvedValue(ok);
    mount();

    await waitFor(() => {
      const button = screen.getByRole("button", { name: /^Implantar$/ });
      expect((button as HTMLButtonElement).disabled).toBe(false);
    });
  });

  it("submits the valid plan to the real deployment handler", async () => {
    const ok = planOutOfLayerOrder();
    post.mockImplementation((url: string) =>
      url === "/api/ui/deploy"
        ? Promise.resolve({ job_id: "job-valid" })
        : Promise.resolve(ok),
    );
    mount();

    const button = await screen.findByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(button);

    await waitFor(() => {
      expect(post).toHaveBeenCalledWith("/api/ui/deploy", expect.objectContaining({
        group: "qwen",
        kind: "shards",
      }));
    });
  });

  it("never authorizes copies: a plan the node blocks for a missing file names it", async () => {
    const ok = planOutOfLayerOrder();
    ok.copies = [{
      kind: "slice",
      name: "qwen-L8-15.gguf",
      size_bytes: 4 * 1024 ** 3,
      layers: [8, 15],
      source_node_id: "ksia",
      source_host: "ksia",
      target_node_id: "salem",
      target_host: "salem",
    }];
    ok.blocked = "salem has no qwen-L8-15.gguf (layers 8-15). Files are not copied between machines";
    post.mockImplementation((url: string) =>
      url === "/api/ui/deploy" ? Promise.resolve({ job_id: "job-copies" }) : Promise.resolve(ok),
    );
    mount();

    expect(await screen.findByText(/salem has no qwen-L8-15.gguf/)).toBeTruthy();
    const button = await screen.findByRole("button", { name: /^Implantar$/ });
    expect((button as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByTestId("deploy-copies")).toBeNull();
  });

  it("displays the reason of an automatic undeploy that blocked the deploy", async () => {
    const ok = planOutOfLayerOrder();
    ok.blocked = null;
    post.mockImplementation((url: string) =>
      url === "/api/ui/deploy" ? Promise.resolve({ job_id: "job-failed" }) : Promise.resolve(ok),
    );
    jobsResponse = {
      jobs: [{
        id: "job-failed",
        state: "failed",
        step: "aguardando recursos ociosos",
        message: "a máquina ksia não ficou ociosa dentro do limite; ficou ocupado: modelo=qwen",
        progress: null,
        outputs: [],
        started_at: new Date().toISOString(),
        finished_at: new Date().toISOString(),
      }],
    };
    mount();

    const deployButton = await screen.findByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((deployButton as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(deployButton);
    // The progress strip and the progress window both carry the reason.
    const alerts = await screen.findAllByRole("alert");
    expect(alerts).toHaveLength(2);
    for (const alert of alerts) expect(alert.textContent).toMatch(/ksia não ficou ociosa/);
    expect(screen.getByText("A implantação falhou")).toBeTruthy();
  });

  it("follows the deploy already running when the node refuses a second one", async () => {
    const ok = planOutOfLayerOrder();
    ok.blocked = null;
    post.mockImplementation((url: string) =>
      url === "/api/ui/deploy"
        ? Promise.reject(new ApiError(409, "already being deployed", { code: "deploy_in_progress", job_id: "job-running" }))
        : Promise.resolve(ok),
    );
    jobsResponse = {
      jobs: [{
        id: "job-running",
        state: "running",
        step: "carregando as fatias",
        message: "",
        progress: 40,
        outputs: [],
        started_at: new Date().toISOString(),
        finished_at: null,
      }],
    };
    mount();

    const deployButton = await screen.findByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((deployButton as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(deployButton);
    expect((await screen.findByTestId("deploy-progress")).textContent).toMatch(/carregando as fatias/);
  });

  it("follows the deploy it started in a window, with Close and Switch to dashboard", async () => {
    const ok = planOutOfLayerOrder();
    ok.blocked = null;
    post.mockImplementation((url: string) =>
      url === "/api/ui/deploy" ? Promise.resolve({ job_id: "job-running" }) : Promise.resolve(ok),
    );
    jobsResponse = {
      jobs: [{
        id: "job-running",
        state: "running",
        step: "carregando as fatias",
        message: "",
        progress: 40,
        outputs: [],
        started_at: new Date().toISOString(),
        finished_at: null,
      }],
    };
    mount();

    const deployButton = await screen.findByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((deployButton as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(deployButton);
    const progress = await screen.findByTestId("deploy-progress");
    expect(progress.textContent).toMatch(/carregando as fatias/);
    expect(screen.getByRole("button", { name: "Ir para o painel" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Fechar" }));
    await waitFor(() => expect(screen.queryByTestId("deploy-progress")).toBeNull());
  });

  it("allows all ranges when a single machine covers the model", async () => {
    const ok = planOutOfLayerOrder();
    ok.blocked = null;
    ok.steps = [0, 8, 16, 24].map((start, index) => step(start, start + 7, "ksia", index + 1));
    ok.machines = ok.machines.filter((machine) => machine.node_id === "ksia");
    post.mockResolvedValue(ok);
    mount();

    await waitFor(() => {
      const button = screen.getByRole("button", { name: /^Implantar$/ });
      expect((button as HTMLButtonElement).disabled).toBe(false);
    });
  });

  it("names each range without a machine instead of staying quiet", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = "a fatia L8-15 está sem máquina";
    p.complete = false;
    p.missing = [[8, 15]];
    // Grouped as the node grouped: the two ranges with no machine came out together,
    // so L24-31 came before L16-23.
    p.steps = [
      step(0, 7, "ksia", 1),
      step(8, 15, "", 2),
      step(24, 31, "", 3),
      step(16, 23, "mac", 4),
    ];
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => {
      expect(screen.getByText(/Não dá para implantar deste jeito/)).toBeTruthy();
    });
    expect(screen.getAllByText(/está sem máquina/).length).toBeGreaterThan(0);
    expect((screen.getByRole("button", { name: /^Implantar$/ }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("draws RTT between the hops and shows the machine layers in the inspector", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    p.route_options = [
      { from_node_id: "ksia", to_node_id: "salem", rtt_ms: 1.2, selected: true, note: "rota direta" },
      { from_node_id: "salem", to_node_id: "mac", rtt_ms: 2.8, selected: true, note: "rota direta" },
    ];
    p.machines = p.machines.map((machine, index) => ({
      ...machine,
      rtt_ms: index === 0 ? 1.2 : 2.8,
      gpu_free_mb: 12_000,
      ram_free_mb: 32_000,
      load: 0,
    }));
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    expect(screen.getAllByText(/RTT 1\.2 ms/).length).toBeGreaterThan(0);
    expect(screen.getAllByText(/RTT 2\.8 ms/).length).toBeGreaterThan(0);
    expect(screen.getByText("Camadas nesta máquina")).toBeTruthy();
    expect(screen.getAllByText("L0–7").length).toBeGreaterThan(0);
  });

  it("uses the machine RTT when the plan brings no route options", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    p.route_options = [];
    p.machines = p.machines.map((machine, index) => ({
      ...machine,
      rtt_ms: [1.2, 2.8, 4.4][index],
      load: 0,
    }));
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    const canvas = screen.getAllByText("Mapa da implantação")[0].closest("section");
    expect(canvas).not.toBeNull();
    expect(within(canvas!).getByText("RTT 2.8 ms")).toBeTruthy();
    expect(within(canvas!).getByText("RTT 4.4 ms")).toBeTruthy();
  });

  it("does not render the legacy panels below the canvas again", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    expect(screen.queryByText("Quais fatias entram, e em qual máquina")).toBeNull();
    expect(screen.queryByText("O que cada uma tem e quanto lhe sobra")).toBeNull();
  });

  it("reuses the canvas controls and allows assigning a layer to the selected machine", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    expect(screen.getByRole("button", { name: "Diminuir zoom" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Aumentar zoom" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Ajustar ao canvas" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Organizar máquinas" })).toBeTruthy();

    const hideInspector = screen.getByRole("button", { name: "Ocultar inspetor" });
    expect(hideInspector.className).toContain("w-6");
    expect(hideInspector.className).toContain("inset-y-0");
    fireEvent.click(hideInspector);
    expect(screen.queryByText("Camadas nesta máquina")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Mostrar inspetor" }));

    const assign = screen.getByRole("button", { name: "Atribuir L8–15 a ksia" });
    fireEvent.click(assign);
    await waitFor(() => {
      expect(
        post.mock.calls.some(([url, body]) =>
          url === "/api/ui/deploy/plan" &&
          JSON.stringify(body).includes('"start_layer":8') &&
          JSON.stringify(body).includes('"target_node_id":"ksia"'),
        ),
      ).toBe(true);
    });
  });

  it("does not silently swap the range of another machine when assigning a layer", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Atribuir L8–15 a ksia" }));

    await waitFor(() => {
      const selectionBody = post.mock.calls
        .map(([, body]) => body as { selection?: Array<{ start_layer: number; target_node_id: string }> })
        .find((body) => body.selection?.some((row) => row.start_layer === 8 && row.target_node_id === "ksia"));
      expect(selectionBody?.selection?.find((row) => row.start_layer === 8)?.target_node_id).toBe("salem");
      expect(selectionBody?.selection?.find((row) => row.start_layer === 0)?.target_node_id).toBe("ksia");
      expect(selectionBody?.selection?.filter((row) => row.start_layer === 8)).toHaveLength(2);
      expect(selectionBody?.selection?.find((row) => row.start_layer === 8 && row.target_node_id === "salem")).toBeTruthy();
    });
  });

  it("removes the row without a machine when the range is assigned from the map", async () => {
    const p = planWithUnassignedMiddleRange();
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    fireEvent.doubleClick(screen.getByRole("button", { name: /ksia-2.*RTT/ }));
    fireEvent.click(screen.getByRole("button", { name: "Atribuir L23–26 a ksia-2" }));

    await waitFor(() => {
      const selectionBody = post.mock.calls
        .map(([, body]) => body as { selection?: Array<{ start_layer: number; end_layer: number; target_node_id: string }> })
        .find((body) => body.selection?.some((row) => row.start_layer === 23 && row.target_node_id === "ksia-2"));
      expect(selectionBody?.selection?.filter((row) => row.start_layer === 23 && row.end_layer === 26)).toEqual([
        { start_layer: 23, end_layer: 26, target_node_id: "ksia-2", n_gpu_layers: null, offload_kqv: null },
      ]);
    });
  });

  /**
   * The defect this test guards: in the "Camadas disponíveis neste peer" block
   * the range already assigned became a `disabled` button. Selecting was a one
   * way path - the operator could put two ranges on the same machine and could
   * no longer take either of them off. Removal existed only per whole machine
   * ("Remover da implantação"), never per range.
   *
   * The original intent of the guard still holds: assigning a range to another
   * peer never *silently* removes what the peer already serves. A click by the
   * operator on the marked range is not silent removal - it is their order.
   */
  it("deselects a layer already assigned to the machine", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());

    // L0-7 already belongs to ksia in the plan: the button has to offer the way out, not be grey.
    const selected = screen.getByRole("button", { name: "Remover L0–7 de ksia" });
    expect((selected as HTMLButtonElement).disabled).toBe(false);

    fireEvent.click(selected);

    await waitFor(() => {
      const body = post.mock.calls
        .map(([, body]) => body as { selection?: Array<{ start_layer: number; target_node_id: string }> })
        .reverse()
        .find((body) => Array.isArray(body.selection));
      expect(body?.selection?.some((row) => row.start_layer === 0 && row.target_node_id === "ksia")).toBe(false);
      // The ranges of the other machines stay where they were.
      expect(body?.selection?.some((row) => row.start_layer === 8 && row.target_node_id === "salem")).toBe(true);
      expect(body?.selection?.some((row) => row.start_layer === 24 && row.target_node_id === "mac")).toBe(true);
    });
  });

  /** Take off and put back: the same button does both directions. */
  it("assigns the same layer again after removing it", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Remover L0–7 de ksia" }));

    const again = await screen.findByRole("button", { name: "Atribuir L0–7 a ksia" });
    fireEvent.click(again);

    await waitFor(() => {
      expect(screen.getByRole("button", { name: "Remover L0–7 de ksia" })).toBeTruthy();
    });
  });

  /**
   * The defect these tests guard: the switch to join neighbouring slices
   * existed, was deleted in the canvas rewrite (6340c49) and nobody noticed,
   * because everything around it survived - the `merge` state, the API field and
   * the strings of both languages. What was left was a refusal telling the
   * operator to "juntar as fatias vizinhas na tela", pointing at a control that
   * no longer existed, and the backend's capability (`splitter::merge`) became
   * unreachable through the interface.
   *
   * That is why the test covers both sides: that the control is on screen and
   * that it reaches the node. A control rendered and disconnected from the
   * request would be the same defect with another face.
   */
  it("offers the switch to merge neighboring slices in the Globals tab", async () => {
    const p = planOutOfLayerOrder();
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));

    const toggle = screen.getByRole("checkbox", { name: "Juntar fatias vizinhas na mesma máquina" });
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    // Non-contiguous ranges on the same machine are now valid without merge.
    expect(screen.queryByText(/ksia ficou com duas faixas/)).toBeNull();
    expect((screen.getByRole("button", { name: /^Implantar$/ }) as HTMLButtonElement).disabled).toBe(false);
  });

  it("sends merge to the node when the operator turns on merging neighboring slices", async () => {
    const p = planOutOfLayerOrder();
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "Juntar fatias vizinhas na mesma máquina" }));

    await waitFor(() => {
      expect(
        post.mock.calls.some(
          ([url, body]) =>
            url === "/api/ui/deploy/plan" && (body as { merge?: boolean }).merge === true,
        ),
      ).toBe(true);
    });
    await waitFor(() => {
      expect(
        screen
          .getByRole("checkbox", { name: "Juntar fatias vizinhas na mesma máquina" })
          .getAttribute("aria-checked"),
      ).toBe("true");
    });
  });

  it("includes in the canvas a peer added by double click", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    p.steps = p.steps.filter((item) => item.target_node_id !== "mac");
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    const availablePeer = screen.getByRole("button", { name: /mac RTT/ });
    fireEvent.doubleClick(availablePeer);
    await waitFor(() => {
      expect(post.mock.calls.some(([, body]) => (body as { preferred_node_ids?: string[] }).preferred_node_ids?.includes("mac"))).toBe(true);
      expect(screen.getByRole("button", { name: /mac.*disponível para o plano/ })).toBeTruthy();
    });
  });

  it("includes in the canvas a peer dragged from the bottom bar", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    p.steps = p.steps.filter((item) => item.target_node_id !== "mac");
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    const canvas = screen.getAllByText("Mapa da implantação")[0].closest("section");
    expect(canvas).not.toBeNull();
    fireEvent.drop(canvas!, { dataTransfer: { getData: () => "mac" } });

    await waitFor(() => {
      expect(post.mock.calls.some(([, body]) => (body as { preferred_node_ids?: string[] }).preferred_node_ids?.includes("mac"))).toBe(true);
      expect(screen.getByRole("button", { name: /mac.*disponível para o plano/ })).toBeTruthy();
    });
  });

  it("hides development controls without POM_DEV_MODE", () => {
    post.mockResolvedValue(planOutOfLayerOrder());
    mount();
    expect(screen.queryByRole("radio", { name: "Tensor" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Simples" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Completa" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Voltar ao plano do nó" })).toBeNull();
  });

  it("sends all peers as candidates when the strategy is complete", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockResolvedValue(p);
    mount(true, true);

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Completa" }));

    await waitFor(() => {
      expect(post.mock.calls.some(([, body]) => {
        const request = body as { strategy?: string; preferred_node_ids?: string[] };
        return request.strategy === "complete" && MACHINES.every((nodeId) => request.preferred_node_ids?.includes(nodeId));
      })).toBe(true);
    });
  });

  it("separates global and machine settings in tabs", async () => {
    post.mockResolvedValue(planOutOfLayerOrder());
    mount();

    await waitFor(() => expect(screen.getByRole("tab", { name: "Globais" })).toBeTruthy());
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));
    expect(screen.getByText("Configurações padrões")).toBeTruthy();
    expect(screen.queryByText("Camadas nesta máquina")).toBeNull();

    fireEvent.click(screen.getByRole("tab", { name: /Máquina:/ }));
    expect(screen.getByText("Camadas nesta máquina")).toBeTruthy();
  });

  it("removes a machine from the canvas and returns the peer to the footer", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockImplementation((_url: string, body?: { selection?: Array<{ target_node_id: string }> }) => {
      if (body?.selection) {
        return Promise.resolve({
          ...p,
          steps: p.steps.filter((step) => body.selection?.some((row) => row.target_node_id === step.target_node_id)),
        });
      }
      return Promise.resolve(p);
    });
    mount();

    await waitFor(() => expect(screen.getByRole("button", { name: "Remover ksia da implantação" })).toBeTruthy());
    const remove = screen.getByRole("button", { name: "Remover ksia da implantação" });
    fireEvent.pointerDown(remove);
    fireEvent.pointerUp(remove);
    fireEvent.click(remove);

    await waitFor(() => {
      expect(screen.getByRole("button", { name: /ksia.*RTT/ })).toBeTruthy();
      expect(screen.queryByRole("button", { name: "Remover ksia da implantação" })).toBeNull();
    });
  });

  it("does not animate the GPU of a machine without GPU", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    p.machines = p.machines.map((machine) => ({ ...machine, gpu_used_pct: null }));
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    const card = screen.getByRole("button", { name: /ksia na implantação/ });
    expect(within(card).getByText("sem GPU")).toBeTruthy();
    expect(within(card).queryAllByRole("progressbar")).toHaveLength(1);
  });
});
