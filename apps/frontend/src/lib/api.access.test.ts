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
  vi.unstubAllGlobals();
});

describe("administrative access token", () => {
  it("is sent only to administrative paths, never to pipeline routes", async () => {
    apiModule.setAdminAccessToken("admin-secret");
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: vi.fn().mockResolvedValue("{}"),
    });
    vi.stubGlobal("fetch", fetchMock);

    await apiModule.api.get("/api/ui/node", { auth: false });
    await apiModule.api.get("/v1/models", { auth: false });

    expect(fetchMock.mock.calls[0][1].headers).toMatchObject({
      "X-POM-Access-Token": "admin-secret",
    });
    expect(fetchMock.mock.calls[1][1].headers).not.toHaveProperty("X-POM-Access-Token");
    expect(fetchMock.mock.calls[0][1]).toMatchObject({ credentials: "same-origin" });
    expect(fetchMock.mock.calls[1][1]).toMatchObject({ credentials: "same-origin" });
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
