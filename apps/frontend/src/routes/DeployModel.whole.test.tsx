// @vitest-environment jsdom
/**
 * Implantar um modelo inteiro pela mesma tela.
 *
 * O que estes testes guardam não é uma segunda tela - é que **não** existe uma.
 * A escolha de cima troca a pergunta que vai ao nó (`kind`) e some com as
 * perguntas que passam a ter uma resposta só; tudo o mais - o plano, a
 * validação, os ajustes, o botão - continua sendo o caminho das fatias.
 *
 * Os dois erros que fariam a tela mentir estão cobertos aqui:
 *
 * * mandar `kind` errado (ou não mandar), e o nó responder sobre fatias
 *   enquanto a tela mostra um modelo inteiro;
 * * deixar a validação de cobertura por camadas na tela - a fita de cobertura
 *   e a coluna "Entra", que num modelo inteiro são uma barra sólida e uma
 *   caixinha que nunca desmarca. Ruído fantasiado de escolha.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DeployPlan } from "../lib/types";

const post = vi.fn();
vi.mock("../lib/api", () => ({
  api: {
    get: vi.fn(),
    post: (...args: unknown[]) => post(...args),
  },
  getConnectionKey: () => "",
  setConnectionKey: () => {},
  ApiError: class extends Error {},
}));

import { DeployModel } from "./DeployModel";
import { LocaleProvider } from "../i18n/provider";
import { keys } from "../lib/queries";

const MACHINES = [
  { node_id: "ksia", label: "ksia" },
  { node_id: "mac", label: "mac" },
];

/** O plano de um modelo inteiro: um passo, todas as camadas, uma máquina. */
function wholePlan(target = "ksia"): DeployPlan {
  return {
    group: "qwen3.5-9b",
    kind: "whole",
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
    } as DeployPlan["params"],
    prompt_tokens: 3840,
    chunk_tokens: 512,
  };
}

/** O mesmo plano, mas sobre fatias - o que a tela pede por padrão. */
function shardPlan(): DeployPlan {
  const p = wholePlan();
  p.kind = "shards";
  p.group = "qwen";
  p.available[0].name = "qwen_0_31.gguf";
  return p;
}

/** Uma máquina com o GGUF inteiro **e** um conjunto de fatias, para que as
 *  duas opções existam e a troca entre elas seja de verdade. */
function mount() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, refetchInterval: false } },
  });
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

/** Responde cada rota com o plano que corresponde ao `kind` pedido. */
function answerPlans() {
  post.mockImplementation((url: string, body: { kind?: string }) => {
    if (url === "/api/ui/deploy") return Promise.resolve({ job_id: "j1" });
    return Promise.resolve(body?.kind === "whole" ? wholePlan() : shardPlan());
  });
}

const planCalls = () =>
  post.mock.calls.filter(([url]) => url === "/api/ui/deploy/plan");

const chooseWhole = () =>
  fireEvent.click(screen.getByRole("radio", { name: /Um modelo inteiro/ }));

