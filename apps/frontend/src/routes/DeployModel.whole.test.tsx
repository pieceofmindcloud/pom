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
import type { DeployPlan, RuntimeParams } from "../lib/types";

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

const MACHINES = [
  { node_id: "ksia", label: "ksia" },
  { node_id: "mac", label: "mac" },
  { node_id: "salem", label: "salem" },
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
      fit_memory: true,
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

/** O mesmo plano, mas sobre fatias - o que a tela pede por padrão. */
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

/** Uma máquina com o GGUF inteiro **e** um conjunto de fatias, para que as
 *  duas opções existam e a troca entre elas seja de verdade. */
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
        shard_sets: [{
          group: "qwen",
          model_label: "qwen3.5-9b",
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
              shard_group: "qwen",
              sha256: "tp0",
              model_label: "qwen3.5-9b",
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
              shard_group: "qwen",
              sha256: "tp1",
              model_label: "qwen3.5-9b",
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
        shard_sets: node_id === "mac" ? [{
          group: "qwen",
          model_label: "qwen3.5-9b",
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
            shard_group: "qwen",
            sha256: "tp1",
            model_label: "qwen3.5-9b",
            pom_id: "pom-qwen",
            tp_rank: 1,
            tp_size: 2,
          }],
          complete: false,
        }] : [],
      })),
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

const chooseTensor = () => fireEvent.click(screen.getByRole("radio", { name: "Tensor" }));

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

    await waitFor(() => expect(screen.getByText(/Cobertura do modelo/)).toBeTruthy());
    expect(screen.getAllByText("L0–31").length).toBeGreaterThan(0);

    chooseWhole();

    await waitFor(() => {
      expect(screen.queryByText(/Cobertura do modelo/)).toBeNull();
    });
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

  it("adiciona uma réplica do modelo inteiro ao incluir outra máquina no mapa", async () => {
    answerPlans();
    mount();
    chooseWhole();

    await waitFor(() => expect(screen.getByText("Quantização")).toBeTruthy());
    fireEvent.click(await screen.findByRole("button", { name: /mac RTT/ }));

    await waitFor(() => {
      const last = planCalls().at(-1)![1] as {
        selection?: Array<{
          start_layer: number;
          end_layer: number;
          target_node_id: string;
          n_gpu_layers: number | null;
          offload_kqv: boolean | null;
        }>;
      };
      expect(last.selection).toEqual([
        { start_layer: 0, end_layer: 31, target_node_id: "ksia", n_gpu_layers: null, offload_kqv: null },
        { start_layer: 0, end_layer: 31, target_node_id: "mac", n_gpu_layers: null, offload_kqv: null },
      ]);
    });
  });

  it("implanta mandando o mesmo `kind` que planejou", async () => {
    answerPlans();
    mount();
    chooseWhole();

    const button = await screen.findByRole("button", { name: /^Implantar$/ });
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
    const button = screen.getByRole("button", { name: /^Implantar$/ });
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
      const button = screen.getByRole("button", { name: /^Implantar$/ });
      expect((button as HTMLButtonElement).disabled).toBe(false);
    });
    expect(screen.getAllByText("L0–31").length).toBeGreaterThan(0);
  });

  it("permite ligar e desligar o ajuste automático de memória", async () => {
    answerPlans();
    mount();
    chooseWhole();

    const toggle = await screen.findByRole("checkbox", {
      name: /Ajustar à memória/,
    });
    expect(toggle.getAttribute("aria-checked")).toBe("true");

    fireEvent.click(toggle);
    await waitFor(() => {
      const last = planCalls().at(-1)![1] as { params: RuntimeParams };
      expect(last.params.fit_memory).toBe(false);
    });
  });
});

