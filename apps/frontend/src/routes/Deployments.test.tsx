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
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
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
    get: vi.fn(async () => ({ jobs: [] })),
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
  n_batch: 512,
  max_new_tokens: 512,
  max_sessions: 1,
  prefill_chunk_tokens: 512,
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
  opts: { serving: boolean; isDefault: boolean; kind?: "shards" | "whole" },
): DeploymentRow {
  return {
    deployment: {
      deployment_version: 1,
      deploy_id: id,
      kind: opts.kind ?? "shards",
      model: { label: "qwen3:8b", total_layers: 32 },
      created_at: "2026-08-19T00:00:00Z",
      created_by: "coordenador",
      activated_at: "2026-08-19T00:00:00Z",
      nodes: [
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
      ],
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

  it("reimplanta mandando o parâmetro mexido e preservando o resto", async () => {
    listDeployments.mockResolvedValue({
      node_id: "eu",
      deployments: [row("d-aaaaaaaa", { serving: true, isDefault: true })],
    });
    redeployDeployment.mockResolvedValue({ job_id: "j1", deploy_id: "d-aaaaaaaa" });

    mount();
    await waitFor(() => expect(screen.getByText("d-aaaaaaaa")).toBeTruthy());

    click(screen.getByText("Reimplantar"));

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
    expect(screen.queryByText("Reimplantar")).toBeNull();
  });
});
