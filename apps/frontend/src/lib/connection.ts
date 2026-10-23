import type { ConnectionStatus } from "./types";

/**
 * How long one step of the way into a network may take before the screens say
 * it looks stuck and show why. Finding the network and getting the token take
 * seconds on a good path; a minute means something is in the way.
 */
export const CONNECTION_STUCK_AFTER_SECS = 60;

/**
 * How long after the token arrives the chat keeps showing the last step,
 * "see the models", before it says the network serves none: the models list
 * and the cluster view catch up a few seconds after the token.
 */
export const MODELS_GRACE_SECS = 90;

/** Whether this node is still on its way into its network. */
export function isConnecting(status: ConnectionStatus | undefined): boolean {
  return status?.phase === "discovering" || status?.phase === "authenticating";
}

/** Whether the current step has lasted past `CONNECTION_STUCK_AFTER_SECS`. */
export function connectionStuck(status: ConnectionStatus | undefined, nowSecs = Date.now() / 1000): boolean {
  return isConnecting(status) && nowSecs - status!.since >= CONNECTION_STUCK_AFTER_SECS;
}

/** Whether the token arrived so recently that the models may still be on their way. */
export function awaitingModels(status: ConnectionStatus | undefined, nowSecs = Date.now() / 1000): boolean {
  return status?.phase === "connected" && nowSecs - status.since < MODELS_GRACE_SECS;
}

/** Whether a member holds its token but sees no other machine of the network. */
export function clusterOutOfSight(status: ConnectionStatus | undefined): boolean {
  return status?.phase === "connected" && (status.peers ?? 0) === 0;
}
