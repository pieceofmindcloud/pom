import type { TransferJob } from "./types";

export type TransferState = "running" | "verifying" | "done" | "failed";

export function isFinishedTransfer(state: TransferJob["state"]) {
  return state === "done" || state === "failed";
}

export function isActiveTransfer(state: TransferJob["state"]) {
  return !isFinishedTransfer(state);
}
