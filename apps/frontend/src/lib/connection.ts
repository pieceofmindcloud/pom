import type { ConnectionStatus } from "./types";

/**
 * How long one step of the way into a network may take before the screens say
 * it looks stuck and show why. Finding the network and getting the token take
 * seconds on a good path; a minute means something is in the way.
 */
export const CONNECTION_STUCK_AFTER_SECS = 60;

/** Whether this node is still on its way into its network. */
export function isConnecting(status: ConnectionStatus | undefined): boolean {
  return status?.phase === "discovering" || status?.phase === "authenticating";
}

/** Whether the current step has lasted past `CONNECTION_STUCK_AFTER_SECS`. */
export function connectionStuck(status: ConnectionStatus | undefined, nowSecs = Date.now() / 1000): boolean {
  return isConnecting(status) && nowSecs - status!.since >= CONNECTION_STUCK_AFTER_SECS;
}
