// @vitest-environment jsdom
/**
 * A rede vazia na visao estendida mostra uma unica mensagem.
 *
 * Antes do fix, com `c.peers.length === 0` a visao estendida desenhava duas
 * mensagens empilhadas: o painel `dashboard.noMachines` dentro da area de
 * rolagem e o overlay `dashboard.emptyNetwork` ancorado ao fundo. O overlay
 * saiu, e este teste confirma que so uma aparece.
 */
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { Dashboard } from "./Dashboard";
import { LocaleProvider } from "../i18n/provider";
import { fakeCluster, fakeNode, fakePeer, fakeSlicePeer } from "../test/fixtures";
import { keys } from "../lib/queries";
import type { DeploymentRow } from "../lib/api";
import { ProfileProvider } from "../lib/profile";
import type { NodeInfo, RuntimeParams } from "../lib/types";
import type { PipelineRouteCost } from "../components/PipelineGatewayNode";

const deploymentParams: RuntimeParams = {
  inference_mode: "auto",
  n_gpu_layers: 99,
  n_ctx: 8192,
  offload_kqv: true,
  fit_memory: true,
  fit_memory_margin_mb: 256,
  n_batch: 512,
  max_new_tokens: 256,
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
  unload_if_idle: false,
  idle_minutes: 60,
};

function deploymentRow(
  deployId: string,
  options: { isDefault: boolean; activeVersion: number; versions?: number[] },
): DeploymentRow {
  const nodes = [{
    node_id: "a",
    hostname: "a",
    start_layer: 0,
    end_layer: 31,
    peer_index: 0,
    peer_count: 1,
    primary: true,
    config: { ...deploymentParams },
  }];
  const versions = (options.versions ?? [1]).map((version) => ({
    version,
    created_at: "2026-09-11T15:00:00Z",
    created_by: "a",
    params: { ...deploymentParams },
    nodes,
  }));
  return {
    deployment: {
      deployment_version: 2,
      deploy_id: deployId,
      kind: "whole",
      model: { label: "qwen35", total_layers: 32 },
      created_at: "2026-09-11T15:00:00Z",
      created_by: "a",
      activated_at: "2026-09-11T15:00:00Z",
      nodes,
      versions,
      active_version: options.activeVersion,
    },
    serving: true,
    is_default: options.isDefault,
    holders: ["a"],
  };
}

function mount(
  cluster = fakeCluster([], "qwen35"),
  profile: "admin" | "node" | "infer" = "admin",
  rttMeasurements?: { from_node_id: string; to_node_id: string; rtt_ms: number; transport: string }[],
  nodeOverride?: Partial<NodeInfo> | null,
  deployments?: { node_id: string; deployments: DeploymentRow[] },
  pipelineRoutes?: PipelineRouteCost[],
) {
  // active_model is set, but there are no peers: the estendida view shows
  // "Nenhuma maquina na rede" in its panel.
  if (cluster.peers.length === 0) {
    cluster.total_layers = 0;
    cluster.covered_layers = 0;
    cluster.complete = false;
  }
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity, refetchInterval: false } },
  });
  // useQuery will read from the cache immediately.
  client.setQueryData(keys.cluster, cluster);
  if (rttMeasurements || pipelineRoutes) {
    client.setQueryData(keys.networkRtt(rttMeasurements ? ["a", "b"] : []), {
      measurements: rttMeasurements ?? [],
      pipeline_routes: pipelineRoutes?.map((route) => ({
        node_ids: route.nodeIds,
        legs_ms: route.legsMs,
        rtt_ms: route.rttMs ?? null,
        network_ms: route.networkMs ?? null,
        total_ms: route.totalMs ?? null,
        return_mode: route.returnMode ?? null,
      })),
    });
  }
  if (deployments) client.setQueryData(keys.deployments, deployments);
  const defaultNode = {
    node_id: "no-0", short_id: "no-0", hostname: "no-0",
    role: "peer" as const, profile, connection_key: "",
    p2p_port: 42777, gateway_port: 8080, inference_port: 8181,
    config: null,
    status: { state: "disabled" as const, detail: "", pid: 0, restarts: 0,
      log_path: null, reported_total_layers: 0, changed_at: "" },
    system: { hostname: "no-0", os: "linux", arch: "x86_64", cpu_count: 4, load_avg: [], ram: { total_mb: 16384, used_mb: 4096, available_mb: 12288, used_pct: 25 }, gpu_kind: "none", gpu_note: null, gpus: [], gpu_processes: [], wired_mb: 0, disk: { total_mb: 100000, free_mb: 50000, mount_point: "/" }, sampled_at: "2026-08-15T00:00:00Z" }, peer_count: 0, load: 0,
    active_model: "", build_commit: "abc1234", dev_update: false,
  } satisfies Partial<NodeInfo>;
  if (nodeOverride !== null) {
    client.setQueryData(keys.node, { ...defaultNode, ...nodeOverride });
  }
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <ProfileProvider initialProfile={profile}>
          <Dashboard />
        </ProfileProvider>
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

