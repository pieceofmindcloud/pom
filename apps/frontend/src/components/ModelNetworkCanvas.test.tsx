// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { edgeStyle, isControlTarget, ModelNetworkCanvas } from "./ModelNetworkCanvas";
import type { PipelineCycleCost, PipelineRouteCost } from "./PipelineGatewayNode";
import { LocaleProvider } from "../i18n/provider";
import type { ChatTelemetryPeerPerf, ChatTelemetryRtt, ClusterView } from "../lib/types";
import { fakeCluster, fakeNode, fakePeer, fakeSlicePeer, fakeSystem } from "../test/fixtures";
import { CARD_HEIGHT, deploymentLayoutKey, writePositions } from "../lib/networkLayout";
import { SERVICE_TETHER_HEIGHT } from "./PluginServices";

describe("edgeStyle", () => {
  it("paints the main route green, solid and with an arrow", () => {
    const s = edgeStyle("principal");
    expect(s.className).toContain("stroke-aqua");
    expect(s.strokeDasharray).toBeUndefined();
    expect(s.markerEnd).toBeTruthy();
  });

  it("paints a parallel route blue, solid and without an arrow", () => {
    const s = edgeStyle("paralela");
    expect(s.className).toContain("stroke-iris");
    expect(s.strokeDasharray).toBeUndefined();
    expect(s.markerEnd).toBeUndefined();
  });

  it("paints an inactive route dashed gray", () => {
    const s = edgeStyle("inativa");
    expect(s.className).toContain("stroke-faint");
    expect(s.strokeDasharray).toBe("7 7");
  });
});

const cluster = fakeCluster([fakeSlicePeer("a", 0, 15), fakeSlicePeer("b", 16, 31)]);

function mount(
  view: ClusterView = cluster,
  rttMeasurements: ChatTelemetryRtt[] = [],
  pipelineCost?: PipelineCycleCost,
  properties?: { open: boolean; onToggle: () => void },
  pipelineRoutes?: PipelineRouteCost[],
  peerPerf?: Record<string, ChatTelemetryPeerPerf>,
) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <LocaleProvider initialLocale="pt-BR">
      <ModelNetworkCanvas
        cluster={view}
        rttMeasurements={rttMeasurements}
        pipelineCost={pipelineCost}
        pipelineRoutes={pipelineRoutes}
        peerPerf={peerPerf}
        propertiesOpen={properties?.open}
        onToggleProperties={properties?.onToggle}
      />
      </LocaleProvider>
    </QueryClientProvider>,
  );
}

