// @vitest-environment jsdom
/**
 * A tela de implantar tem que dizer por que o botão está cinza.
 *
 * O defeito que estes testes guardam: `validated` comparava as linhas da tela
 * - que a tela mantém em ordem de camada - contra `plan.steps` posição a
 * posição, e o nó devolvia os passos agrupados por máquina de destino. Bastava
 * uma máquina ficar com duas faixas, ou duas faixas ficarem sem máquina, para
 * a ordem divergir. E como *todos* os painéis de explicação renderizavam sob
 * `validated`, o operador ficava exatamente com o que o captain relatou: botão
 * desabilitado e nenhuma explicação na tela.
 *
 * Os dois lados entram aqui: a comparação deixou de depender da ordem, e a
 * explicação deixou de depender da comparação.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DeployPlan, DeployStep } from "../lib/types";

const post = vi.fn();
vi.mock("../lib/api", () => ({
  api: {
    get: vi.fn((url: string) =>
      url === "/api/ui/jobs" ? Promise.resolve({ jobs: [] }) : undefined,
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
 * O plano como o nó o devolvia: agrupado por máquina, e por isso fora da ordem
 * de camadas. `ksia` fica com L0-7 e L16-23, então L16-23 vem antes de L8-15.
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
    blocked:
      "ksia ficou com duas faixas (L0-7 e L16-23) - junte as fatias vizinhas na tela, ou mova uma delas para outra máquina",
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
    } as DeployPlan["params"],
    prompt_tokens: 3840,
    chunk_tokens: 512,
  };
}

function mount() {
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
        shard_sets: [{ group: "qwen", shards: [] }],
      },
    ],
  });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <DeployModel />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

describe("DeployModel", () => {
  beforeEach(() => {
    post.mockReset();
    localStorage.clear();
  });
  afterEach(cleanup);

  it("explica a recusa mesmo quando o nó devolve os passos fora da ordem de camadas", async () => {
    post.mockResolvedValue(planOutOfLayerOrder());
    mount();

    // O motivo da recusa aparece - era exatamente ele que sumia.
    await waitFor(() => {
      expect(screen.getByText(/Não dá para implantar deste jeito/)).toBeTruthy();
    });
    expect(screen.getByText(/ksia ficou com duas faixas/)).toBeTruthy();

    const button = screen.getByRole("button", { name: /Implantar assim/ });
    expect((button as HTMLButtonElement).disabled).toBe(true);
  });

  it("libera o botão quando o plano descreve as mesmas linhas em outra ordem", async () => {
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
      const button = screen.getByRole("button", { name: /Implantar assim/ });
      expect((button as HTMLButtonElement).disabled).toBe(false);
    });
  });

  it("nomeia cada faixa sem máquina em vez de calar", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = "a fatia L8-15 está sem máquina";
    p.complete = false;
    p.missing = [[8, 15]];
    // Agrupado como o nó agrupava: as duas faixas sem máquina saíam juntas,
    // então L24-31 vinha antes de L16-23.
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
  });
});
