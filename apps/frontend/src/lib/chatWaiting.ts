/**
 * The line the Chat shows while an answer has not started: which message, and
 * when it changes. The texts play with the product's name - a mind made of
 * pieces spread over machines - and rotate so a long wait does not look
 * frozen.
 *
 * Four sets, picked by what the wait is: `wake` while the deployment is asleep
 * or loading again (`unload_if_idle`), then `pieces`, `assemble` and `long` as
 * the wait grows. The catalog keys are `chat.wait.<set>.<n>`.
 */
export type WaitingSet = "wake" | "pieces" | "assemble" | "long";

/** How many messages each set has in `messages.csv`. */
export const WAITING_SET_SIZE: Record<WaitingSet, number> = {
  wake: 6,
  pieces: 5,
  assemble: 5,
  long: 4,
};

/** How long one message stays before the next one. */
export const WAITING_ROTATE_MS = 3500;
const ASSEMBLE_AFTER_MS = 5000;
const LONG_AFTER_MS = 16000;

export function waitingSet(asleep: boolean, elapsedMs: number): WaitingSet {
  if (asleep) return "wake";
  if (elapsedMs >= LONG_AFTER_MS) return "long";
  if (elapsedMs >= ASSEMBLE_AFTER_MS) return "assemble";
  return "pieces";
}

/**
 * The catalog key for this moment of the wait. `seed` shifts where each set
 * starts, so two waits in a row do not open with the same line; within one
 * wait the messages follow in order and never repeat back to back.
 */
export function waitingMessageKey(input: { asleep: boolean; elapsedMs: number; seed: number }): string {
  const set = waitingSet(input.asleep, input.elapsedMs);
  const size = WAITING_SET_SIZE[set];
  const step = Math.floor(Math.max(0, input.elapsedMs) / WAITING_ROTATE_MS);
  // The waking set always opens with its first line: it is the one that says
  // what is happening.
  const offset = set === "wake" ? 0 : Math.abs(Math.trunc(input.seed));
  return `chat.wait.${set}.${((step + offset) % size) + 1}`;
}