describe("ModelNetworkCanvas: edge RTT", () => {
  afterEach(cleanup);

  it("shows the directional RTT in the same badge as the deployment map", () => {
    const view = fakeCluster([
      fakeSlicePeer("a", 0, 15),
      fakeSlicePeer("b", 16, 31),
    ]);
    view.routing_edges = [{
      from_node_id: "a",
      to_node_id: "b",
      model_id: "qwen35",
      from_end_layer: 15,
      to_start_layer: 16,
    }];

    mount(view, [{
      from_node_id: "a",
      to_node_id: "b",
      rtt_ms: 7.4,
      transport: "p2p",
    }]);

    expect(screen.getByText("RTT 7.4 ms")).toBeTruthy();
  });

  it("draws the return from the last peer to the gateway without reverse edges", () => {
    const view = fakeCluster([
      fakeSlicePeer("a", 0, 15),
      fakeSlicePeer("b", 16, 31),
      fakeSlicePeer("c", 32, 47),
    ]);
    view.routing_edges = [
      { from_node_id: "a", to_node_id: "b", model_id: "qwen35", from_end_layer: 15, to_start_layer: 16 },
      { from_node_id: "b", to_node_id: "c", model_id: "qwen35", from_end_layer: 31, to_start_layer: 32 },
      { from_node_id: "c", to_node_id: "b", model_id: "qwen35", from_end_layer: 47, to_start_layer: 16 },
    ];

    const { container } = mount(view, [
      { from_node_id: "a", to_node_id: "b", rtt_ms: 30, transport: "https" },
      { from_node_id: "b", to_node_id: "c", rtt_ms: 31, transport: "https" },
      { from_node_id: "c", to_node_id: "gateway", rtt_ms: 29, transport: "https" },
    ]);

    expect(screen.getByRole("heading", { name: "Gateway" })).toBeTruthy();
    expect(container.querySelectorAll('svg[aria-hidden="true"] path[marker-end]').length).toBe(3);
    expect(screen.getByText("RTT 29.0 ms")).toBeTruthy();
    expect(screen.queryByText("RTT 31.0 ms")).toBeTruthy();
    const gateway = screen.getByRole("heading", { name: "Gateway" }).closest("article") as HTMLElement;
    const lastPeer = Array.from(container.querySelectorAll("h3")).find((heading) => heading.textContent === "c")?.closest("article") as HTMLElement;
    expect(parseFloat(gateway.style.left)).toBeGreaterThan(parseFloat(lastPeer.style.left));
  });

  it("shows at the gateway the full cost of the legs and of the compute", () => {
    const view = fakeCluster([
      fakeSlicePeer("a", 0, 15),
      fakeSlicePeer("b", 16, 31),
      fakeSlicePeer("c", 32, 47),
    ]);
    const cost: PipelineCycleCost = {
      legsMs: [20, 30, 31, 29],
      computeMs: 40,
    };

    mount(view, [], cost);

    expect(screen.getByTestId("pipeline-total").textContent).toBe("150.0 ms");
    expect(screen.getByText("rede 110.0 ms · compute 40.0 ms")).toBeTruthy();
    // The legs one by one stay in the gateway's window.
    fireEvent.click(screen.getByTestId("pipeline-gateway-open"));
    expect(screen.getByTestId("pipeline-total-detail").textContent).toBe("Total do pipeline: 150.0 ms");
    expect(screen.getByTestId("pipeline-gateway-details").textContent).toContain("perna 4");
  });

  it("shows all head routes, sorted by the backend time per token", () => {
    const view = fakeCluster([
      fakeSlicePeer("head-fast", 0, 15),
      fakeSlicePeer("head-slow", 0, 15),
      fakeSlicePeer("tail", 16, 31),
    ]);
    view.routing_edges = [
      { from_node_id: "head-fast", to_node_id: "tail", model_id: "qwen35", from_end_layer: 15, to_start_layer: 16 },
      { from_node_id: "head-slow", to_node_id: "tail", model_id: "qwen35", from_end_layer: 15, to_start_layer: 16 },
    ];

    mount(view, [], undefined, undefined, [
      { nodeIds: ["head-slow", "tail", "gateway"], legsMs: [18, 12, 3], computeMs: null, networkMs: 16.5, returnMode: "direct" },
      { nodeIds: ["head-fast", "tail", "gateway"], legsMs: [4, 12, 3], computeMs: null, networkMs: 9.5, returnMode: "direct" },
    ]);

    const gateway = screen.getByRole("heading", { name: "Gateway" }).closest("article") as HTMLElement;
    expect(gateway.textContent).not.toContain("não medido");
    expect(gateway.textContent).toContain("2 rotas");
    expect(screen.getByTestId("pipeline-total").textContent).toBe("9.5 ms");
    // The whole list stays in the window, so the card does not grow with the replicas.
    fireEvent.click(screen.getByTestId("pipeline-gateway-open"));
    const routes = screen.getAllByTestId("pipeline-route").map((route) => route.textContent ?? "");
    // The sum of the legs (19 and 33 ms) would count whole round trips.
    expect(routes[0]).toMatch(/head-fast.*9\.5 ms/);
    expect(routes[0]).toContain("mais rápida");
    expect(routes[1]).toMatch(/head-slow.*16\.5 ms/);
    // Leg by leg, from the entry at the gateway to the return.
    expect(routes[1]).toMatch(/Gateway → head-slow18\.0 ms/);
    expect(routes[1]).toMatch(/tail → Gateway3\.0 ms/);
  });
});

describe("ModelNetworkCanvas: machine speed", () => {
  afterEach(cleanup);

  it("shows the decode and prefill TPS of each machine and the tok/s of the best route", () => {
    const view = fakeCluster([fakeSlicePeer("head", 0, 15), fakeSlicePeer("tail", 16, 31)]);
    mount(view, [], undefined, undefined, [
      { nodeIds: ["head", "tail", "gateway"], legsMs: [4, 12, 4], computeMs: 30, networkMs: 20, totalMs: 50, returnMode: "ring" },
    ], {
      head: { decode_tokens_per_sec: 66.7, prefill_tokens_per_sec: 1250, active_generations: 0 },
    });

    const cards = screen.getAllByTestId("machine-tps").map((element) => element.textContent);
    expect(cards).toContain("decode66.7tok/sprefill1250tok/s");
    expect(cards).toContain("velocidade não medida");
    // A single measured slice has nothing to compare with: neutral tone.
    expect(screen.getByTestId("speed-decode").getAttribute("data-tone")).toBe("solo");
    expect(screen.getByTestId("pipeline-throughput-badge").textContent).toBe("20.0 tok/s");
    fireEvent.click(screen.getByTestId("pipeline-gateway-open"));
    expect(screen.getByTestId("pipeline-total-detail").textContent).toBe("Total do pipeline: 50.0 ms");
    expect(screen.getByTestId("pipeline-throughput").textContent).toBe("≈ 20.0 tok/s por ciclo de token");
    expect(screen.getByTestId("pipeline-route-breakdown").textContent).toBe("rede 20.0 ms (anel) · compute 30.0 ms");
  });
});

