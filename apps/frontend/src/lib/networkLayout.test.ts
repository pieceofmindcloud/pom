import { describe, expect, it } from "vitest";
import { deploymentLayoutKey, fallbackPosition } from "./networkLayout";

describe("network layout", () => {
  it("invalidates stored positions when a deployment model changes", () => {
    const peers = [
      { node_id: "node-a", start_layer: 0, end_layer: 15, changed_at: "2026-08-12T10:00:00Z" },
      { node_id: "node-b", start_layer: 16, end_layer: 31, changed_at: "2026-08-12T10:00:00Z" },
    ];

    expect(deploymentLayoutKey("qwen", peers)).not.toBe(deploymentLayoutKey("llama", peers));
  });

  it("invalidates stored positions when a peer receives a new range", () => {
    const peers = [{ node_id: "node-a", start_layer: 0, end_layer: 15, changed_at: "2026-08-12T10:00:00Z" }];
    const moved = [{ node_id: "node-a", start_layer: 0, end_layer: 31, changed_at: "2026-08-12T10:00:00Z" }];

    expect(deploymentLayoutKey("qwen", peers)).not.toBe(deploymentLayoutKey("qwen", moved));
  });

  it("places the automatic layout inside the canvas bounds", () => {
    expect(fallbackPosition(0)).toEqual({ left: 48, top: 96 });
    expect(fallbackPosition(5)).toEqual({ left: 668, top: 336 });
  });
});
