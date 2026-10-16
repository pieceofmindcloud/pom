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

/**
 * The administrative session lives in the node's memory and the human token
 * lives in this tab's memory: a node restart, or the end of validity, drops
 * both with the tab open. Without this notice, each screen printed the server's
 * raw 401 and the operator had no way back to the gate other than reloading.
 */
describe("loss of the administrative session", () => {
  function refusal(status: number, kind: string | null) {
    return {
      ok: false,
      status,
      headers: { get: (name: string) => (name === "x-pom-admin-error" ? kind : null) },
      text: vi.fn().mockResolvedValue('{"error":"missing administrative access token"}'),
    };
  }

  it("warns and drops the credential when the node names the refusal", async () => {
    apiModule.setAdminAccessToken("admin-secret");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(refusal(401, "missing-token")));
    const seen = vi.fn();
    const stop = apiModule.onAdminSessionLost(seen);

    await expect(apiModule.api.get("/api/ui/paths", { auth: false })).rejects.toThrow();

    expect(seen).toHaveBeenCalledTimes(1);
    expect(apiModule.getAdminAccessToken()).toBe("");
    stop();
  });

  it("also warns when the credential exists and does not match", async () => {
    apiModule.setAdminAccessToken("admin-secret");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(refusal(403, "invalid-token")));
    const seen = vi.fn();
    const stop = apiModule.onAdminSessionLost(seen);

    await expect(apiModule.api.get("/admin/api-keys")).rejects.toThrow();

    expect(seen).toHaveBeenCalledTimes(1);
    stop();
  });

  /**
   * A profile 403 is not a lost session: sending back to the gate someone who
   * is already authenticated forces them to reauthenticate to see the same refusal.
   */
  it("does not mistake a profile refusal for a lost session", async () => {
    apiModule.setAdminAccessToken("admin-secret");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({
      ok: false,
      status: 403,
      headers: { get: () => null },
      text: vi.fn().mockResolvedValue('{"error":"perfil sem autorização para esta rota"}'),
    }));
    const seen = vi.fn();
    const stop = apiModule.onAdminSessionLost(seen);

    await expect(apiModule.api.get("/api/ui/models")).rejects.toThrow();

    expect(seen).not.toHaveBeenCalled();
    expect(apiModule.getAdminAccessToken()).toBe("admin-secret");
    stop();
  });

  it("ignores a refusal coming from a route outside the administrative surface", async () => {
    apiModule.setAdminAccessToken("admin-secret");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(refusal(401, "missing-token")));
    const seen = vi.fn();
    const stop = apiModule.onAdminSessionLost(seen);

    await expect(apiModule.api.get("/peers", { auth: false })).rejects.toThrow();

    expect(seen).not.toHaveBeenCalled();
    stop();
  });
});
