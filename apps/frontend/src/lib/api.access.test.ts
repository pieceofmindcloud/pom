import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

let apiModule: typeof import("./api");

beforeAll(async () => {
  vi.stubGlobal("localStorage", {
    getItem: () => null,
    setItem: () => undefined,
    removeItem: () => undefined,
  });
  apiModule = await import("./api");
});

afterEach(() => {
  apiModule.clearAdminAccessToken();
  apiModule.forgetChatApiKey();
  vi.unstubAllGlobals();
});

describe("administrative access token", () => {
  it("is sent only to administrative paths, never to pipeline routes", async () => {
    apiModule.setAdminAccessToken("admin-secret");
    const fetchMock = vi.fn((path: string) => Promise.resolve(path === "/api/ui/chat-key"
      ? { ok: true, status: 201, text: vi.fn().mockResolvedValue('{"key":"sk-chat"}') }
      : { ok: true, status: 200, text: vi.fn().mockResolvedValue("{}") }));
    vi.stubGlobal("fetch", fetchMock);

    await apiModule.api.get("/api/ui/node", { auth: false });
    await apiModule.api.get("/v1/models", { auth: false });

    expect(fetchMock.mock.calls[0][1].headers).toMatchObject({
      "X-POM-Access-Token": "admin-secret",
    });
    const inferenceCall = fetchMock.mock.calls.find(([path]) => path === "/v1/models");
    expect(inferenceCall?.[1].headers).not.toHaveProperty("X-POM-Access-Token");
    expect(inferenceCall?.[1].headers).toMatchObject({ "X-API-Key": "sk-chat" });
    expect(fetchMock.mock.calls[0][1]).toMatchObject({ credentials: "same-origin" });
    expect(inferenceCall?.[1]).toMatchObject({ credentials: "same-origin" });
  });

  it("recreates a revoked dedicated key and retries inference once", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce({
        ok: true,
        status: 201,
        text: vi.fn().mockResolvedValue('{"key":"sk-old"}'),
      })
      .mockResolvedValueOnce({
        ok: false,
        status: 403,
        text: vi.fn().mockResolvedValue('{"error":"invalid inference API key"}'),
      })
      .mockResolvedValueOnce({
        ok: true,
        status: 201,
        text: vi.fn().mockResolvedValue('{"key":"sk-new"}'),
      })
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        text: vi.fn().mockResolvedValue("{}"),
      });
    vi.stubGlobal("fetch", fetchMock);

    await apiModule.api.get("/v1/models");

    const inferenceCalls = fetchMock.mock.calls.filter(([path]) => path === "/v1/models");
    expect(inferenceCalls).toHaveLength(2);
    expect(inferenceCalls[0][1].headers).toMatchObject({ "X-API-Key": "sk-old" });
    expect(inferenceCalls[1][1].headers).toMatchObject({ "X-API-Key": "sk-new" });
    expect(JSON.stringify(inferenceCalls)).not.toContain("invalid inference API key");
  });

  it("never stores the administrative token or puts it in a URL", async () => {
    const storage = {
      getItem: () => null,
      setItem: vi.fn(),
      removeItem: vi.fn(),
    };
    vi.stubGlobal("localStorage", storage);
    vi.stubGlobal("sessionStorage", storage);
    apiModule.setAdminAccessToken("admin-secret");
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: vi.fn().mockResolvedValue("{}"),
    });
    vi.stubGlobal("fetch", fetchMock);

    await apiModule.api.get("/api/ui/node", { auth: false });

    expect(storage.setItem).not.toHaveBeenCalledWith(expect.anything(), "admin-secret");
    expect(fetchMock.mock.calls[0][0]).not.toContain("admin-secret");
  });
});
