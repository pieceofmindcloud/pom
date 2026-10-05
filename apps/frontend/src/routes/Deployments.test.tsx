// @vitest-environment jsdom
/**
 * O que esta tela promete: mostrar **todas** as implantações do cluster, sem
 * confundir "no ar" com "padrão", e deixar mexer num parâmetro sem refazer a
 * implantação inteira.
 *
 * O caso que mais importa é o que era inexprimível antes do `deploy_id`: o
 * mesmo modelo no ar duas vezes. Se a tela colapsar as duas em uma linha, ou
 * marcar as duas como padrão, o operador reimplanta a errada.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Deployments } from "./Deployments";
import { LocaleProvider } from "../i18n/provider";
import type { DeploymentRow } from "../lib/api";
import type { RuntimeParams } from "../lib/types";

const listDeployments = vi.fn();
const redeployDeployment = vi.fn();
const activateDeployment = vi.fn();

vi.mock("../lib/api", () => ({
  listDeployments: (...a: unknown[]) => listDeployments(...a),
  redeployDeployment: (...a: unknown[]) => redeployDeployment(...a),
  activateDeployment: (...a: unknown[]) => activateDeployment(...a),
  undeployPipeline: vi.fn(async () => ({
    group: "qwen3:8b",
    released: 1,
    failed: 0,
    results: [],
  })),
  // `useJobs` passa por aqui; sem uma resposta de verdade o react-query
  // reclama de `undefined` e polui a saída do teste.
  api: {
    get: vi.fn(async (url: string) =>
      url === "/api/ui/node" ? { connection_key: "", pom_desenv: true } : { jobs: [] },
    ),
    post: vi.fn(async () => ({})),
    del: vi.fn(async () => ({})),
  },
  getConnectionKey: () => "k",
  setConnectionKey: vi.fn(),
  ApiError: class extends Error {},
}));

const params: RuntimeParams = {
  inference_mode: "auto",
  n_gpu_layers: 99,
  n_ctx: 8192,
  offload_kqv: true,
  fit_memory: true,
  fit_memory_margin_mb: 256,
  n_batch: 512,
  max_new_tokens: 512,
  max_sessions: 1,
  prefill_chunk_tokens: 512,
  prefill_max_inflight_chunks: 2,
  temperature: 0.7,
  top_p: 0.95,
  top_k: 40,
  repeat_penalty: 1.1,
  repeat_last_n: 64,
  no_thinking: true,
  no_embeddings: false,
  speculative_decoding_enabled: false,
  speculative_k: 4,
  speculative_lookup_window: 3,
  mtp_enabled: false,
  mtp_draft_tokens: 3,
  mtp_min_probability: 0.1,
};

function row(
  id: string,
  opts: {
    serving: boolean;
    isDefault: boolean;
    kind?: "shards" | "whole";
    versions?: { version: number; n_ctx: number }[];
    activeVersion?: number;
    activatedAt?: string;
  },
): DeploymentRow {
  const nodes = [
    {
      node_id: "no-a",
      hostname: "ksia",
      start_layer: 0,
      end_layer: 31,
      peer_index: 0,
      peer_count: 1,
      primary: true,
      config: { ...params },
    },
  ];
  const versionSpecs = opts.versions ?? [{ version: 1, n_ctx: 8192 }];
  const versions = versionSpecs.map((v) => ({
    version: v.version,
    created_at: "2026-08-19T00:00:00Z",
    created_by: "coordenador",
    params: { ...params, n_ctx: v.n_ctx },
    nodes,
  }));
  return {
    deployment: {
      deployment_version: 2,
      deploy_id: id,
      kind: opts.kind ?? "shards",
      model: { label: "qwen3:8b", total_layers: 32 },
      created_at: "2026-08-19T00:00:00Z",
      created_by: "coordenador",
      activated_at: opts.activatedAt ?? "2026-08-19T00:00:00Z",
      nodes,
      versions,
      active_version: opts.activeVersion ?? versionSpecs[versionSpecs.length - 1].version,
    },
    serving: opts.serving,
    is_default: opts.isDefault,
    holders: ["no-a"],
  };
}

function mount() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, refetchInterval: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <Deployments />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

function click(el: Element) {
  act(() => {
    fireEvent.click(el);
  });
}

beforeEach(() => {
  listDeployments.mockReset();
  redeployDeployment.mockReset();
  activateDeployment.mockReset();
});

afterEach(cleanup);

describe("Deployments", () => {
  it("mostra as duas implantações do mesmo modelo, com estados distintos", async () => {
    listDeployments.mockResolvedValue({
      node_id: "eu",
      deployments: [
        row("d-aaaaaaaa", { serving: true, isDefault: false }),
        row("d-bbbbbbbb", { serving: false, isDefault: true, kind: "whole" }),
      ],
    });

    mount();

    await waitFor(() => expect(screen.getByText("d-aaaaaaaa")).toBeTruthy());
    expect(screen.getByText("d-bbbbbbbb")).toBeTruthy();

    // Uma está no ar sem ser padrão; a outra é padrão sem estar no ar. Se a
    // tela tratasse as duas palavras como sinônimo, um destes sumiria.
    expect(screen.getByText("No ar")).toBeTruthy();
    expect(screen.getByText("Fora do ar")).toBeTruthy();
    expect(screen.getAllByText("Padrão")).toHaveLength(1);

    // E a forma de cada uma aparece, que é o que distingue as duas na prática.
    expect(screen.getByText("Fatias")).toBeTruthy();
    expect(screen.getByText("Modelo inteiro")).toBeTruthy();
  });

  it("organiza pela última ativação de versão, não pela criação", async () => {
    listDeployments.mockResolvedValue({
      node_id: "no-a",
      deployments: [
        row("d-old", {
          serving: false,
          isDefault: false,
          activatedAt: "2026-08-20T12:00:00Z",
        }),
        row("d-new", {
          serving: true,
          isDefault: true,
          activatedAt: "2026-08-29T12:00:00Z",
        }),
      ],
    });

    mount();
    const table = await screen.findByRole("table");
    // A primeira coluna é o controle de expansão; o `deploy_id` é a segunda.
    const cells = Array.from(table.querySelectorAll("tbody tr")).map((tr) => tr.cells[1]);

    expect(cells.map((cell) => cell.textContent ?? "")).toEqual([
      expect.stringContaining("d-new"),
      expect.stringContaining("d-old"),
    ]);
    expect(table.querySelectorAll("thead th")[1]?.textContent).toBe("Id");
    expect(cells[0].className).toContain("px-5");
    expect(screen.getByText("Última ativação")).toBeTruthy();
  });

  it("reimplanta mandando o parâmetro mexido e preservando o resto", async () => {
    listDeployments.mockResolvedValue({
      node_id: "eu",
      deployments: [row("d-aaaaaaaa", { serving: true, isDefault: true })],
    });
    redeployDeployment.mockResolvedValue({ job_id: "j1", deploy_id: "d-aaaaaaaa" });

    mount();
    await waitFor(() => expect(screen.getByText("d-aaaaaaaa")).toBeTruthy());

    click(screen.getByText("Reimplantar com novos parâmetros"));

    // O formulário abre preenchido com o que a implantação está rodando.
    const nCtx = (await screen.findByRole("spinbutton", { name: /contexto|n_ctx/i })) as
      | HTMLInputElement
      | undefined;
    const campo = nCtx ?? (document.querySelector('input[name="n_ctx"]') as HTMLInputElement);
    expect(campo.value).toBe("8192");

    act(() => {
      fireEvent.change(campo, { target: { value: "16384" } });
    });
    click(screen.getByText("Recarregar com estes valores"));

    await waitFor(() => expect(redeployDeployment).toHaveBeenCalled());
    const [id, body] = redeployDeployment.mock.calls[0] as [
      string,
      { params: RuntimeParams; per_node: Record<string, unknown> },
    ];

    // Mesmo id: reimplantar mexendo num parâmetro não cria outra implantação,
    // senão todo cliente presa a `modelo@d-aaaaaaaa` deixaria de achar.
    expect(id).toBe("d-aaaaaaaa");
    expect(body.params.n_ctx).toBe(16384);
    // O resto vai junto como estava: mandar só o campo mexido faria o nó
    // recarregar com os padrões nos outros vinte.
    expect(body.params.temperature).toBe(0.7);
    expect(body.params.n_batch).toBe(512);
    expect(body.per_node["no-a"]).toBeTruthy();
  });

  it("só oferece tornar padrão para quem ainda não é", async () => {
    listDeployments.mockResolvedValue({
      node_id: "eu",
      deployments: [
        row("d-aaaaaaaa", { serving: true, isDefault: true }),
        row("d-bbbbbbbb", { serving: true, isDefault: false }),
      ],
    });
    activateDeployment.mockResolvedValue({
      activated: true,
      deploy_id: "d-bbbbbbbb",
      model: "qwen3:8b",
    });

    mount();
    await waitFor(() => expect(screen.getByText("d-bbbbbbbb")).toBeTruthy());

    const botoes = screen.getAllByText("Tornar padrão");
    expect(botoes).toHaveLength(1);

    click(botoes[0]);
    await waitFor(() => expect(activateDeployment).toHaveBeenCalledWith("d-bbbbbbbb"));
  });

  it("não oferece ação nenhuma quando não há implantação", async () => {
    listDeployments.mockResolvedValue({ node_id: "eu", deployments: [] });
    mount();
    await waitFor(() => expect(screen.getByText("Nada implantado ainda")).toBeTruthy());
    expect(screen.queryByText("Reimplantar com novos parâmetros")).toBeNull();
  });

  it("expande uma linha e reimplanta uma versão pelo número", async () => {
    listDeployments.mockResolvedValue({
      node_id: "eu",
      deployments: [
        row("d-aaaaaaaa", {
          serving: true,
          isDefault: true,
          versions: [
            { version: 1, n_ctx: 8192 },
            { version: 2, n_ctx: 16384 },
          ],
          activeVersion: 2,
        }),
      ],
    });
    redeployDeployment.mockResolvedValue({ job_id: "j9", deploy_id: "d-aaaaaaaa" });

    mount();
    await waitFor(() => expect(screen.getByText("d-aaaaaaaa")).toBeTruthy());

    // Fechada, as versões não aparecem.
    expect(screen.queryByText("Reimplantar esta versão")).toBeNull();

    click(screen.getByRole("button", { name: "Ver versões" }));

    // As duas versões, com a ativa marcada.
    expect(screen.getByText("v1")).toBeTruthy();
    expect(screen.getByText("v2")).toBeTruthy();
    expect(screen.getByText("Ativa")).toBeTruthy();

    const botoes = screen.getAllByText("Reimplantar esta versão");
    // v2 primeiro (mais recente); clicar em v1 é o segundo.
    click(botoes[1]);

    await waitFor(() => expect(redeployDeployment).toHaveBeenCalledWith("d-aaaaaaaa", {
      version: 1,
    }));
  });

  it("edita e reimplanta a partir de uma versão, no mesmo inspetor à direita", async () => {
    listDeployments.mockResolvedValue({
      node_id: "eu",
      deployments: [
        row("d-aaaaaaaa", {
          serving: true,
          isDefault: true,
          versions: [
            { version: 1, n_ctx: 8192 },
            { version: 2, n_ctx: 16384 },
          ],
          activeVersion: 2,
        }),
      ],
    });
    redeployDeployment.mockResolvedValue({ job_id: "j3", deploy_id: "d-aaaaaaaa" });

    mount();
    await waitFor(() => expect(screen.getByText("d-aaaaaaaa")).toBeTruthy());
    click(screen.getByRole("button", { name: "Ver versões" }));
    click(screen.getByRole("button", { name: "Ver as propriedades da versão 1" }));
    click(screen.getByText("Editar e reimplantar"));

    // O painel de leitura dá lugar ao de edição, semeado com a versão 1.
    expect(screen.queryByText("Propriedades da versão 1")).toBeNull();
    const panel = screen.getByText("Reimplantar qwen3:8b").closest("aside")!;
    const nCtx = panel.querySelector('input[name="n_ctx"]') as HTMLInputElement;
    expect(nCtx.value).toBe("8192");
    click(within(panel).getByText("Recarregar com estes valores"));
    await waitFor(() => expect(redeployDeployment).toHaveBeenCalled());
    const [, body] = redeployDeployment.mock.calls[0] as [string, { params: RuntimeParams }];
    expect(body.params.n_ctx).toBe(8192);
  });

  /**
   * O mesmo inspetor da tela de implantar, aqui em somente leitura: é o que
   * responde "com que parâmetros esta versão subiu?" sem obrigar a abrir o
   * diálogo de reimplantar, que é um formulário editável e sugere mudança.
   */
  it("abre as propriedades de uma versão em somente leitura", async () => {
    listDeployments.mockResolvedValue({
      node_id: "eu",
      deployments: [
        row("d-aaaaaaaa", {
          serving: true,
          isDefault: true,
          versions: [
            { version: 1, n_ctx: 8192 },
            { version: 2, n_ctx: 16384 },
          ],
          activeVersion: 2,
        }),
      ],
    });

    mount();
    await waitFor(() => expect(screen.getByText("d-aaaaaaaa")).toBeTruthy());
    click(screen.getByRole("button", { name: "Ver versões" }));

    // Fechado, o inspetor não existe.
    expect(screen.queryByText("Configurações padrões")).toBeNull();

    click(screen.getByRole("button", { name: "Ver as propriedades da versão 1" }));

    expect(screen.getByText("Propriedades da versão 1")).toBeTruthy();
    expect(screen.getByText("Configurações padrões")).toBeTruthy();
    expect(screen.getByText("Configurações POM")).toBeTruthy();
    expect(screen.getByText("Configurações POM (DESENV)")).toBeTruthy();

    // Somente leitura de verdade: o valor está escrito, não num campo.
    const inspector = screen.getByText("Configurações padrões").closest("aside");
    expect(inspector).toBeTruthy();
    expect(inspector!.querySelectorAll("input")).toHaveLength(0);
    expect(inspector!.querySelectorAll("select")).toHaveLength(0);
    expect(within(inspector!).getByText("8192")).toBeTruthy();
    // E o interruptor do plano continua legível, sem ser acionável.
    const noThinking = within(inspector!).getByRole("checkbox", {
      name: "Desativar thinking",
    }) as HTMLButtonElement;
    expect(noThinking.getAttribute("aria-checked")).toBe("true");
    expect(noThinking.disabled).toBe(true);

    click(screen.getByRole("button", { name: "Fechar as propriedades" }));
    expect(screen.queryByText("Configurações padrões")).toBeNull();
  });
});
