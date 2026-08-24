import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

let api: typeof import("./api").api;
let clearConditionalCache: typeof import("./api").clearConditionalCache;

beforeAll(async () => {
  vi.stubGlobal("localStorage", {
    getItem: () => null,
    setItem: () => undefined,
    removeItem: () => undefined,
  });
  const module = await import("./api");
  api = module.api;
  clearConditionalCache = module.clearConditionalCache;
});

describe("conditional JSON requests", () => {
  beforeEach(() => {
    clearConditionalCache();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("reuses the cached value on 304 without reading or parsing a body", async () => {
    const first = {
      ok: true,
      status: 200,
      headers: new Headers({ ETag: '"offers-v1"' }),
      text: vi.fn().mockResolvedValue('{"offers":[]}'),
    };
    const notModified = {
      ok: false,
      status: 304,
      headers: new Headers({ ETag: '"offers-v1"' }),
      text: vi.fn(),
    };
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(first)
      .mockResolvedValueOnce(notModified);
    vi.stubGlobal("fetch", fetchMock);

    const firstValue = await api.getConditional<{ offers: unknown[] }>("/api/transfer/offers");
    const secondValue = await api.getConditional<{ offers: unknown[] }>("/api/transfer/offers");

    expect(secondValue).toBe(firstValue);
    expect(notModified.text).not.toHaveBeenCalled();
    expect(fetchMock.mock.calls[1][1]).toMatchObject({
      headers: { "If-None-Match": '"offers-v1"' },
    });
  });

  it("replaces the cached value when the server version changes", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        headers: new Headers({ ETag: '"jobs-v1"' }),
        text: vi.fn().mockResolvedValue('{"jobs":[]}'),
      })
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        headers: new Headers({ ETag: '"jobs-v2"' }),
        text: vi.fn().mockResolvedValue('{"jobs":[{"id":"1"}]}'),
      });
    vi.stubGlobal("fetch", fetchMock);

    const firstValue = await api.getConditional<{ jobs: unknown[] }>("/api/ui/jobs");
    const secondValue = await api.getConditional<{ jobs: unknown[] }>("/api/ui/jobs");

    expect(secondValue).not.toBe(firstValue);
    expect(secondValue.jobs).toHaveLength(1);
  });
});