describe("Dashboard", () => {
  afterEach(cleanup);

  it("renderiza uma unica mensagem com a rede vazia na visao estendida", async () => {
    mount();

    await waitFor(() => {
      expect(screen.queryByText("Fluxo")).toBeTruthy();
    });

    act(() => {
      fireEvent.click(screen.getByText("Detalhes"));
    });

    // The estendida view panel shows the empty-state message. The
    // hidden PipelineRail also renders this text by coincidence
    // (pipeline.empty shares the same string in pt-BR), so getAllByText
    // finds more than one: we only care that the overlay that used to
    // stack on top is gone.
    await waitFor(() => {
      const visible = screen.getAllByText(/Nenhuma m.quina na rede/i)
        .filter((el) => el.closest(".hidden") == null);
      expect(visible.length).toBe(1);
    });

    // The absolute overlay that used to render alongside the panel is gone.
    expect(screen.queryByText("A rede esta vazia")).toBeNull();
  });

  it("sem modelo ativo oculta Adicionar e abre Detalhes por padrão", async () => {
    mount(fakeCluster([], ""));

    const details = await screen.findByRole("button", { name: /Detalhes/ });
    const flow = screen.getByRole("button", { name: /Fluxo/ });
    expect(details.getAttribute("aria-pressed")).toBe("true");
    expect(flow.getAttribute("aria-pressed")).toBe("false");
    expect(screen.queryByRole("button", { name: "Adicionar" })).toBeNull();
    expect(screen.getAllByText(/Nenhuma m.quina na rede/i).some((el) => el.closest(".hidden") == null)).toBe(true);
  });

  it("mostra no fluxo do Painel o RTT das arestas da implantação", async () => {
    const cluster = fakeCluster([
      fakeSlicePeer("a", 0, 15),
      fakeSlicePeer("b", 16, 31),
    ], "qwen35");
    cluster.routing_edges = [{
      from_node_id: "a",
      to_node_id: "b",
      model_id: "qwen35",
      from_end_layer: 15,
      to_start_layer: 16,
    }];

    mount(cluster, "admin", [{
      from_node_id: "a",
      to_node_id: "b",
      rtt_ms: 7.4,
      transport: "libp2p",
    }]);

    await waitFor(() => expect(screen.getByText("RTT 7.4 ms")).toBeTruthy());
  });

  it("mostra o custo da rota de um modelo inteiro com um unico head", async () => {
    const cluster = fakeCluster([fakeSlicePeer("head", 0, 31)], "qwen35");

    mount(cluster, "admin", undefined, undefined, undefined, [
      // Resposta direta: meio RTT da ida (5) e meio da volta (3).
      { nodeIds: ["head", "gateway"], legsMs: [5, 3], computeMs: null, networkMs: 4, returnMode: "direct" },
    ]);

    await waitFor(() => {
      const gateway = screen.getByRole("heading", { name: "Gateway" }).closest("article") as HTMLElement;
      expect(gateway.textContent).not.toContain("não medido");
      expect(gateway.textContent).toMatch(/head.*4\.0 ms/);
    });
  });

  /**
   * A cobertura sai do que os pares anunciam no gossip, não da leitura de cada
   * um: uma máquina cujo endereço anunciado morreu continua contribuindo
   * camadas. Mostrar "completo" em verde nesse estado é o pior que a tela pode
   * fazer - o operador acredita numa pipeline fechada que ninguém consegue
   * consultar.
   */
  it("não dá cobertura por confirmada quando uma máquina dela não pôde ser lida", async () => {
    const cluster = fakeCluster([
      fakeSlicePeer("a", 0, 15),
      { ...fakeSlicePeer("b", 16, 31), rediscovering: true },
    ], "qwen35");
    cluster.model_inventory = [{
      model_id: "qwen35",
      total_layers: 32,
      covered_layers: 32,
      coverage: [],
      complete: true,
      unreadable_nodes: ["b"],
    }];

    mount(cluster);

    await waitFor(() =>
      expect(screen.getByText("1 máquina desta cobertura não pôde ser lida")).toBeTruthy(),
    );
  });

  it("esconde adicionar e desimplantar para um nó comum", async () => {
    const cluster = fakeCluster([
      fakeSlicePeer("a", 0, 15),
      fakeSlicePeer("b", 16, 31),
    ], "qwen35");

    mount(cluster, "node");

    // O seletor de vista não existe neste perfil: o Fluxo é a única vista.
    await waitFor(() => expect(screen.getByLabelText("Modelo")).toBeTruthy());
    expect(screen.queryByRole("button", { name: "Adicionar" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Desimplantar" })).toBeNull();
  });

  /**
   * O Painel tem duas vistas do mesmo cluster, e os Detalhes são operação do
   * cluster: os cards trazem o estado interno de cada máquina e os botões de
   * liberar fatia, remover do pipeline e tirar da rede. Fora do `admin` resta
   * o Fluxo, e onde há uma vista só não há o que escolher.
   */
  it("nega os Detalhes ao perfil node e tira o seletor de vista da tela", async () => {
    const cluster = fakeCluster([
      fakeSlicePeer("a", 0, 15),
      fakeSlicePeer("b", 16, 31),
    ], "qwen35");

    mount(cluster, "node");

    await waitFor(() => expect(screen.getByLabelText("Modelo")).toBeTruthy());
    expect(screen.queryByRole("button", { name: /Detalhes/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /Fluxo/ })).toBeNull();
  });

  it("mantém as duas vistas e o seletor para o administrador", async () => {
    const cluster = fakeCluster([
      fakeSlicePeer("a", 0, 15),
      fakeSlicePeer("b", 16, 31),
    ], "qwen35");

    mount(cluster, "admin");

    await waitFor(() => expect(screen.getByRole("button", { name: /Fluxo/ })).toBeTruthy());
    expect(screen.getByRole("button", { name: /Fluxo/ }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: /Detalhes/ })).toBeTruthy();
  });

  it("mostra adicionar e desimplantar para o administrador", async () => {
    const cluster = fakeCluster([
      fakeSlicePeer("a", 0, 15),
      fakeSlicePeer("b", 16, 31),
    ], "qwen35");

    mount(cluster, "admin");

    await waitFor(() => {
      expect(screen.getByRole("button", { name: "Adicionar" })).toBeTruthy();
      expect(screen.getByRole("button", { name: "Desimplantar" })).toBeTruthy();
    });
  });

  it("usa a implantação padrão para manter as propriedades no mapa", async () => {
    const stale = deploymentRow("d-stale", {
      isDefault: false,
      activeVersion: 2,
      versions: [1],
    });
    const current = deploymentRow("d-current", {
      isDefault: true,
      activeVersion: 1,
    });

    mount(
      fakeCluster([fakeSlicePeer("a", 0, 31)], "qwen35"),
      "admin",
      undefined,
      undefined,
      { node_id: "a", deployments: [stale, current] },
    );

    await waitFor(() => {
      expect(screen.getAllByRole("button", { name: "Ocultar propriedades da implantação" })
        .some((button) => button.className.includes("w-8"))).toBe(true);
    });
    expect(screen.getByText(/d-curren/)).toBeTruthy();
  });

  it.each(["node", "infer"] as const)(
    "mostra a fatia local e não transforma a cobertura do cluster em alarme no perfil %s",
    async (profile) => {
      const cluster = fakeCluster([
        fakeSlicePeer("self", 4, 11, {
          is_self: true,
          node: fakeNode({ node_id: "self", short_id: "self", profile }),
        }),
      ]);
      cluster.active_model = "";
      cluster.total_layers = 0;
      cluster.covered_layers = 0;
      cluster.complete = false;
      cluster.model_inventory = [];

      mount(cluster, profile, undefined, {
        ...fakeNode({ node_id: "self", short_id: "self", profile, load: 0 }),
        config: {
          ...fakeNode().config,
          model_path: "/models/qwen.gguf",
          model_label: "qwen35",
          deploy_id: "deploy-qwen",
          deployment_name: "qwen35",
          start_layer: 4,
          end_layer: 11,
          total_layers: 32,
        } as NodeInfo["config"],
      });

      await waitFor(() => expect(screen.getByText("Fatia deste nó")).toBeTruthy());
      expect(screen.getAllByText("L4–11").length).toBeGreaterThan(0);
      expect(screen.getAllByText("qwen35").length).toBeGreaterThan(0);
      expect(screen.queryByText("pipeline incompleto")).toBeNull();
      expect(screen.queryByText("nenhum modelo atribuído")).toBeNull();
    },
  );

  it("diz que os dados locais são desconhecidos quando o perfil não consegue lê-los", async () => {
    const cluster = fakeCluster([
      fakePeer({ node_id: "self", is_self: true, load: null }),
    ], "");
    cluster.total_layers = 0;
    cluster.covered_layers = 0;
    cluster.complete = false;

    mount(cluster, "infer", undefined, null);

    await waitFor(() => expect(screen.getAllByText("Os dados ao vivo deste nó estão indisponíveis").length).toBeGreaterThan(0));
    expect(screen.queryByText("pipeline incompleto")).toBeNull();
    expect(screen.queryByText("nenhum modelo atribuído")).toBeNull();
  });
});
