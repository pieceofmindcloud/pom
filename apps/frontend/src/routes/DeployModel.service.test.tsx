// @vitest-environment jsdom
/**
 * Deploying with a plugin's inference service (Strata).
 *
 * Choosing the service changes three things and nothing else: the mode becomes
 * whole model, the models offered are those of the architecture the engine runs
 * and the deployment carries `params.inference_service` - in the plan and in the deploy.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DeployPlan } from "../lib/types";

const CLUSTER = {
  peers: [
    {
      node_id: "ksia",
      node: {
        plugin_services: [
          {
            plugin_code: "strata",
            id: "strata",
            kind: "inference",
            name: "Strata",
            status: "ready",
            architectures: ["qwen4exp"],
            settings: [
              { key: "sampling.top_k", type: "integer", min: 1, max: 64,
                label: { en: "Default top_k", "pt-BR": "top_k padrão" } },
              { key: "fit_max_tokens", type: "boolean", label: { en: "Fit max_tokens", "pt-BR": "Ajustar max_tokens" } },
            ],
          },
        ],
      },
    },
    { node_id: "salem", node: { plugin_services: [] } },
  ],
};

/** What the cluster query answers; a test may swap it before mounting. */
let cluster: unknown = CLUSTER;
const post = vi.fn();
vi.mock("../lib/api", () => ({
  api: {
    get: vi.fn((url: string) =>
      url === "/api/ui/jobs"
        ? Promise.resolve({ jobs: [] })
        : url === "/api/ui/node"
          ? Promise.resolve({ connection_key: "", pom_dev_mode: false })
          : url === "/api/ui/cache-types"
            ? Promise.resolve({ cache_type_k: "f16", cache_type_v: "f16", choices: ["f16", "q4_0"] })
            : undefined,
    ),
    getConditional: vi.fn((url: string) =>
      Promise.resolve(url === "/api/ui/cluster" ? cluster : url === "/api/ui/jobs" ? { jobs: [] } : undefined),
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
  { node_id: "salem", label: "salem" },
];

function wholePlan(group: string): DeployPlan {
  return {
    group,
    kind: "whole",
    mmproj_path: null,
    total_layers: 48,
    total_layers_inferred: false,
    steps: [
      {
        position: 1,
        start_layer: 0,
        end_layer: 47,
        total_layers: 48,
        name: group,
        size_bytes: 75_000_000_000,
        target_node_id: "ksia",
        target_host: "ksia",
        ready: true,
        source_node_id: null,
        source_host: null,
        source_path: null,
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
      { start_layer: 0, end_layer: 47, total_layers: 48, name: group, size_bytes: 1, holders: ["ksia"], selected: true },
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
      n_ctx: 8192,
      n_batch: 512,
      max_sessions: 1,
      max_new_tokens: 256,
      prefill_chunk_tokens: 512,
      n_gpu_layers: 99,
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
    prompt_tokens: 7000,
    chunk_tokens: 512,
  };
}

function model(name: string, architecture: string) {
  return {
    id: name,
    name,
    source: "dir",
    source_label: "Folder",
    path: `/models/${name}.gguf`,
    size_bytes: 1,
    architecture,
    total_layers: 48,
    quant: "IQ3_XXS",
    shard_range: null,
    shard_group: null,
  };
}

function sliceSet(group: string, architecture: string) {
  const shard = (start: number, end: number) => ({
    ...model(`${group}-${start}`, architecture),
    shard_range: [start, end],
    shard_group: group,
  });
  return {
    model_label: group,
    pom_id: group,
    group,
    total_layers: 48,
    total_layers_inferred: false,
    shards: [shard(0, 23), shard(24, 47)],
    complete: true,
  };
}

function mount(shardSets: unknown[] = []) {
  post.mockImplementation((url: string, body: { group?: string }) =>
    Promise.resolve(url === "/api/ui/deploy" ? { job_id: "j1" } : wholePlan(body.group ?? "")),
  );
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, refetchInterval: false } } });
  client.setQueryData(keys.jobs, { jobs: [] });
  client.setQueryData(keys.node, { connection_key: "", pom_dev_mode: false });
  client.setQueryData(keys.cluster, cluster);
  client.setQueryData(keys.clusterModels, {
    peers: [
      {
        node_id: "ksia",
        hostname: "ksia",
        is_self: false,
        models: [model("llama-3-8b", "llama"), model("qwen3.8-flash-next", "qwen4exp")],
        shard_sets: shardSets,
        complete: true,
      },
    ],
  });
  render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <ToastProvider>
          <DeployModel />
        </ToastProvider>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

describe("DeployModel · plugin inference service", () => {
  beforeEach(() => {
    post.mockReset();
    localStorage.clear();
    cluster = CLUSTER;
  });

  it("with the engine that runs slices, offers only the slices of its architecture", async () => {
    cluster = {
      peers: CLUSTER.peers.map((peer) => ({
        ...peer,
        node: {
          plugin_services: peer.node.plugin_services.map((service) => ({ ...service, stages: true })),
        },
      })),
    };
    mount([sliceSet("llama-3-70b", "llama"), sliceSet("qwen3.8-flash-next", "qwen4exp")]);
    const model = (await screen.findByRole("combobox", { name: "Modelo técnico" })) as HTMLSelectElement;
    await waitFor(() => expect(model.options.length).toBe(2));

    fireEvent.change(screen.getByRole("combobox", { name: "Serviço de inferência" }), { target: { value: "strata" } });
    await waitFor(() =>
      expect([...model.options].map((option) => option.value)).toEqual(["qwen3.8-flash-next"]),
    );
    expect(screen.getByRole("radio", { name: "Fatias" }).getAttribute("aria-checked")).toBe("true");
  });

  it("without a plugin inference service, there is no engine choice", async () => {
    cluster = { peers: [{ node_id: "ksia", node: { plugin_services: [] } }] };
    mount();
    await screen.findByRole("combobox", { name: "Modelo técnico" });
    expect(screen.queryByRole("combobox", { name: "Serviço de inferência" })).toBeNull();
  });
  afterEach(cleanup);

  it("offers only the models the engine runs and sends the service in the plan and in the deploy", async () => {
    mount();
    const service = await screen.findByRole("combobox", { name: "Serviço de inferência" });
    fireEvent.change(service, { target: { value: "strata" } });

    const model = screen.getByRole("combobox", { name: "Modelo técnico" }) as HTMLSelectElement;
    await waitFor(() =>
      expect([...model.options].map((option) => option.value)).toEqual(["qwen3.8-flash-next"]),
    );
    expect((screen.getByRole("radio", { name: "Fatias" }) as HTMLButtonElement).disabled).toBe(true);

    await waitFor(() => {
      const plan = post.mock.calls.filter(([url]) => url === "/api/ui/deploy/plan").at(-1);
      expect(plan?.[1]).toMatchObject({
        group: "qwen3.8-flash-next",
        kind: "whole",
        params: { inference_service: "strata" },
      });
    });

    const add = await screen.findByRole("button", { name: /ksia RTT/ });
    fireEvent.click(add);
    expect(screen.queryByRole("button", { name: /salem RTT/ })).toBeNull();
    // GPU layers and the KV cache on the GPU are llama.cpp's: not offered for the plugin's engine.
    await screen.findByRole("button", { name: /Remover ksia da implantação/ });
    expect(screen.queryByRole("spinbutton", { name: "Camadas na GPU" })).toBeNull();
    expect(screen.queryByRole("checkbox", { name: "KV cache na GPU" })).toBeNull();

    fireEvent.click(await screen.findByRole("tab", { name: "Globais" }));
    expect(await screen.findByText("Configurações do Strata")).toBeTruthy();
    expect(screen.queryByRole("combobox", { name: "Motor de inferência" })).toBeNull();
    const topK = screen.getByRole("spinbutton", { name: "top_k padrão" });
    fireEvent.change(topK, { target: { value: "20" } });
    fireEvent.blur(topK);
    fireEvent.change(screen.getByRole("combobox", { name: "Ajustar max_tokens" }), { target: { value: "true" } });

    const button = screen.getByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(button);
    await waitFor(() => {
      const deploy = post.mock.calls.find(([url]) => url === "/api/ui/deploy");
      expect(deploy?.[1]).toMatchObject({
        group: "qwen3.8-flash-next",
        kind: "whole",
        params: { inference_service: "strata", engine_settings: { "sampling.top_k": 20, fit_max_tokens: true } },
      });
    });
  });

  it("a single machine takes the deployment's role, never a role left on its row", async () => {
    // Adding a second machine makes the first the split's prefill machine;
    // removing the second leaves one machine, whose inspector shows the
    // deployment's role - the `prefill` left on its row must not reach the
    // deploy (it made a deployment no machine answers for).
    const strata = { ...CLUSTER.peers[0].node.plugin_services[0], pom_settings: ["inference_role"] };
    cluster = {
      peers: [
        { node_id: "ksia", node: { plugin_services: [strata] } },
        { node_id: "salem", node: { plugin_services: [strata] } },
      ],
    };
    mount();
    fireEvent.change(await screen.findByRole("combobox", { name: "Serviço de inferência" }), {
      target: { value: "strata" },
    });
    fireEvent.click(await screen.findByRole("button", { name: /ksia RTT/ }));
    await screen.findByRole("button", { name: /Remover ksia da implantação/ });
    fireEvent.click(await screen.findByRole("button", { name: /salem RTT/ }));
    fireEvent.click(await screen.findByRole("button", { name: /Remover salem da implantação/ }));
    await waitFor(() => expect(screen.queryByRole("button", { name: /Remover salem da implantação/ })).toBeNull());

    const button = screen.getByRole("button", { name: /^Implantar$/ });
    await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(button);
    await waitFor(() => {
      const deploy = post.mock.calls.find(([url]) => url === "/api/ui/deploy");
      expect(deploy).toBeTruthy();
      const selection = (deploy?.[1] as { selection: Array<Record<string, unknown>> }).selection;
      expect(selection.map((row) => row.target_node_id)).toEqual(["ksia"]);
      expect(selection[0]).not.toHaveProperty("inference_role");
    });
  });
});