describe("ModelNetworkCanvas: relayed route", () => {
  afterEach(cleanup);

  it("paints the relayed edge orange and explains the slowness at the gateway", () => {
    const view = fakeCluster([fakeSlicePeer("head", 0, 15), fakeSlicePeer("tail", 16, 31)]);
    view.routing_edges = [
      { from_node_id: "head", to_node_id: "tail", model_id: "qwen35", from_end_layer: 15, to_start_layer: 16 },
    ];
    const { container } = mount(view, [
      { from_node_id: "head", to_node_id: "tail", rtt_ms: 67, transport: "p2p-tunnel", relayed: true },
      { from_node_id: "tail", to_node_id: "gateway", rtt_ms: 3, transport: "quic" },
    ], undefined, undefined, [
      { nodeIds: ["head", "tail", "gateway"], legsMs: [4, 67, 3], computeMs: 30, networkMs: 37, totalMs: 67, returnMode: "direct", relayed: true, relayedLegs: [["head", "tail"]] },
    ]);

    const label = screen.getByTestId("route-relay-label");
    expect(label.textContent).toContain("RELAY");
    expect(label.textContent).toContain("RTT 67.0 ms");
    expect(label.getAttribute("title")).toContain("peer de relé");
    expect(container.querySelectorAll('path[data-relayed="true"]').length).toBe(1);
    expect(screen.getByTestId("pipeline-relay-warning").textContent).toContain("Rota por relé");
    // The long text only appears on the card, when clicking the icon.
    expect(screen.queryByTestId("pipeline-relay-popup")).toBeNull();
    fireEvent.click(screen.getByTestId("pipeline-relay-info"));
    expect(screen.getByTestId("pipeline-relay-legs").textContent).toBe("Pernas por relé: head → tail");
    expect(screen.getByTestId("pipeline-relay-popup").textContent).toContain("não consegue contornar");
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByTestId("pipeline-relay-popup")).toBeNull();
    // In the window, the marked route and the leg through a relay highlighted.
    fireEvent.click(screen.getByTestId("pipeline-gateway-open"));
    expect(screen.getByTestId("pipeline-route-relay").textContent).toBe("RELAY");
    expect(screen.getByTestId("pipeline-route").textContent).toMatch(/head → tail67\.0 ms/);
  });

  it("says when the ring bypasses the relay of the direct return", () => {
    const view = fakeCluster([fakeSlicePeer("gw", 0, 15), fakeSlicePeer("tail", 16, 31)]);
    mount(view, [], undefined, undefined, [
      { nodeIds: ["gw", "tail", "gw"], legsMs: [0, 30, 52], computeMs: 90, networkMs: 30, totalMs: 120, returnMode: "ring", relayBypassedByRing: true },
    ]);
    expect(screen.getByTestId("pipeline-relay-warning").textContent).toContain("Relé contornado pelo anel");
    fireEvent.click(screen.getByTestId("pipeline-relay-info"));
    expect(screen.getByTestId("pipeline-relay-bypass").textContent).toContain("tail → gw");
    expect(screen.getByTestId("pipeline-relay-popup").textContent).toContain("volta pelo anel");
  });

  it("does not alert when no leg goes through a relay", () => {
    const view = fakeCluster([fakeSlicePeer("head", 0, 15), fakeSlicePeer("tail", 16, 31)]);
    mount(view, [], undefined, undefined, [
      { nodeIds: ["head", "tail", "gateway"], legsMs: [4, 12, 3], computeMs: 30, networkMs: 9.5, totalMs: 39.5, returnMode: "direct" },
    ]);
    expect(screen.queryByTestId("pipeline-relay-warning")).toBeNull();
    expect(screen.queryByTestId("route-relay-label")).toBeNull();
  });
});

describe("ModelNetworkCanvas: deployment properties", () => {
  afterEach(cleanup);

  it("keeps the compact toggle inside the map", () => {
    const onToggle = vi.fn();
    const { container } = mount(cluster, [], undefined, { open: true, onToggle });
    const canvas = container.querySelector("section");
    const button = screen.getByRole("button", { name: "Ocultar propriedades da implantação" });

    expect(button.textContent).toBe("");
    expect(button.className).toContain("w-8");
    expect(button.closest("section")).toBe(canvas);
    fireEvent.click(button);
    expect(onToggle).toHaveBeenCalledTimes(1);
  });
});

/**
 * How many layers ended up on the CPU has to show on the card itself.
 *
 * Before, this only existed behind the double click: inference was slow
 * because part of the model did not fit in VRAM, and the card - which shows VRAM, RAM and
 * load - said nothing. The summary here does not replace the detail; it exists
 * so the operator does not need to open machine by machine to find out which
 * of them is running half on the CPU.
 */