describe("DeployModel · modelo inteiro", () => {
  beforeEach(() => {
    post.mockReset();
    localStorage.clear();
  });
  afterEach(cleanup);

  it("pergunta ao nó sobre fatias por padrão, e sobre o modelo inteiro quando escolhido", async () => {
    answerPlans();
    mount();

    await waitFor(() => expect(planCalls().length).toBeGreaterThan(0));
    expect(planCalls()[0][1]).toMatchObject({ group: "qwen", kind: "shards" });

    chooseWhole();

    await waitFor(() => {
      const last = planCalls().at(-1)![1];
      expect(last).toMatchObject({ group: "qwen3.5-9b", kind: "whole" });
    });
  });

  it("mostra tamanho, quantização e camadas do modelo escolhido", async () => {
    answerPlans();
    mount();
    chooseWhole();

    await waitFor(() => {
      expect(screen.getByText("Quantização")).toBeTruthy();
    });
    expect(screen.getByText("Q4_K_M")).toBeTruthy();
    expect(screen.getByText("32")).toBeTruthy();
    expect(screen.getByText(/4,7 GB|5,0 GB|4.7 GB/)).toBeTruthy();
    // Quem tem o arquivo é nomeado: é o que faz "receberia" ser previsível.
    expect(screen.getByText("Está em")).toBeTruthy();
  });

  /**
   * O ponto do defeito relatado: num modelo inteiro não existe faixa para
   * marcar nem camada reivindicada duas vezes, e mostrar esses controles é
   * inventar uma decisão que não existe.
   */
  it("não mostra a fita de cobertura nem a coluna de marcar faixa", async () => {
    answerPlans();
    mount();

    await waitFor(() => expect(screen.getByText(/Camadas cobertas/)).toBeTruthy());
    expect(screen.queryByText("Entra")).toBeTruthy();

    chooseWhole();

    await waitFor(() => {
      expect(screen.queryByText(/Camadas cobertas/)).toBeNull();
    });
    expect(screen.queryByText("Entra")).toBeNull();
    expect(screen.queryByRole("checkbox", { name: /Incluir/ })).toBeNull();
    // No lugar delas, a faixa inteira como um fato, não como uma escolha.
    expect(screen.getAllByText("L0–31").length).toBeGreaterThan(0);
  });

  it("deixa escolher a máquina e manda a faixa inteira na seleção", async () => {
    answerPlans();
    mount();
    chooseWhole();

    const select = await screen.findByRole("combobox", {
      name: /Máquina que serve o modelo inteiro/,
    });
    fireEvent.change(select, { target: { value: "mac" } });

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

  it("implanta mandando o mesmo `kind` que planejou", async () => {
    answerPlans();
    mount();
    chooseWhole();

    const button = await screen.findByRole("button", { name: /Implantar assim/ });
    await waitFor(() => expect((button as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(button);

    await waitFor(() => {
      const deploys = post.mock.calls.filter(([url]) => url === "/api/ui/deploy");
      expect(deploys).toHaveLength(1);
      expect(deploys[0][1]).toMatchObject({ group: "qwen3.5-9b", kind: "whole" });
    });
  });

  /**
   * A recusa do nó tem que aparecer aqui como aparece nas fatias. Sem isto o
   * operador vê um botão cinza e nenhum motivo, que é o defeito que a suíte
   * irmã guarda para o outro caminho.
   */
  it("mostra a recusa do nó e mantém o botão cinza", async () => {
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
    const button = screen.getByRole("button", { name: /Implantar assim/ });
    expect((button as HTMLButtonElement).disabled).toBe(true);
  });

  /** Uma máquina que ainda não tem o arquivo é uma cópia, e a tela diz de onde. */
  it("diz de onde o arquivo vem quando a máquina escolhida não o tem", async () => {
    post.mockImplementation((url: string, body: { kind?: string }) =>
      Promise.resolve(body?.kind === "whole" ? wholePlan("mac") : shardPlan()),
    );
    mount();
    chooseWhole();

    await waitFor(() => {
      expect(screen.getByText(/copiar de ksia/)).toBeTruthy();
    });
  });

  /**
   * Um nó de binário antigo não devolve `kind` nenhum, e ele só sabe responder
   * sobre fatias. Ler a ausência como "fatias" é o que impede a tela de emudecer
   * durante uma atualização em andamento.
   */
  it("lê um plano sem `kind` como um plano de fatias", async () => {
    post.mockImplementation(() => {
      const p = shardPlan();
      delete (p as { kind?: string }).kind;
      return Promise.resolve(p);
    });
    mount();

    await waitFor(() => {
      const button = screen.getByRole("button", { name: /Implantar assim/ });
      expect((button as HTMLButtonElement).disabled).toBe(false);
    });
    expect(screen.queryByText("Entra")).toBeTruthy();
  });
});
