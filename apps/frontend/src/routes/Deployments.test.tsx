// @vitest-environment jsdom
/**
 * What this screen promises: to show **all** the deployments of the cluster,
 * without confusing "in service" with "default", and to let a parameter be
 * changed without redoing the whole deployment.
 *
 * The case that matters most is the one that was inexpressible before the
 * `deploy_id`: the same model in service twice. If the screen collapses the two
 * into one row, or marks both as default, the operator redeploys the wrong one.
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
  // `useJobs` goes through here; without a real response react-query
  // complains about `undefined` and pollutes the test output.
  api: {
    get: vi.fn(async (url: string) =>
      url === "/api/ui/node" ? { connection_key: "", pom_dev_mode: true } : { jobs: [] },
    ),
    post: vi.fn(async () => ({})),
    del: vi.fn(async () => ({})),
  },
  getConnectionKey: () => "k",
  setConnectionKey: vi.fn(),
  ApiError: class extends Error {},
}));

const params: RuntimeParams = {
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
  it("shows the two deployments of the same model, with distinct states", async () => {
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

    // One is in service without being the default; the other is the default without
    // being in service. If the screen treated the two words as synonyms, one of these would vanish.
    expect(screen.getByText("No ar")).toBeTruthy();
    expect(screen.getByText("Fora do ar")).toBeTruthy();
    expect(screen.getAllByText("Padrão")).toHaveLength(1);

    // And the form of each one shows, which is what tells the two apart in practice.
    expect(screen.getByText("Fatias")).toBeTruthy();
    expect(screen.getByText("Modelo inteiro")).toBeTruthy();
  });

  it("organizes by the last version activation, not by creation", async () => {
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
    // The first column is the expand control; the `deploy_id` is the second.
    const cells = Array.from(table.querySelectorAll("tbody tr")).map((tr) => tr.cells[1]);

    expect(cells.map((cell) => cell.textContent ?? "")).toEqual([
      expect.stringContaining("d-new"),
      expect.stringContaining("d-old"),
    ]);
    expect(table.querySelectorAll("thead th")[1]?.textContent).toBe("Id");
    expect(cells[0].className).toContain("px-5");
    expect(screen.getByText("Última ativação")).toBeTruthy();
  });

  it("redeploys sending the changed parameter and preserving the rest", async () => {
    listDeployments.mockResolvedValue({
      node_id: "eu",
      deployments: [row("d-aaaaaaaa", { serving: true, isDefault: true })],
    });
    redeployDeployment.mockResolvedValue({ job_id: "j1", deploy_id: "d-aaaaaaaa" });

    mount();
    await waitFor(() => expect(screen.getByText("d-aaaaaaaa")).toBeTruthy());

    click(screen.getByText("Reimplantar com novos parâmetros"));

    // The form opens filled with what the deployment is running.
    const nCtx = (await screen.findByRole("spinbutton", { name: /contexto|n_ctx/i })) as
      | HTMLInputElement
      | undefined;
    const field = nCtx ?? (document.querySelector('input[name="n_ctx"]') as HTMLInputElement);
    expect(field.value).toBe("8192");

    act(() => {
      fireEvent.change(field, { target: { value: "16384" } });
    });
    click(screen.getByText("Recarregar com estes valores"));

    await waitFor(() => expect(redeployDeployment).toHaveBeenCalled());
    const [id, body] = redeployDeployment.mock.calls[0] as [
      string,
      { params: RuntimeParams; per_node: Record<string, unknown> },
    ];

    // Same id: redeploying by changing a parameter does not create another deployment,
    // otherwise every client pinned to `modelo@d-aaaaaaaa` would stop finding it.
    expect(id).toBe("d-aaaaaaaa");
    expect(body.params.n_ctx).toBe(16384);
    // The rest goes along as it was: sending only the changed field would make the
    // node reload with the defaults in the other twenty.
    expect(body.params.temperature).toBe(0.7);
    expect(body.params.n_batch).toBe(512);
    expect(body.per_node["no-a"]).toBeTruthy();
  });

  it("only offers make default to whoever is not yet", async () => {
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

    const buttons = screen.getAllByText("Tornar padrão");
    expect(buttons).toHaveLength(1);

    click(buttons[0]);
    await waitFor(() => expect(activateDeployment).toHaveBeenCalledWith("d-bbbbbbbb"));
  });

  it("offers no action at all when there is no deployment", async () => {
    listDeployments.mockResolvedValue({ node_id: "eu", deployments: [] });
    mount();
    await waitFor(() => expect(screen.getByText("Nada implantado ainda")).toBeTruthy());
    expect(screen.queryByText("Reimplantar com novos parâmetros")).toBeNull();
  });

  it("shows the plugin engine and its settings, not the llama.cpp numbers", async () => {
    const strata = row("d-strata01", { serving: true, isDefault: true, kind: "whole" });
    for (const target of [...strata.deployment.versions!.map((v) => v.params), strata.deployment.nodes[0].config]) {
      Object.assign(target, { inference_service: "strata", engine_settings: { "sampling.top_k": 20 } });
    }
    listDeployments.mockResolvedValue({ node_id: "eu", deployments: [strata] });

    mount();
    await waitFor(() => expect(screen.getByText("d-strata01")).toBeTruthy());
    // Without the service announced in the cluster, the engine shows by the plugin's code.
    expect(screen.getByText("strata")).toBeTruthy();

    click(screen.getByRole("button", { name: "Ver versões" }));
    const summary = screen.getByText(/ctx 8192/);
    expect(summary.textContent).toBe("strata · ctx 8192 · sess 1 · sampling.top_k=20");
    expect(summary.textContent).not.toContain("batch");
  });

  it("expands a row and redeploys a version by number", async () => {
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

    // Closed, the versions do not show.
    expect(screen.queryByText("Reimplantar esta versão")).toBeNull();

    click(screen.getByRole("button", { name: "Ver versões" }));

    // Both versions, with the active one marked.
    expect(screen.getByText("v1")).toBeTruthy();
    expect(screen.getByText("v2")).toBeTruthy();
    expect(screen.getByText("Ativa")).toBeTruthy();

    const buttons = screen.getAllByText("Reimplantar esta versão");
    // v2 first (most recent); clicking v1 is the second.
    click(buttons[1]);

    await waitFor(() => expect(redeployDeployment).toHaveBeenCalledWith("d-aaaaaaaa", {
      version: 1,
    }));
  });

  it("edits and redeploys from a version, in the same inspector on the right", async () => {
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

    // The read panel gives way to the edit one, seeded with version 1.
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
   * The same inspector as the deploy screen, here read-only: it is what
   * answers "which parameters did this version come up with?" without forcing the
   * redeploy dialog open, which is an editable form and suggests change.
   */
  it("opens the properties of a version read-only", async () => {
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

    // Closed, the inspector does not exist.
    expect(screen.queryByText("Configurações padrões")).toBeNull();

    click(screen.getByRole("button", { name: "Ver as propriedades da versão 1" }));

    expect(screen.getByText("Propriedades da versão 1")).toBeTruthy();
    expect(screen.getByText("Configurações padrões")).toBeTruthy();
    expect(screen.getByText("Configurações POM")).toBeTruthy();
    expect(screen.getByText("Configurações POM (DESENV)")).toBeTruthy();

    // Truly read-only: the value is written out, not in a field.
    const inspector = screen.getByText("Configurações padrões").closest("aside");
    expect(inspector).toBeTruthy();
    expect(inspector!.querySelectorAll("input")).toHaveLength(0);
    expect(inspector!.querySelectorAll("select")).toHaveLength(0);
    expect(within(inspector!).getByText("8192")).toBeTruthy();
    // And the plan's switch stays readable, without being actionable.
    const noThinking = within(inspector!).getByRole("checkbox", {
      name: "Desativar thinking",
    }) as HTMLButtonElement;
    expect(noThinking.getAttribute("aria-checked")).toBe("true");
    expect(noThinking.disabled).toBe(true);

    click(screen.getByRole("button", { name: "Fechar as propriedades" }));
    expect(screen.queryByText("Configurações padrões")).toBeNull();
  });
});
