// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { CARD_HEIGHT, CARD_WIDTH, routeGeometry, type CanvasPosition } from "../lib/networkLayout";
import type { DeployMachine, DeployPlan, DeploySelection, DeployStep } from "../lib/types";
import { DeployNetworkCanvas } from "./DeployNetworkCanvas";
import type { PipelineCycleCost } from "./PipelineGatewayNode";

const params = {
  n_gpu_layers: 0,
  n_ctx: 4096,
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
  no_thinking: false,
  no_embeddings: false,
  speculative_decoding_enabled: false,
  speculative_k: 4,
  speculative_lookup_window: 4,
  mtp_enabled: false,
  mtp_draft_tokens: 3,
  mtp_min_probability: 0.75,
};

function machine(node_id: string): DeployMachine {
  return {
    node_id,
    label: node_id,
    mem_free_mb: 100_000,
    gpu_free_mb: 0,
    ram_free_mb: 100_000,
    gpu_used_pct: null,
    ram_used_pct: 10,
    disk_free_mb: 100_000,
    rtt_ms: null,
    load: 0,
    holds: [[0, 3], [4, 7], [8, 11]],
    taking: null,
  };
}

function step(start_layer: number, end_layer: number, target_node_id: string, position: number): DeployStep {
  return {
    position,
    start_layer,
    end_layer,
    total_layers: 12,
    name: `qwen L${start_layer}-${end_layer}`,
    size_bytes: 1_000_000,
    target_node_id,
    target_host: target_node_id,
    ready: true,
    source_node_id: null,
    source_host: null,
    source_path: null,
    source_sha256: null,
    target_path: "/tmp/qwen",
    primary: start_layer === 0,
    n_gpu_layers: null,
    offload_kqv: null,
    options: [],
    problem: null,
    note: null,
    merged: false,
    parts: [],
  };
}

function candidate(start_layer: number, end_layer: number) {
  return {
    start_layer,
    end_layer,
    total_layers: 12,
    name: `qwen L${start_layer}-${end_layer}`,
    size_bytes: 1_000_000,
    holders: ["a", "b", "c"],
    selected: true,
  };
}

function plan(steps: DeployStep[], route_options: DeployPlan["route_options"]): DeployPlan {
  return {
    group: "qwen",
    deployment_name: "qwen",
    strategy: "simple",
    kind: "shards",
    total_layers: 12,
    total_layers_inferred: false,
    steps,
    complete: true,
    transfer_bytes: 0,
    blocked: null,
    auto: true,
    merge: false,
    available: [candidate(0, 3), candidate(4, 7), candidate(8, 11)],
    machines: [machine("a"), machine("b"), machine("c")],
    missing: [],
    overlap: [],
    warnings: [],
    params,
    prompt_tokens: 3840,
    chunk_tokens: 512,
    route_options,
    load_distribution: [],
  };
}

function renderCanvas(current: DeployPlan, rows: DeploySelection[], pipelineCost?: PipelineCycleCost) {
  return render(
    <LocaleProvider initialLocale="pt-BR">
      <DeployNetworkCanvas
        plan={current}
        rows={rows}
        inspectedNodeId="a"
        onInspect={vi.fn()}
        onAddPeer={vi.fn()}
        onRemovePeer={vi.fn()}
        onDropPeer={vi.fn()}
        labelOf={(nodeId) => nodeId}
        pipelineCost={pipelineCost}
      />
    </LocaleProvider>,
  );
}

