import type { RemoteModelSummary } from "./types";

export const HUGGING_FACE_TRENDING_URL =
  "https://huggingface.co/api/models?sort=trendingScore&limit=20&filter=gguf";

type HuggingFaceModel = {
  id?: unknown;
  author?: unknown;
  downloads?: unknown;
  likes?: unknown;
  private?: unknown;
  gated?: unknown;
  tags?: unknown;
  lastModified?: unknown;
};

export async function getTrendingModels(signal?: AbortSignal): Promise<RemoteModelSummary[]> {
  const response = await fetch(HUGGING_FACE_TRENDING_URL, {
    signal,
    credentials: "omit",
    headers: { Accept: "application/json" },
  });
  if (!response.ok) throw new Error(`Hugging Face returned HTTP ${response.status}`);
  const payload: unknown = await response.json();
  if (!Array.isArray(payload)) throw new Error("Hugging Face returned an invalid model list");

  return payload.flatMap((value: HuggingFaceModel) => {
    if (typeof value?.id !== "string" || value.id.length === 0) return [];
    const tags = Array.isArray(value.tags)
      ? value.tags.filter((tag): tag is string => typeof tag === "string")
      : [];
    return [{
      repo_id: value.id,
      author: typeof value.author === "string" ? value.author : value.id.split("/")[0] ?? null,
      description: null,
      downloads: typeof value.downloads === "number" ? value.downloads : null,
      likes: typeof value.likes === "number" ? value.likes : null,
      private: typeof value.private === "boolean" ? value.private : null,
      gated: typeof value.gated === "boolean" ? value.gated : null,
      tags,
      last_modified: typeof value.lastModified === "string" ? value.lastModified : null,
      has_gguf: true,
      installed: false,
      downloading: false,
    }];
  });
}
