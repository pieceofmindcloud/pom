import type { Job } from "./types";

export const MODEL_DOWNLOAD_POLL_INTERVAL = 2_000;
export const MODEL_DOWNLOAD_IDLE_INTERVAL = 10_000;

export interface ModelSearchPathOptions {
  query: string;
  ggufOnly: boolean;
  page?: number;
  pageSize?: number;
  sort?: "relevance" | "downloads" | "recent";
}

export function modelSearchPath(options: ModelSearchPathOptions): string {
  const params = new URLSearchParams();
  params.set("q", options.query);
  if (options.page != null) params.set("page", String(options.page));
  if (options.pageSize != null) params.set("page_size", String(options.pageSize));
  if (options.ggufOnly) params.set("gguf_only", "true");
  if (options.sort && options.sort !== "relevance") params.set("sort", options.sort);
  return `/api/ui/model-provider/search?${params.toString()}`;
}

export function modelDownloadJobs(jobs: Job[]): Job[] {
  return jobs.filter((job) => job.kind === "model_download");
}

export function modelDownloadPollInterval(jobs: Job[]): number {
  return jobs.some((job) => job.state === "running")
    ? MODEL_DOWNLOAD_POLL_INTERVAL
    : MODEL_DOWNLOAD_IDLE_INTERVAL;
}

export function percentOfDownload(job: Pick<Job, "bytes_downloaded" | "total_bytes">): number | null {
  if (
    job.bytes_downloaded == null ||
    job.total_bytes == null ||
    job.total_bytes <= 0
  ) {
    return null;
  }
  return Math.min(100, Math.max(0, (job.bytes_downloaded / job.total_bytes) * 100));
}

export function isModelDownloadActive(job: Job): boolean {
  return job.kind === "model_download" && job.state === "running";
}

const SPLIT_PART = /^(.*)-(\d{5})-of-(\d{5})\.gguf$/i;

/**
 * The other files of a repository that a download of `filename` goes with:
 * `parts` are the rest of a split GGUF (the model does not load without them),
 * `sidecars` the vision projectors and MTP drafts the operator may add.
 */
export function downloadCompanions(filename: string, files: { filename: string; size_bytes: number | null }[]) {
  const split = SPLIT_PART.exec(filename);
  const parts = split
    ? files.filter((file) => {
        const other = SPLIT_PART.exec(file.filename);
        return file.filename !== filename && other !== null && other[1] === split[1] && other[3] === split[3];
      })
    : [];
  const sidecars = files.filter(
    (file) => file.filename !== filename && !parts.includes(file) && /mmproj|(^|[-_.])mtp([-_.]|$)/i.test(file.filename),
  );
  return { parts, sidecars };
}