describe("DeployNetworkCanvas", () => {
  afterEach(() => {
    cleanup();
    localStorage.clear();
  });

  it("draws the forward hops and the return from the last peer to the gateway", async () => {
    const steps = [
      step(0, 3, "a", 1),
      step(4, 7, "a", 2),
      step(8, 11, "b", 3),
    ];
    const current = plan(steps, [
      { from_node_id: "a", to_node_id: "b", rtt_ms: 7.4, selected: true, note: "rota escolhida" },
      { from_node_id: "b", to_node_id: "a", rtt_ms: 74.0, selected: false, note: "rota reversa" },
    ]);
    const view = renderCanvas(current, steps.map(({ start_layer, end_layer, target_node_id }) => ({ start_layer, end_layer, target_node_id })));
    const canvas = screen.getByText("Mapa da implantação").closest("section");
    expect(canvas).not.toBeNull();

    await waitFor(() => {
      expect(canvas!.querySelectorAll('svg[aria-hidden="true"] path[marker-end]')).toHaveLength(2);
    });
    expect(within(canvas!).getByRole("heading", { name: "Gateway" })).toBeTruthy();
    expect(within(canvas!).getByText("RTT 7.4 ms")).toBeTruthy();
    expect(within(canvas!).getByText("RTT -")).toBeTruthy();
    expect(within(canvas!).getByText("a → b · 7.4 ms")).toBeTruthy();
    expect(within(canvas!).queryByText("b → a · 74.0 ms")).toBeNull();
    view.unmount();
  });

  it("does not connect stages across a range with no machine", async () => {
    const steps = [
      step(0, 3, "a", 1),
      step(4, 7, "", 2),
      step(8, 11, "b", 3),
    ];
    const current = {
      ...plan(steps, []),
      complete: false,
      blocked: "as camadas L4-7 não estão em nenhuma fatia escolhida",
    };
    const view = renderCanvas(current, steps.map(({ start_layer, end_layer, target_node_id }) => ({ start_layer, end_layer, target_node_id })));
    const canvas = screen.getByText("Mapa da implantação").closest("section");
    expect(canvas).not.toBeNull();

    await waitFor(() => {
      // Even with a hole in the middle, the last peer still has the return leg
      // to the virtual gateway. No forward leg is invented.
      expect(canvas!.querySelectorAll('svg[aria-hidden="true"] path[marker-end]')).toHaveLength(1);
    });
    expect(within(canvas!).getByRole("heading", { name: "Gateway" })).toBeTruthy();
    view.unmount();
  });

  it("moves a card to its pipeline column once it gets a range, without Reset", async () => {
    const draw = (rows: DeploySelection[]) => (
      <LocaleProvider initialLocale="pt-BR">
        <DeployNetworkCanvas
          plan={plan(rows.map((row, index) => step(row.start_layer, row.end_layer, row.target_node_id, index + 1)), [])}
          rows={rows}
          inspectedNodeId="a"
          onInspect={vi.fn()}
          onAddPeer={vi.fn()}
          onRemovePeer={vi.fn()}
          onDropPeer={vi.fn()}
          labelOf={(nodeId) => nodeId}
          mapNodeIds={["a", "b"]}
        />
      </LocaleProvider>
    );
    const cardOf = (nodeId: string) => screen.getByRole("heading", { name: new RegExp(`^${nodeId}\\b`) }).closest("article") as HTMLElement;
    // b is on the map without a range: it waits on the row under the pipeline
    const view = render(draw([{ start_layer: 0, end_layer: 11, target_node_id: "a" }]));
    await waitFor(() => expect(cardOf("b").style.top).not.toBe(cardOf("a").style.top));

    view.rerender(draw([
      { start_layer: 0, end_layer: 3, target_node_id: "a" },
      { start_layer: 4, end_layer: 11, target_node_id: "b" },
    ]));
    // with a range it joins the pipeline on the same axis, after a
    await waitFor(() => expect(cardOf("b").style.top).toBe(cardOf("a").style.top));
    expect(parseFloat(cardOf("b").style.left)).toBeGreaterThan(parseFloat(cardOf("a").style.left));
    view.unmount();
  });

  it("marks a machine with Shift+click instead of inspecting it", () => {
    const onInspect = vi.fn();
    const onToggleMarked = vi.fn();
    const rows = [
      { start_layer: 0, end_layer: 3, target_node_id: "a" },
      { start_layer: 4, end_layer: 11, target_node_id: "b" },
    ];
    render(
      <LocaleProvider initialLocale="pt-BR">
        <DeployNetworkCanvas
          plan={plan([step(0, 3, "a", 1), step(4, 11, "b", 2)], [])}
          rows={rows}
          inspectedNodeId={null}
          onInspect={onInspect}
          onAddPeer={vi.fn()}
          onRemovePeer={vi.fn()}
          onDropPeer={vi.fn()}
          labelOf={(nodeId) => nodeId}
          mapNodeIds={["a", "b"]}
          markedNodeIds={[]}
          onToggleMarked={onToggleMarked}
        />
      </LocaleProvider>,
    );
    const cardB = screen.getByRole("heading", { name: /^b/ }).closest("article")!;
    fireEvent.click(cardB, { shiftKey: true });
    expect(onToggleMarked).toHaveBeenCalledWith("b");
    expect(onInspect).not.toHaveBeenCalled();
    fireEvent.click(cardB);
    expect(onInspect).toHaveBeenCalledWith("b");
  });

  it("stacks two heads and leaves the pipeline from the head to the machine that continues it", async () => {
    const rows = [
      { start_layer: 0, end_layer: 11, target_node_id: "a" },
      { start_layer: 0, end_layer: 3, target_node_id: "b" },
      { start_layer: 4, end_layer: 11, target_node_id: "c" },
    ];
    renderCanvas(
      plan(rows.map((row, index) => step(row.start_layer, row.end_layer, row.target_node_id, index + 1)), []),
      rows,
    );
    const cardOf = (nodeId: string) => screen.getByRole("heading", { name: new RegExp(`^${nodeId}`) }).closest("article") as HTMLElement;
    // the two heads share the first column, one under the other
    await waitFor(() => expect(cardOf("a").style.left).toBe(cardOf("b").style.left));
    expect(parseFloat(cardOf("b").style.top)).not.toBe(parseFloat(cardOf("a").style.top));
    expect(parseFloat(cardOf("c").style.left)).toBeGreaterThan(parseFloat(cardOf("b").style.left));
    // one hop (b -> c, layer 4 follows layer 3) and the answer from both machines on the last layer
    const canvas = screen.getByText("Mapa da implantação").closest("section")!;
    expect(canvas.querySelectorAll('svg[aria-hidden="true"] path[marker-end]')).toHaveLength(3);
    expect(within(canvas).getByText("a | b")).toBeTruthy();
  });

  it("keeps the toolbar, the legend and the route chips out of the area that scrolls", () => {
    const steps = [step(0, 3, "a", 1), step(4, 11, "b", 2)];
    renderCanvas(
      plan(steps, [{ from_node_id: "a", to_node_id: "b", rtt_ms: 3.2, selected: true, note: "" }]),
      steps.map(({ start_layer, end_layer, target_node_id }) => ({ start_layer, end_layer, target_node_id })),
    );
    const scroll = screen.getByTestId("deploy-map-scroll");
    expect(scroll.contains(screen.getByText("Mapa da implantação"))).toBe(false);
    expect(scroll.contains(screen.getByText("a → b · 3.2 ms"))).toBe(false);
    expect(scroll.contains(screen.getByText("resposta"))).toBe(false);
    // the cards scroll with the map
    expect(scroll.contains(screen.getByRole("heading", { name: /^a/ }))).toBe(true);
  });

  it("shows no in-flight load on a deploy card: it says nothing about the deployment", () => {
    const current = plan([step(0, 3, "a", 1)], []);
    renderCanvas(current, [{ start_layer: 0, end_layer: 3, target_node_id: "a" }]);
    const card = screen.getByRole("heading", { name: /^a/ }).closest("article")!;
    expect(within(card as HTMLElement).queryByText(/em voo/)).toBeNull();
    // the range sits in the header, next to the X
    expect(within(card as HTMLElement).getByText("L0–3")).toBeTruthy();
  });

  it("anchors the curve on the correct side when the receiver is on the left", () => {
    const from: CanvasPosition = { left: 600, top: 100 };
    const to: CanvasPosition = { left: 100, top: 100 };
    const geometry = routeGeometry(from, to);
    expect(geometry.x1).toBe(from.left);
    expect(geometry.y1).toBe(100 + CARD_HEIGHT / 2);
    expect(geometry.x2).toBe(100 + CARD_WIDTH);
    expect(geometry.y2).toBe(100 + CARD_HEIGHT / 2);
    expect(geometry.c1x).toBeLessThan(geometry.x1);
    expect(geometry.c2x).toBeGreaterThan(geometry.x2);
  });

  it("shows the total cost measured at the virtual gateway", () => {
    const steps = [
      step(0, 3, "a", 1),
      step(4, 7, "b", 2),
      step(8, 11, "c", 3),
    ];
    const current = plan(steps, []);
    const view = renderCanvas(
      current,
      steps.map(({ start_layer, end_layer, target_node_id }) => ({ start_layer, end_layer, target_node_id })),
      { legsMs: [18, 30, 31, 29], computeMs: 42 },
    );

    expect(screen.getByTestId("pipeline-total").textContent).toBe("150.0 ms");
    expect(screen.getByText("rede 108.0 ms · compute 42.0 ms")).toBeTruthy();
    view.unmount();
  });
});
