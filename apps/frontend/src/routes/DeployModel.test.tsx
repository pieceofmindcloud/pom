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
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DeployPlan, DeployStep } from "../lib/types";

const post = vi.fn();
vi.mock("../lib/api", () => ({
  api: {
    get: vi.fn((url: string) =>
      url === "/api/ui/jobs"
        ? Promise.resolve({ jobs: [] })
        : url === "/api/ui/cluster"
          ? Promise.resolve({ peers: [] })
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

    const button = screen.getByRole("button", { name: /Implantar/ });
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
      const button = screen.getByRole("button", { name: /^Implantar$/ });
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

  it("desenha RTT entre os hops e mostra as camadas da máquina no inspetor", async () => {
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
    expect(screen.getAllByText("1.2 ms").length).toBeGreaterThan(0);
    expect(screen.getAllByText("2.8 ms").length).toBeGreaterThan(0);
    expect(screen.getByText("Camadas nesta máquina")).toBeTruthy();
    expect(screen.getAllByText("L0–7").length).toBeGreaterThan(0);
  });

  it("usa o RTT da máquina quando o plano não traz opções de rota", async () => {
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

  it("não renderiza novamente os painéis legados abaixo do canvas", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    expect(screen.queryByText("Quais fatias entram, e em qual máquina")).toBeNull();
    expect(screen.queryByText("O que cada uma tem e quanto lhe sobra")).toBeNull();
  });

  it("reaproveita os controles do canvas e permite atribuir uma camada à máquina selecionada", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    expect(screen.getByRole("button", { name: "Diminuir zoom" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Aumentar zoom" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Ajustar ao canvas" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Organizar máquinas" })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Ocultar inspetor" }));
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

  it("não troca silenciosamente a faixa de outra máquina ao atribuir uma camada", async () => {
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

  it("inclui no canvas um peer adicionado por duplo clique", async () => {
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

  it("inclui no canvas um peer arrastado da barra inferior", async () => {
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

  it("manda todos os peers como candidatos quando a estratégia é completa", async () => {
    const p = planOutOfLayerOrder();
    p.blocked = null;
    post.mockResolvedValue(p);
    mount();

    await waitFor(() => expect(screen.getByText("Camadas nesta máquina")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Completa" }));

    await waitFor(() => {
      expect(post.mock.calls.some(([, body]) => {
        const request = body as { strategy?: string; preferred_node_ids?: string[] };
        return request.strategy === "complete" && MACHINES.every((nodeId) => request.preferred_node_ids?.includes(nodeId));
      })).toBe(true);
    });
  });

  it("separa configurações globais e da máquina em tabs", async () => {
    post.mockResolvedValue(planOutOfLayerOrder());
    mount();

    await waitFor(() => expect(screen.getByRole("tab", { name: "Globais" })).toBeTruthy());
    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));
    expect(screen.getByText("Configurações globais")).toBeTruthy();
    expect(screen.queryByText("Camadas nesta máquina")).toBeNull();

    fireEvent.click(screen.getByRole("tab", { name: /Máquina:/ }));
    expect(screen.getByText("Camadas nesta máquina")).toBeTruthy();
  });

  it("remove uma máquina do canvas e devolve o peer ao rodapé", async () => {
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

  it("não anima GPU de uma máquina sem GPU", async () => {
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
