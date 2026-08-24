import { beforeAll, afterEach, describe, expect, it, vi } from "vitest";

let apiModule: typeof import("./api");

beforeAll(async () => {
  vi.stubGlobal("localStorage", {
    getItem: () => null,
    setItem: () => undefined,
    removeItem: () => undefined,
  });
  apiModule = await import("./api");
});

afterEach(() => vi.unstubAllGlobals());

describe("remote model API", () => {
  it("sends structured search and preserves AbortSignal", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: vi.fn().mockResolvedValue('{"models":[],"total":0,"page":1,"page_size":20,"has_next":false}'),
    });
    vi.stubGlobal("fetch", fetchMock);
    const signal = new AbortController().signal;

    await apiModule.searchRemoteModels(
      { query: "Qwen", ggufOnly: true, page: 1, pageSize: 20 },
      signal,
    );

    expect(fetchMock).toHaveBeenCalledWith(
      "/api/ui/model-provider/search?q=Qwen&page=1&page_size=20&gguf_only=true",
      expect.objectContaining({ signal }),
    );
    expect(String(fetchMock.mock.calls[0][0])).not.toContain("hf download");
  });

  it("reads structured Hugging Face CLI status", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: vi.fn().mockResolvedValue(
        '{"cli":{"installed":false,"version":null,"authenticated":false,"username":null}}',
      ),
    });
    vi.stubGlobal("fetch", fetchMock);

    const status = await apiModule.getRemoteModelProviderStatus();

    expect(status.cli.installed).toBe(false);
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/ui/model-provider/status",
      expect.objectContaining({ method: "GET" }),
    );
  });

  it("starts, cancels and retries through structured backend endpoints", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: vi.fn().mockResolvedValue('{"download_id":"d-1"}'),
    });
    vi.stubGlobal("fetch", fetchMock);

    await apiModule.startRemoteModelDownload({
      repoId: "Qwen/model",
      revision: "main",
      filename: "model-Q5_K_M.gguf",
      quantization: "Q5_K_M",
    });
    await apiModule.cancelRemoteModelDownload("d-1");
    await apiModule.retryRemoteModelDownload("d-1");

    expect(fetchMock.mock.calls.map(([path, options]) => [path, options?.method])).toEqual([
      ["/api/ui/model-provider/downloads", "POST"],
      ["/api/ui/model-provider/downloads/d-1/cancel", "POST"],
      ["/api/ui/model-provider/downloads/d-1/retry", "POST"],
    ]);
  });
});
