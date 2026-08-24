import type { Job, TransferOffer } from "./types";

export const FAST_POLL_INTERVAL = 2_000;
export const IDLE_POLL_INTERVAL = 10_000;

/** Keep a quiet notification query cheap without making active work sluggish. */
export function jobsPollInterval(data: { jobs: Pick<Job, "state">[] } | undefined) {
  return data?.jobs.some((job) => job.state === "running")
    ? FAST_POLL_INTERVAL
    : IDLE_POLL_INTERVAL;
}

/** A pending offer is interactive work, so it keeps the fast cadence. */
export function offersPollInterval(
  data: { offers: Pick<TransferOffer, "id">[] } | undefined,
) {
  return data?.offers.length ? FAST_POLL_INTERVAL : IDLE_POLL_INTERVAL;
}
