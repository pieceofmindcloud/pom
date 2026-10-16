import type { TransferJob } from "./types";

export function isFinishedTransfer(state: TransferJob["state"]) {
  return state === "done" || state === "failed" || state === "cancelled";
}

export function isActiveTransfer(state: TransferJob["state"]) {
  return !isFinishedTransfer(state);
}

/**
 * Only a transfer in progress can be cancelled.
 *
 * Offering the button on one that has already finished would invite the operator to an action
 * with no effect, and on an already verified download it would suggest that the
 * file already on disk can be undone - which the route does not do.
 */
export function canCancel(state: TransferJob["state"]) {
  return state === "running" || state === "verifying";
}

/** For how long a transfer's outcome stays in the bell. */
export const RECENTLY_FINISHED_MS = 5 * 60 * 1000;

/**
 * Finished a moment ago - what the bell still needs to show.
 *
 * The bell listed only what was in progress, so "completed", "failed" and
 * "refused" vanished from the screen at the instant they happened: the receiver
 * saw the bar move and then nothing, and the sender saw no outcome at all. The
 * result only survived on the Transfers screen, which was exactly the field
 * report.
 *
 * A missing `finished_at` is a node of an earlier version, which does not stamp
 * the time: there it is impossible to know whether it ended just now or
 * yesterday, and inventing "now" would fill the bell with old transfers on every
 * page load.
 */
export function isRecentlyFinished(job: TransferJob, now: number = Date.now()) {
  if (!isFinishedTransfer(job.state)) return false;
  if (!job.finished_at) return false;
  const at = Date.parse(job.finished_at);
  if (Number.isNaN(at)) return false;
  return now - at < RECENTLY_FINISHED_MS;
}

/** What the bell shows: what is running, plus what has just finished. */
export function isWorthNotifying(job: TransferJob, now: number = Date.now()) {
  return isActiveTransfer(job.state) || isRecentlyFinished(job, now);
}
