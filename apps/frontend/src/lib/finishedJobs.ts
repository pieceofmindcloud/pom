import { storageKey } from "./brand";
import { RECENTLY_FINISHED_MS } from "./transferStatus";
import type { Job } from "./types";

/**
 * The jobs the bell still shows after they ended (a deploy, a slicing run).
 *
 * The bell listed only `running` jobs, so a deploy that finished between two
 * polls vanished the instant the bell opened: the badge said (1), the tray was
 * empty, and the operator deployed again. A job that ended within
 * `RECENTLY_FINISHED_MS` stays with its outcome, the same window as a
 * transfer's. A model download is left out: its panel already carries the
 * outcome with Try again and Dismiss.
 */
export function recentlyFinishedJobs(jobs: Job[], now = Date.now()): Job[] {
  return jobs.filter((job) => {
    if (job.kind === "model_download") return false;
    if (job.state !== "done" && job.state !== "failed") return false;
    const finished = job.finished_at ? Date.parse(job.finished_at) : NaN;
    return Number.isFinite(finished) && now - finished <= RECENTLY_FINISHED_MS;
  });
}

const SEEN_KEY = storageKey("finishedJobsSeen");
const DISMISSED_KEY = storageKey("finishedJobsDismissed");
/** Enough for every job one node finishes inside the window. */
const MAX_REMEMBERED = 200;

function read(key: string): Set<string> {
  try {
    const value = JSON.parse(localStorage.getItem(key) ?? "[]");
    return new Set(Array.isArray(value) ? value.filter((id) => typeof id === "string") : []);
  } catch {
    return new Set();
  }
}

function write(key: string, ids: Set<string>) {
  try {
    localStorage.setItem(key, JSON.stringify([...ids].slice(-MAX_REMEMBERED)));
  } catch {
    // Private window or full storage: the bell still works for this tab.
  }
}

/** Finished jobs this browser already showed in an open bell (not unread). */
export function readSeenJobs(): Set<string> {
  return read(SEEN_KEY);
}

export function markJobsSeen(ids: string[]): Set<string> {
  const seen = read(SEEN_KEY);
  for (const id of ids) seen.add(id);
  write(SEEN_KEY, seen);
  return seen;
}

/** Finished jobs the operator removed from the bell. */
export function readDismissedJobs(): Set<string> {
  return read(DISMISSED_KEY);
}

export function dismissJob(id: string): Set<string> {
  const dismissed = read(DISMISSED_KEY);
  dismissed.add(id);
  write(DISMISSED_KEY, dismissed);
  return dismissed;
}