describe("ModelNetworkCanvas: layers on the GPU and on the CPU", () => {
  afterEach(cleanup);

  function withGpuLayers(reported: number | null) {
    const peer = fakeSlicePeer("a", 0, 63);
    return fakeCluster([
      {
        ...peer,
        node: { ...peer.node!, status: { ...peer.node!.status, reported_gpu_layers: reported } },
      },
    ]);
  }

  it("shows the split and highlights what was left on the CPU", () => {
    // 64 layers served, 55 on the GPU: the case the captain reported.
    mount(withGpuLayers(55));
    expect(screen.getByText("55 GPU · 9 CPU")).toBeTruthy();
    // Amber is the highlight: it is the signal that matters at a glance.
    expect(screen.getByText("55 GPU · 9 CPU").className).toContain("text-amber");
    expect(screen.getByLabelText(/9 camadas não couberam na GPU/)).toBeTruthy();
  });

  it("raises no alarm when everything fit on the GPU", () => {
    mount(withGpuLayers(-1));
    const value = screen.getByText("64 na GPU");
    expect(value.className).not.toContain("text-amber");
    expect(screen.queryByLabelText(/não couberam na GPU/)).toBeNull();
  });

  it("stays quiet when the engine reported nothing, instead of guessing", () => {
    mount(withGpuLayers(null));
    expect(screen.queryByText(/GPU ·/)).toBeNull();
    expect(screen.queryByText(/na GPU$/)).toBeNull();
  });

  it("stays quiet on a machine without GPU, even with layers reported on the GPU", () => {
    // The captain's case: Windows with no card at all showing "25 on the GPU" - the
    // engine reported the request's sentinel, not a real hardware reading.
    const peer = fakeSlicePeer("windows-no-gpu", 0, 63);
    const cluster = fakeCluster([
      {
        ...peer,
        node: {
          ...peer.node!,
          status: { ...peer.node!.status, reported_gpu_layers: 25 },
          system: { ...peer.node!.system, gpu_kind: "none", gpus: [] },
        },
      },
    ]);
    mount(cluster);
    expect(screen.queryByText(/GPU ·/)).toBeNull();
    expect(screen.queryByText(/na GPU$/)).toBeNull();
  });
});

describe("ModelNetworkCanvas: temperature", () => {
  afterEach(cleanup);

  it("shows no temperature when gpu_kind says the machine has no GPU", () => {
    const peer = fakeSlicePeer("cpu-only", 0, 31, {
      node: fakeNode({
        node_id: "cpu-only",
        short_id: "cpu-only",
        system: {
          ...fakeNode().system,
          hostname: "cpu-only",
          gpu_kind: "none",
          gpus: [{
            index: 0,
            name: "stale device",
            vendor: "nvidia",
            vram_total_mb: null,
            vram_used_mb: null,
            vram_used_pct: null,
            utilization_pct: null,
            power_watts: null,
            temperature_c: null,
          }],
        },
      }),
    });

    mount(fakeCluster([peer]));

    expect(screen.queryByText(/temperatura da gpu/i)).toBeNull();
  });
});

/**
 * The Flow card and the Details tab show the same thing, and the way to it is
 * the double click. A single click remains selection and dragging.
 */
