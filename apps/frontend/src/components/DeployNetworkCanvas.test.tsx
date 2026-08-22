// @vitest-environment jsdom
import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { DeployMachine, DeployPlan, DeploySelection, DeployStep } from "../lib/types";
import { DeployNetworkCanvas } from "./DeployNetworkCanvas";

function machine(node_id: string): DeployMachine {
  return {
    node_id,
    label: node_id,
    mem_free_mb: 32_000,
    gpu_free_mb: 12_000,
    ram_free_mb: 32_000,
    disk_free_mb: 500_000,
    rtt_ms: null,
    load: 0,
    holds: [],
    taking: null,
  };
}

function step(node_id: string, start_layer: number, end_layer: number): DeployStep {
  return {
    position: 1,
    start_layer,
    end_layer,
    exec_start: start_layer,
    exec_end: end_layer,
    total_layers: 32,
    name: `${node_id}-${start_layer}-${end_layer}.gguf`,
    size_bytes: 1_000_000,
    target_node_id: node_id,
    target_host: node_id,
    ready: true,
    source_node_id: null,
    source_host: null,
    source_path: null,
    target_path: null,
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

function plan(): DeployPlan {
  return {
    group: "qwen",
    deployment_name: "qwen",
    strategy: "simple",
    kind: "shards",
    total_layers: 32,
    total_layers_inferred: false,
    // The whole-range holder overlaps both execution subranges. It must not
    // cause the canvas to invent branch -> whole -> tail edges.
    steps: [step("branch", 0, 15), step("whole", 0, 31), step("tail", 16, 31)],
    complete: true,
    transfer_bytes: 0,
    blocked: null,
    auto: true,
    merge: false,
    available: [],
    machines: [machine("whole"), machine("branch"), machine("tail")],
    missing: [],
    overlap: [],
    warnings: [],
    params: {} as DeployPlan["params"],
    prompt_tokens: 0,
    chunk_tokens: 0,
    route_options: [
      {
        from_node_id: "branch",
        to_node_id: "tail",
        rtt_ms: 4.5,
        selected: true,
        note: "rota contígua medida no peer de origem",
      },
    ],
    load_distribution: [],
  };
}

describe("DeployNetworkCanvas", () => {
  it("renders execution subranges and the backend's contiguous branch edge", () => {
    const rows: DeploySelection[] = [
      { start_layer: 0, end_layer: 15, target_node_id: "branch" },
      { start_layer: 16, end_layer: 31, target_node_id: "tail" },
    ];

    render(
      <DeployNetworkCanvas
        plan={plan()}
        rows={rows}
        inspectedNodeId={null}
        onInspect={vi.fn()}
        onAddPeer={vi.fn()}
        onRemovePeer={vi.fn()}
        onDropPeer={vi.fn()}
        labelOf={(nodeId) => nodeId}
      />,
    );

    expect(screen.getAllByText("executa L0–15").length).toBeGreaterThan(0);
    expect(screen.getAllByText("executa L16–31").length).toBeGreaterThan(0);
    expect(screen.getByText("branch → tail · 4.5 ms")).toBeTruthy();
    expect(screen.getAllByText("RTT 4.5 ms").length).toBeGreaterThan(0);
  });
});
