// @vitest-environment jsdom
/**
 * Implantar com o serviço de inferência de um plugin (Strata).
 *
 * A escolha do serviço muda três coisas e nada mais: o modo vira modelo
 * inteiro, os modelos oferecidos são os da arquitetura que o motor roda e a
 * implantação leva `params.inference_service` - no plano e no deploy.
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

const post = vi.fn();
vi.mock("../lib/api", () => ({
  api: {
    get: vi.fn((url: string) =>
      url === "/api/ui/jobs"
        ? Promise.resolve({ jobs: [] })
        : url === "/api/ui/node"
          ? Promise.resolve({ connection_key: "", pom_desenv: false })
          : url === "/api/ui/cache-types"
            ? Promise.resolve({ cache_type_k: "f16", cache_type_v: "f16", choices: ["f16", "q4_0"] })
            : undefined,
    ),
    getConditional: vi.fn((url: string) =>
      Promise.resolve(url === "/api/ui/cluster" ? CLUSTER : url === "/api/ui/jobs" ? { jobs: [] } : undefined),
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

function mount() {
  post.mockImplementation((url: string, body: { group?: string }) =>
    Promise.resolve(url === "/api/ui/deploy" ? { job_id: "j1" } : wholePlan(body.group ?? "")),
  );
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, refetchInterval: false } } });
  client.setQueryData(keys.jobs, { jobs: [] });
  client.setQueryData(keys.node, { connection_key: "", pom_desenv: false });
  client.setQueryData(keys.cluster, CLUSTER);
  client.setQueryData(keys.clusterModels, {
    peers: [
      {
        node_id: "ksia",
        hostname: "ksia",
        is_self: false,
        models: [model("llama-3-8b", "llama"), model("qwen3.8-flash-next", "qwen4exp")],
        shard_sets: [],
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

describe("DeployModel · serviço de inferência de plugin", () => {
  beforeEach(() => {
    post.mockReset();
    localStorage.clear();
  });
  afterEach(cleanup);

  it("oferece só os modelos que o motor roda e manda o serviço no plano e no deploy", async () => {
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
});