describe("ModelNetworkCanvas: details on double click", () => {
  afterEach(cleanup);

  it("does not include a peer without a slice in the otherRanges of the details window", () => {
    const b = fakeSlicePeer("b", 0, 0);
    b.total_layers = 0;
    const view = fakeCluster([fakeSlicePeer("a", 5, 10), b], "qwen35");
    mount(view);

    const cards = document.querySelectorAll("article");
    expect(cards.length).toBeGreaterThanOrEqual(1);

    act(() => {
      fireEvent.doubleClick(cards[1]);
    });

    // The dialog shows the serving peer's real range, not the [0,0] that
    // a non-serving peer would contribute to otherRanges without the filter.
    const dialog = screen.getByRole("dialog");
    expect(dialog.textContent).toMatch(/L5.+10/);
  });

  it("opens nothing before the double click", () => {
    mount();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("opens the machine window on a double click of the card", () => {
    mount();
    const card = document.querySelector("article")!;
    act(() => {
      fireEvent.doubleClick(card);
    });
    const dialog = screen.getByRole("dialog");
    expect(dialog.getAttribute("aria-label")).toContain("Detalhes de");
  });

  it("a single click does not open the window", () => {
    mount();
    const card = document.querySelector("article")!;
    act(() => {
      fireEvent.click(card);
    });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  // The window leaves with an animation, so "closed" is asserted by waiting for the exit - and
  // not on the tick right after the click.
  it("closes with the button and with Esc", async () => {
    mount();
    const card = document.querySelector("article")!;
    act(() => {
      fireEvent.doubleClick(card);
    });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Fechar" }));
    });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    act(() => {
      fireEvent.doubleClick(card);
    });
    expect(screen.getByRole("dialog")).toBeTruthy();
    act(() => {
      fireEvent.keyDown(document, { key: "Escape" });
    });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("shows the machine that was clicked, not the first of the list", () => {
    mount({
      ...cluster,
      peers: [
        fakeSlicePeer("a", 0, 15, { node: fakeNode({ node_id: "a", short_id: "a", hostname: "salem" }) }),
        fakeSlicePeer("b", 16, 31, { node: fakeNode({ node_id: "b", short_id: "b", hostname: "ksia" }) }),
      ],
    });
    const cards = document.querySelectorAll("article");
    act(() => {
      fireEvent.doubleClick(cards[1]);
    });
    expect(screen.getByRole("dialog").getAttribute("aria-label")).toContain("ksia");
  });
});

/**
 * The double click belongs to the card, not to the controls inside it: the Flow card
 * has no button today, and the gesture must not become a trap for the first one that comes.
 */
describe("isControlTarget", () => {
  afterEach(cleanup);

  it("recognizes a click that came from inside a button", () => {
    const host = document.createElement("div");
    host.innerHTML = "<article><button><span>Sair da rede</span></button></article>";
    document.body.append(host);
    expect(isControlTarget(host.querySelector("span"))).toBe(true);
    expect(isControlTarget(host.querySelector("article"))).toBe(false);
    host.remove();
  });

  it("does not treat a target without an element as a control", () => {
    expect(isControlTarget(null)).toBe(false);
  });
});

/**
 * A new deployment has nothing saved, and it is precisely the moment when the
 * screen needs to explain itself: layer order from left to
 * right, replicas stacked. The three-column grid that existed before
 * put three machines in a row and the fourth on the line below, with the edge between
 * the ends crossing the whole canvas.
 */
describe("ModelNetworkCanvas: the drawing of a new deployment", () => {
  afterEach(() => {
    cleanup();
    localStorage.clear();
  });

  const fourMachines = fakeCluster([
    fakeSlicePeer("macbook", 0, 18),
    fakeSlicePeer("salem", 19, 34),
    fakeSlicePeer("salem2", 19, 34),
    fakeSlicePeer("ksia", 35, 64),
  ]);

  function cardOf(container: HTMLElement, hostname: string) {
    const title = Array.from(container.querySelectorAll("h3")).find((h) => h.textContent === hostname);
    return title?.closest("article") as HTMLElement;
  }

  it("puts the ranges in columns in pipeline order, with nobody dragging anything", () => {
    const { container } = mount(fourMachines);
    const left = (name: string) => parseFloat(cardOf(container, name).style.left);

    expect(left("macbook")).toBeLessThan(left("salem"));
    expect(left("salem")).toBeLessThan(left("ksia"));
  });

  it("stacks the two replicas of L19-L34 in the same column", () => {
    const { container } = mount(fourMachines);
    const salem = cardOf(container, "salem");
    const salem2 = cardOf(container, "salem2");

    expect(salem.style.left).toBe(salem2.style.left);
    expect(salem.style.top).not.toBe(salem2.style.top);
  });

  it("leaves room under a replica for the service badges it hangs", () => {
    const withServices = fakeCluster([
      fakeSlicePeer("salem", 19, 34, {
        node: fakeNode({
          node_id: "salem", short_id: "salem", hostname: "salem",
          plugin_services: [{ plugin_code: "qdrant", id: "rag", kind: "rag", name: "Qdrant", status: "ready" }],
        }),
      }),
      fakeSlicePeer("salem2", 19, 34),
    ]);
    const { container } = mount(withServices);
    const top = (name: string) => parseFloat(cardOf(container, name).style.top);
    const [upper, lower] = top("salem") < top("salem2") ? ["salem", "salem2"] : ["salem2", "salem"];
    const gap = top(lower) - top(upper);
    // Card plus the tether (line + badge) when the upper one carries services.
    expect(gap).toBeGreaterThanOrEqual(CARD_HEIGHT + (upper === "salem" ? SERVICE_TETHER_HEIGHT : 0));
    // A llama.cpp machine gets the engine badge.
    expect(container.querySelector('[data-testid="plugin-service"][aria-label^="llama.cpp"]')).toBeTruthy();
  });

  /** The entry and the exit stay on the middle axis of the replicas: that is what makes the
   * fork look like a fork instead of a ladder. */
  it("centers whoever does not branch on the axis of whoever branches", () => {
    const { container } = mount(fourMachines);
    const top = (name: string) => parseFloat(cardOf(container, name).style.top);
    const replicaAxis = (top("salem") + top("salem2")) / 2;

    expect(Math.abs(top("macbook") - replicaAxis)).toBeLessThan(1);
    expect(Math.abs(top("ksia") - replicaAxis)).toBeLessThan(1);
  });
});

/**
 * The hand-made arrangement is a decision, and the button that erases decisions is the button
 * that nobody presses.
 *
 * "Organize" tidies up the rest and respects whoever was moved; whoever wants to discard
 * has the "Reset" next to it. Before this there was no choice: a node restart
 * changed the layout key and the arrangement vanished on its own, with nobody asking.
 */
describe("ModelNetworkCanvas: organize and reset", () => {
  afterEach(() => {
    cleanup();
    localStorage.clear();
  });

  const network = fakeCluster([
    fakeSlicePeer("macbook", 0, 18),
    fakeSlicePeer("salem", 19, 34),
    fakeSlicePeer("ksia", 35, 64),
  ]);

  function cardOf(container: HTMLElement, hostname: string) {
    const title = Array.from(container.querySelectorAll("h3")).find((h) => h.textContent === hostname);
    return title?.closest("article") as HTMLElement;
  }

  /**
   * Simulates what the drag writes. The card key is not the raw `node_id`: it is the
   * canvas's `keyFor`, which includes model and range.
   */
  const cardKey = (node_id: string, start: number, end: number) =>
    `${node_id}:qwen35:${start}:${end}`;

  function withDrag(positions: Record<string, { left: number; top: number }>) {
    const peers = [
      { node_id: cardKey("macbook", 0, 18), start_layer: 0, end_layer: 18 },
      { node_id: cardKey("salem", 19, 34), start_layer: 19, end_layer: 34 },
      { node_id: cardKey("ksia", 35, 64), start_layer: 35, end_layer: 64 },
    ];
    writePositions(deploymentLayoutKey("qwen35", peers), positions);
  }

  it("organize keeps what the operator placed, and tidies the rest", () => {
    withDrag({ [cardKey("ksia", 35, 64)]: { left: 900, top: 640 } });
    const { container } = mount(network);
    expect(cardOf(container, "ksia").style.top).toBe("640px");

    fireEvent.click(screen.getByLabelText(/Organizar pela ordem das camadas/i));

    expect(cardOf(container, "ksia").style.left).toBe("900px");
    expect(cardOf(container, "ksia").style.top).toBe("640px");
    // And the rest stays in the layer arrangement.
    const left = (name: string) => parseFloat(cardOf(container, name).style.left);
    expect(left("macbook")).toBeLessThan(left("salem"));
  });

  it("reset discards the drag and restores the arrangement by layers", () => {
    withDrag({ [cardKey("ksia", 35, 64)]: { left: 900, top: 640 } });
    const { container } = mount(network);
    expect(cardOf(container, "ksia").style.top).toBe("640px");

    fireEvent.click(screen.getByLabelText(/Redefinir o arranjo/i));

    expect(cardOf(container, "ksia").style.left).not.toBe("900px");
    const top = (name: string) => parseFloat(cardOf(container, name).style.top);
    expect(Math.abs(top("ksia") - top("macbook"))).toBeLessThan(1);
  });
});

/**
 * Two cards in the same box are not a detail of the big card: the miniature
 * card was where the second GPU vanished. The VRAM here is the sum - it is what the
 * machine has to offer - and the temperature gets one line per card, because
 * one may be boiling while the other sleeps.
 */
describe("ModelNetworkCanvas: machine with more than one GPU", () => {
  afterEach(cleanup);

  const twoGpus = fakeCluster([
    fakeSlicePeer("a", 0, 31, {
      node: fakeNode({
        node_id: "a",
        short_id: "a",
        hostname: "ksia",
        system: fakeSystem({
          gpu_kind: "nvidia",
          gpus: [
            { index: 0, name: "RTX 5090", vendor: "nvidia", vram_total_mb: 32768, vram_used_mb: 8192, vram_used_pct: 25, utilization_pct: 30, power_watts: null, temperature_c: 61 },
            { index: 1, name: "RTX 3090", vendor: "nvidia", vram_total_mb: 24576, vram_used_mb: 4096, vram_used_pct: 16, utilization_pct: 10, power_watts: null, temperature_c: 78 },
          ],
        }),
      }),
    }),
  ]);

  it("sums the VRAM of both cards instead of showing only the first", () => {
    mount(twoGpus);
    // (8192 + 4096) / 1024 = 12.0 GiB of (32768 + 24576) / 1024 = 56.0 GiB.
    expect(screen.getByText("12.0 / 56.0 GiB")).toBeTruthy();
    expect(screen.getByText("VRAM · 2 GPUs")).toBeTruthy();
  });

  it("shows the temperature of each card, by name", () => {
    mount(twoGpus);
    expect(screen.getByText(/61,0 °C/)).toBeTruthy();
    expect(screen.getByText(/78,0 °C/)).toBeTruthy();
    expect(screen.getByText("RTX 5090")).toBeTruthy();
    expect(screen.getByText("RTX 3090")).toBeTruthy();
  });
});

describe("ModelNetworkCanvas: layout of the smaller card", () => {
  afterEach(cleanup);

  it("orders VRAM, layers, memory and load before the GPU rows", () => {
    const view = fakeCluster([
      fakeSlicePeer("compact", 0, 15, {
        load: 2,
        node: fakeNode({
          node_id: "compact",
          short_id: "compact",
          hostname: "compact-node",
          status: { state: "ready", detail: "servindo", reported_gpu_layers: 16 } as never,
          system: fakeSystem({
            gpu_kind: "nvidia",
            gpus: [{
              index: 0,
              name: "RTX A",
              vendor: "nvidia",
              vram_total_mb: 8192,
              vram_used_mb: 2048,
              vram_used_pct: 25,
              utilization_pct: 30,
              temperature_c: 61,
              power_watts: 250,
            } as never],
          }),
        }),
      }),
    ]);

    const { container } = mount(view);
    const card = container.querySelector("h3")?.closest("article") as HTMLElement;
    const meters = screen.getByTestId("machine-meter-group");
    const vram = screen.getByText("VRAM");
    const layerBar = screen.getByText("camadas");
    const memory = screen.getByText("Memória");
    const load = screen.getByText("Carga");
    const gpu = screen.getByText(/61,0 °C/);
    const status = screen.getByTestId("machine-status");
    const layers = screen.getByTestId("machine-layers");

    expect(meters.querySelectorAll('[aria-label]')).toHaveLength(4);
    expect(vram.compareDocumentPosition(layerBar) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(layerBar.compareDocumentPosition(memory) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(memory.compareDocumentPosition(load) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(load.compareDocumentPosition(gpu) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(status.parentElement).toBe(layers.parentElement);
    expect(status.compareDocumentPosition(layers) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(layers.className).toContain("text-[9px]");
    expect(card.className).toContain("p-3");
  });
});

describe("ModelNetworkCanvas: layers left on the CPU", () => {
  afterEach(cleanup);

  const peerWith = (gpu_kind: "nvidia" | "unified") => fakeCluster([
    fakeSlicePeer("slow", 0, 15, {
      node: fakeNode({
        node_id: "slow",
        short_id: "slow",
        hostname: "slow-node",
        status: { ...fakeNode().status, reported_gpu_layers: 12 },
        system: fakeSystem({ gpu_kind }),
      }),
    }),
  ]);

  it("attaches a warning to a machine with GPUs whose layers spilled to the CPU", () => {
    mount(peerWith("nvidia"));
    expect(screen.getByTestId("machine-cpu-warning")).toBeTruthy();
  });

  it("draws no warning on a Mac, whose memory is unified", () => {
    mount(peerWith("unified"));
    expect(screen.queryByTestId("machine-cpu-warning")).toBeNull();
  });
});

/**
 * What the Dashboard must let you see on a large network: who holds the token
 * (speed color), a smaller view of the cards and the gateway wherever the operator
 * wants.
 */
describe("ModelNetworkCanvas: speed in colored badges", () => {
  afterEach(cleanup);

  it("paints each slice by comparison with the fastest of the model", () => {
    const view = fakeCluster([fakeSlicePeer("a", 0, 15), fakeSlicePeer("b", 16, 31), fakeSlicePeer("c", 32, 47)]);
    mount(view, [], undefined, undefined, [], {
      a: { decode_tokens_per_sec: 100, prefill_tokens_per_sec: 2000, active_generations: 0 },
      b: { decode_tokens_per_sec: 60, prefill_tokens_per_sec: 1900, active_generations: 0 },
      c: { decode_tokens_per_sec: 20, prefill_tokens_per_sec: null, active_generations: 0 },
    });

    const decodes = screen.getAllByTestId("speed-decode");
    expect(decodes.map((badge) => badge.getAttribute("data-tone"))).toEqual(["fast", "medium", "slow"]);
    expect(decodes[2].getAttribute("title")).toBe("decode: 20.0 tok/s, 20% da fatia mais rápida deste modelo. Verde a partir de 75%, âmbar a partir de 40%, vermelho abaixo.");
    const prefills = screen.getAllByTestId("speed-prefill");
    expect(prefills.map((badge) => badge.getAttribute("data-tone"))).toEqual(["fast", "fast", "unknown"]);
  });
});

describe("ModelNetworkCanvas: minimized cards", () => {
  afterEach(() => {
    cleanup();
    localStorage.clear();
  });

  const network = fakeCluster([fakeSlicePeer("a", 0, 15), fakeSlicePeer("b", 16, 31)]);

  function cardOf(container: HTMLElement, hostname: string) {
    const title = Array.from(container.querySelectorAll("h3")).find((h) => h.textContent === hostname);
    return title?.closest("article") as HTMLElement;
  }

  it("minimizes a single card, and the button of the card itself expands it back", () => {
    const { container } = mount(network);
    fireEvent.click(cardOf(container, "a").querySelector('[data-testid="machine-compact-toggle"]')!);

    expect(cardOf(container, "a").getAttribute("data-compact")).toBe("true");
    expect(cardOf(container, "a").style.height).toBe("84px");
    expect(cardOf(container, "b").getAttribute("data-compact")).toBeNull();
    // Minimized, it still shows the essentials.
    expect(cardOf(container, "a").textContent).toContain("L0 - L15");
    expect(cardOf(container, "a").querySelector('[data-testid="machine-status"]')).not.toBeNull();

    fireEvent.click(cardOf(container, "a").querySelector('[data-testid="machine-compact-toggle"]')!);
    expect(cardOf(container, "a").getAttribute("data-compact")).toBeNull();
  });

  it("minimizes and expands all from the models bar, and remembers the choice", () => {
    const { container, unmount } = mount(network);
    const leftOpen = parseFloat(cardOf(container, "b").style.left);
    fireEvent.click(screen.getByRole("button", { name: "Minimizar todos os cards" }));

    expect(cardOf(container, "a").getAttribute("data-compact")).toBe("true");
    expect(cardOf(container, "b").getAttribute("data-compact")).toBe("true");
    // The automatic arrangement brings the columns of smaller cards closer.
    expect(parseFloat(cardOf(container, "b").style.left)).toBeLessThan(leftOpen);

    unmount();
    const again = mount(network);
    expect(cardOf(again.container, "a").getAttribute("data-compact")).toBe("true");
    fireEvent.click(screen.getByRole("button", { name: "Expandir todos os cards" }));
    expect(cardOf(again.container, "a").getAttribute("data-compact")).toBeNull();
    expect(cardOf(again.container, "b").getAttribute("data-compact")).toBeNull();
  });
});

describe("ModelNetworkCanvas: draggable gateway", () => {
  afterEach(() => {
    cleanup();
    localStorage.clear();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("moves the gateway to where it was dropped, keeps the position and reset restores it", () => {
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({ left: 0, top: 0, right: 2000, bottom: 1200, width: 2000, height: 1200, x: 0, y: 0, toJSON: () => ({}) } as DOMRect);
    Object.assign(HTMLElement.prototype, { setPointerCapture: vi.fn(), releasePointerCapture: vi.fn(), hasPointerCapture: () => true });
    // jsdom has no `PointerEvent`; without it the event arrives without `clientX`.
    vi.stubGlobal("PointerEvent", class extends MouseEvent {
      pointerId: number;
      constructor(type: string, init: PointerEventInit = {}) { super(type, init); this.pointerId = init.pointerId ?? 0; }
    });
    mount(cluster);
    const gateway = screen.getByTestId("pipeline-gateway");
    const before = { left: gateway.style.left, top: gateway.style.top };

    fireEvent.pointerDown(gateway, { clientX: parseFloat(before.left) + 10, clientY: parseFloat(before.top) + 10, pointerId: 1 });
    fireEvent.pointerMove(gateway, { clientX: 410, clientY: 610, pointerId: 1 });
    fireEvent.pointerUp(gateway, { pointerId: 1 });

    expect(gateway.style.left).toBe("400px");
    expect(gateway.style.top).toBe("600px");
    const saved = Object.entries(localStorage).find(([key]) => key.includes("network-layout"))?.[1] ?? "{}";
    expect(JSON.parse(saved).__gateway__).toEqual({ left: 400, top: 600 });

    fireEvent.click(screen.getByLabelText(/Redefinir o arranjo/i));
    expect(screen.getByTestId("pipeline-gateway").style.left).toBe(before.left);
  });

  it("opens the pipeline window on the button and does not start a drag", () => {
    const capture = vi.fn();
    Object.assign(HTMLElement.prototype, { setPointerCapture: capture });
    mount(cluster, [], { legsMs: [2, 3, 4], computeMs: 10 });
    const open = screen.getByTestId("pipeline-gateway-open");
    fireEvent.pointerDown(open, { pointerId: 1 });
    fireEvent.click(open);

    expect(capture).not.toHaveBeenCalled();
    expect(screen.getByRole("dialog", { name: "Gateway · pipeline e rotas" })).toBeTruthy();
    expect(screen.getByTestId("pipeline-gateway-details").textContent).toContain("Nenhuma rota medida ainda");
  });
});

describe("ModelNetworkCanvas: machine window", () => {
  afterEach(cleanup);

  it("opens with the machine speed, the named sections and the name only once", () => {
    const view = fakeCluster([fakeSlicePeer("a", 0, 15), fakeSlicePeer("b", 16, 31)]);
    mount(view, [], undefined, undefined, [], {
      a: { decode_tokens_per_sec: 100, prefill_tokens_per_sec: 2000, active_generations: 0 },
      b: { decode_tokens_per_sec: 30, prefill_tokens_per_sec: 1900, active_generations: 0 },
    });
    const card = Array.from(document.querySelectorAll("h3")).find((h) => h.textContent === "b")!.closest("article")!;
    act(() => { fireEvent.doubleClick(card); });

    const dialog = screen.getByRole("dialog", { name: "Detalhes de b" });
    const decode = dialog.querySelector('[data-testid="speed-decode"]');
    expect(decode?.getAttribute("data-tone")).toBe("slow");
    expect(dialog.textContent).toContain("Recursos");
    expect(dialog.textContent).toContain("Implantação");
    // The window title stays for screen readers; the visible header is the card's.
    expect(dialog.querySelector("h2")?.className).toBe("sr-only");
  });
});
