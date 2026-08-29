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
import { fakeCluster, fakeSlicePeer } from "../test/fixtures";
import { keys } from "../lib/queries";

function mount(cluster = fakeCluster([], "qwen35"), rttMeasurements?: { from_node_id: string; to_node_id: string; rtt_ms: number; transport: string }[]) {
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
  if (rttMeasurements) {
    client.setQueryData(keys.networkRtt(["a", "b"]), { measurements: rttMeasurements });
  }
  client.setQueryData(keys.node, {
    node_id: "no-0", short_id: "no-0", hostname: "no-0",
    role: "peer" as const, connection_key: "",
    p2p_port: 42777, gateway_port: 8080, inference_port: 8181,
    config: null,
    status: { state: "disabled" as const, detail: "", pid: 0, restarts: 0,
      log_path: null, reported_total_layers: 0, changed_at: "" },
    system: { hostname: "no-0", os: "linux", arch: "x86_64", cpu_count: 4, load_avg: [], ram: { total_mb: 16384, used_mb: 4096, available_mb: 12288, used_pct: 25 }, gpu_kind: "none", gpu_note: null, gpus: [], gpu_processes: [], wired_mb: 0, disk: { total_mb: 100000, free_mb: 50000, mount_point: "/" }, sampled_at: "2026-08-15T00:00:00Z" }, peer_count: 0, load: 0,
    active_model: "", build_commit: "abc1234", dev_update: false,
  });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
        <Dashboard />
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

    mount(cluster, [{
      from_node_id: "a",
      to_node_id: "b",
      rtt_ms: 7.4,
      transport: "libp2p",
    }]);

    await waitFor(() => expect(screen.getByText("RTT 7.4 ms")).toBeTruthy());
  });
});
