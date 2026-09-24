import { afterEach, describe, expect, it, vi } from "vitest";
import { getTrendingModels, HUGGING_FACE_TRENDING_URL } from "./publicModelDiscovery";

afterEach(() => vi.unstubAllGlobals());

describe("getTrendingModels", () => {
  it("reads public trending GGUF models without credentials and normalizes the public response", async () => {
    const fetch = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => [{
        id: "author/model-GGUF",
        author: "author",
        downloads: 42,
        likes: 7,
        gated: false,
        private: false,
        tags: ["gguf", 1],
        lastModified: "2026-01-01T00:00:00Z",
      }],
    });
    vi.stubGlobal("fetch", fetch);

    await expect(getTrendingModels()).resolves.toEqual([{
      repo_id: "author/model-GGUF",
      author: "author",
      description: null,
      downloads: 42,
      likes: 7,
      private: false,
      gated: false,
      tags: ["gguf"],
      last_modified: "2026-01-01T00:00:00Z",
      has_gguf: true,
      installed: false,
      downloading: false,
    }]);
    expect(fetch).toHaveBeenCalledWith(HUGGING_FACE_TRENDING_URL, expect.objectContaining({
      credentials: "omit",
      headers: { Accept: "application/json" },
    }));
  });

  it("rejects failed or malformed public responses", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 503 }));
    await expect(getTrendingModels()).rejects.toThrow("HTTP 503");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, json: async () => ({}) }));
    await expect(getTrendingModels()).rejects.toThrow("invalid model list");
  });
});
