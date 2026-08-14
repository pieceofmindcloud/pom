// @vitest-environment jsdom
import { beforeEach, describe, expect, it } from "vitest";
import { recallDeploy, rememberDeploy, usableSelection } from "./deployMemory";
import type { DeploySelection } from "./types";

const row = (start: number, end: number, target: string): DeploySelection => ({
  start_layer: start,
  end_layer: end,
  target_node_id: target,
});

describe("rememberDeploy / recallDeploy", () => {
  beforeEach(() => localStorage.clear());

  it("gives back the same choice for the same model", () => {
    rememberDeploy("qwen", {
      selection: [row(0, 15, "a"), row(16, 31, "b")],
      params: null,
      merge: true,
    });

    const back = recallDeploy("qwen");
    expect(back?.selection).toHaveLength(2);
    expect(back?.merge).toBe(true);
  });

  it("keeps one memory per model, not one for the whole screen", () => {
    rememberDeploy("qwen", { selection: [row(0, 15, "a")], params: null, merge: false });
    rememberDeploy("phi", { selection: [row(0, 5, "b")], params: null, merge: false });

    expect(recallDeploy("qwen")?.selection[0].target_node_id).toBe("a");
    expect(recallDeploy("phi")?.selection[0].target_node_id).toBe("b");
    expect(recallDeploy("nunca-implantado")).toBeNull();
  });

  it("survives a corrupt entry instead of taking the screen down with it", () => {
    localStorage.setItem("codai.deploy.last", "{ not json");
    expect(recallDeploy("qwen")).toBeNull();
  });
});

describe("usableSelection", () => {
  const ranges = [
    { start_layer: 0, end_layer: 15 },
    { start_layer: 16, end_layer: 31 },
  ];

  it("restores what the cluster still has", () => {
    expect(usableSelection([row(0, 15, "a"), row(16, 31, "b")], ranges, ["a", "b"])).toEqual([
      row(0, 15, "a"),
      row(16, 31, "b"),
    ]);
  });

  it("drops a range nobody holds any more", () => {
    const kept = usableSelection([row(0, 15, "a"), row(32, 47, "b")], ranges, ["a", "b"]);
    expect(kept).toEqual([row(0, 15, "a")]);
  });

  it("keeps the range but forgets a machine that left the cluster", () => {
    const kept = usableSelection([row(0, 15, "foi-embora")], ranges, ["a"]);
    expect(kept).toEqual([row(0, 15, "")]);
  });

  it("returns null when nothing survives, so the node's suggestion stands", () => {
    expect(usableSelection([row(90, 99, "a")], ranges, ["a"])).toBeNull();
    expect(usableSelection([], ranges, ["a"])).toBeNull();
  });
});
