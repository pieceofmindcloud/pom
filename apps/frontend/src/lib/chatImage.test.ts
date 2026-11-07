// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => ({
  ApiError: class ApiError extends Error {
    constructor(public status: number, message: string) { super(message); }
  },
  ensureChatApiKey: vi.fn(() => Promise.resolve("pom-key")),
  forgetChatApiKey: vi.fn(),
}));

import { loadChatImage, startChatImage } from "./chatImage";

function ndjson(lines: string[], status = 200) {
  const body = new ReadableStream({
    start(controller) {
      for (const line of lines) controller.enqueue(new TextEncoder().encode(line));
      controller.close();
    },
  });
  return new Response(body, { status, headers: { "content-type": "application/x-ndjson" } });
}

afterEach(() => vi.unstubAllGlobals());

describe("chat images", () => {
  it("reports progress then resolves with the stored images", async () => {
    const fetchMock = vi.fn(() => Promise.resolve(ndjson(['{"p":1,"of":8}\n{"p":', '8,"of":8}\n', '{"images":[{"hash":"' + "a".repeat(64) + '","ext":"png","width":8,"height":8}]}\n'])));
    vi.stubGlobal("fetch", fetchMock);
    const progress: number[] = [];
    const images = await startChatImage({ workflowId: "builtin:img-1", nodeId: "n1", prompt: "a fox", onProgress: (p) => progress.push(p) });
    expect(progress).toEqual([1, 8]);
    expect(images[0].ext).toBe("png");
    const [, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(JSON.parse(init.body as string)).toEqual({ workflow_id: "builtin:img-1", node_id: "n1", prompt: "a fox" });
    expect(new Headers(init.headers).get("X-API-Key")).toBe("pom-key");
  });

  it("rejects with the plugin's reason", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(ndjson(['{"error":"no machine serves diffusion of img-x"}\n']))));
    await expect(startChatImage({ workflowId: "w", nodeId: "n", prompt: "x" })).rejects.toThrow("no machine serves diffusion of img-x");
  });

  it("says when the node no longer has an image", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("{}", { status: 404 }))));
    expect(await loadChatImage("a".repeat(64))).toBeNull();
  });
});