describe("DeployModel · MTP", () => {
  beforeEach(() => {
    post.mockReset();
    localStorage.clear();
  });
  afterEach(cleanup);

  /** Plano inteiro com MTP desligado, que é o padrão honesto. */
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
          <DeployModel />
        </LocaleProvider>
      </QueryClientProvider>,
    );
  }

  it("liga o MTP, vê os controles numéricos e os manda na implantação", async () => {
    mountWhole();
    chooseWhole();

    const switchEl = await screen.findByRole("checkbox", {
      name: /Predição multi-token/,
    });
    expect(switchEl).toBeTruthy();
    fireEvent.click(switchEl);

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
      expect(body.params.mtp_draft_tokens).toBe(5);
    });
  });

  /// "Voltar ao plano do nó" tem que desfazer **também** o que está meio
  /// digitado.
  ///
  /// O texto em edição vive dentro do `RuntimeParamsForm` desde que ele saiu
  /// desta tela. Quem limpava o rascunho aqui era um `setDrafts({})` que deixou
  /// de existir: sem remontar o formulário, o botão zerava os parâmetros por
  /// baixo e o campo continuava mostrando o número velho — a tela dizendo uma
  /// coisa e o pedido levando outra.
  it("restaurar o plano do nó também limpa o campo meio digitado", async () => {
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
          <DeployModel />
        </LocaleProvider>
      </QueryClientProvider>,
    );

    const campo = () =>
      document.querySelector('input[name="n_ctx"]') as HTMLInputElement | null;
    await waitFor(() => expect(campo()).toBeTruthy());
    // O plano do nó traz 4096 (ver `wholePlan`).
    await waitFor(() => expect(campo()!.value).toBe("4096"));

    fireEvent.change(campo()!, { target: { value: "65536" } });
    await waitFor(() => expect(campo()!.value).toBe("65536"));

    fireEvent.click(screen.getByRole("button", { name: /Voltar ao plano do nó/ }));

    await waitFor(() => expect(campo()!.value).toBe("4096"));
  });

  it("em modo fatiado o interruptor do MTP está habilitado e liga os controles", async () => {
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
          <DeployModel />
        </LocaleProvider>
      </QueryClientProvider>,
    );

    fireEvent.click(screen.getByRole("tab", { name: "Globais" }));
    const switchEl = await screen.findByRole("checkbox", {
      name: /Predição multi-token/,
    });
    expect(switchEl.getAttribute("aria-checked")).toBe("false");
    expect((switchEl as HTMLInputElement).disabled).toBe(false);

    fireEvent.click(switchEl);

    await waitFor(() => {
      expect(
        screen.getByText("Tokens rascunhados pelo MTP"),
      ).toBeTruthy();
    });
  });

  it("o MTP desligado por padrão: o interruptor nasce apagado", async () => {
    mountWhole(false);

    await waitFor(async () => {
      chooseWhole();
    });

    const switchEl = await screen.findByRole("checkbox", {
      name: /Predição multi-token/,
    });
    expect(switchEl.getAttribute("aria-checked")).toBe("false");
    // Sem MTP ligado, os controles numéricos não aparecem.
    expect(
      screen.queryByText("Tokens rascunhados pelo MTP"),
    ).toBeNull();
    expect(
      screen.queryByText("Probabilidade mínima do MTP"),
    ).toBeNull();
  });

  it("o MTP e a especulação por lookup são independentes — ligar um não mexe no outro", async () => {
    mountWhole();
    chooseWhole();

    const mtpSwitch = await screen.findByRole("checkbox", {
      name: /Predição multi-token/,
    });
    fireEvent.click(mtpSwitch);

    await waitFor(() => {
      expect(screen.getByText("Tokens rascunhados pelo MTP")).toBeTruthy();
    });

    // The speculative decoding switch should still be off.
    const specSwitch = screen.getByRole("checkbox", {
      name: /Decodificação especulativa/,
    });
    expect(specSwitch.getAttribute("aria-checked")).toBe("false");

    // And none of the speculative knobs are visible.
    expect(screen.queryByText("Largura do rascunho")).toBeNull();
    expect(screen.queryByText("Janela de busca")).toBeNull();
  });
});

describe("DeployModel · Tensor simples", () => {
  beforeEach(() => {
    post.mockReset();
    localStorage.clear();
  });
  afterEach(cleanup);

  it("monta os ranks apenas com as máquinas que estão no mapa", async () => {
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

  it("escolhe o grupo TP fatiado e mostra os ranks, não camadas de pipeline", async () => {
    answerPlans();
    mount();
    chooseTensor();

    const model = await screen.findByRole("combobox", { name: "Modelo técnico" });
    expect((model as HTMLSelectElement).value).toBe("qwen:2");
    expect(screen.getByText(/1of2/)).toBeTruthy();
    expect(screen.getByText(/2of2/)).toBeTruthy();
    expect(screen.queryByText("Camadas disponíveis neste peer")).toBeNull();
  });
});
