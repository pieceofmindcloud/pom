/**
 * Which route each profile sees.
 *
 * A single table, and the menu and the router read the same one. Enterprise product routes
 * are the discovery exception: they stay open so that the menu can
 * present the feature even before activation. The data blocking happens
 * in the backend and the App shows the purchase state when the entitlement is missing.
 *
 * **Scope:** this is presentation, not authorization - see the header of
 * `profile.tsx`. Blocking here prevents the interface from offering work that
 * does not belong to that profile. Authorization of the administrative surface is done
 * on the server with the access token; the connection key is a separate credential
 * for the nodes and the pipeline.
 *
 * A route that does not appear in `RESTRICTED` is free for every profile, except
 * `infer`, which has an explicit reduced view below. The default is
 * permissive on purpose: the restriction is the exception that someone wrote
 * deliberately, and a new screen does not stop opening because it was forgotten
 * in a list.
 */
import type { Profile } from "./profile";
import { pluginRouteProfiles } from "./pluginUi";

/**
 * Route prefix -> profiles that get in.
 *
 * The key matches by segment prefix, so a future child of `/logs`
 * remains protected, and `/deploy` **does not** cover `/deployx`.
 */
const RESTRICTED: Readonly<Record<string, readonly Profile[]>> = {
  "/deploy": ["admin"],
  "/deployments": ["admin"],
  "/config/administration": ["admin", "worker"],
  "/config/network/participants": ["admin", "worker"],
  // The cluster's connection key is the operator's credential: only the
  // admin sees it.
  "/config/keys/connection": ["admin"],
};

/**
 * Local view of infer. The network route stays visible so that the node can
 * ask to join the processing, but the backend returns only data from this
 * machine.
 */
const INFER_ROUTES = [
  "/public-networks",
  "/chat",
  "/network-chat",
  "/documentation",
  "/config/keys/api",
  // Plugins are local to each machine: an inference node browses the
  // catalog and installs plugins on itself, and their menus join the list
  // below its conversations.
  "/plugins",
  "/config/network",
  "/config/my-machine",
  // Its own token usage and budget: every profile that runs inference has one.
  "/config/usage",
  "/config/budget",
  "/config/sharing",
  "/config/preferences",
  // Anchoring trust precedes authority and is not the privilege of any
  // profile: an `infer` also receives the admin's anchor. The lock on this screen belongs
  // to the server (loopback, node yet to be anchored, fingerprint checked).
  "/config/trust",
] as const;

const INFER_DENIED_ROUTES = [
  "/config/network/participants",
  "/config/preferences/admin",
  // Deployment logs are the network's operation, not an inference node's:
  // hidden even where a plugin claims the route.
  "/deployments",
] as const;

/** `/deployx` is not under `/deploy`. */
function isUnder(path: string, prefix: string): boolean {
  const pathname = path.split(/[?#]/, 1)[0];
  return pathname === prefix || pathname.startsWith(prefix + "/");
}

export function isEnterprisePath(path: string): boolean {
  return isUnder(path, "/logs") || isUnder(path, "/statistics");
}

/** The profiles that get into this route, or `null` when it is free. */
export function routeProfiles(path: string): readonly Profile[] | null {
  // The most specific key wins for any future route exception.
  let best: { prefix: string; profiles: readonly Profile[] } | null = null;
  for (const [prefix, profiles] of Object.entries(RESTRICTED)) {
    if (!isUnder(path, prefix)) continue;
    if (!best || prefix.length > best.prefix.length) best = { prefix, profiles };
  }
  return best?.profiles ?? null;
}

/** Whether this profile can open this route, including Enterprise surfaces. */
export function canAccessRoute(path: string, profile: Profile): boolean {
  // Plugin UIs are local to this node and open to every profile.
  // The explicit blocks on native `infer` surfaces still apply.
  if (profile === "visitor" && INFER_DENIED_ROUTES.some((prefix) => isUnder(path, prefix))) return false;
  const pluginProfiles = pluginRouteProfiles(path);
  if (pluginProfiles) return pluginProfiles.includes(profile);
  // Request logs and statistics: a visitor sees the requests that entered
  // through its own node once a plugin serves them, but gets no advertising
  // fallback before that.
  if (isEnterprisePath(path)) return profile !== "visitor";
  if (profile === "visitor") return INFER_ROUTES.some((prefix) => isUnder(path, prefix));
  const allowed = routeProfiles(path);
  return allowed === null || allowed.includes(profile);
}

/** Enterprise surfaces are intentionally discoverable before activation. */
export function canAccessEnterprise(path: string, _active: boolean): boolean {
  return isEnterprisePath(path);
}

/**
 * The home screen of each profile.
 *
 * The `/` is the Dashboard, which `infer` does not open. Without this, an inference node
 * opens the interface straight on the refusal screen - a dead end, on top of
 * telling you to switch profile, something that no longer exists. It stays here, beside
 * the route table, so as not to become a second list.
 */
export function homeRoute(profile: Profile): string {
  return canAccessRoute("/", profile) ? "/" : "/chat";
}

/**
 * Whether this profile sees the machine-by-machine detail of the Dashboard.
 *
 * The Dashboard has two views of the same cluster: the **Flow**, which is the drawing of the
 * network, and the **Details**, which are the cards with each machine's internal
 * state - runtime, memory, layer range, and the buttons to release a slice,
 * remove from the pipeline and leave the network. The same card opens through the Flow, by clicking
 * on a machine. That is cluster operation, not the node's own: outside `admin`
 * only one view remains, and where there is only one view there is nothing to choose - the selector
 * does not appear either.
 *
 * It stays beside the route table because it is the same question ("what does this profile
 * see"), and because the Dashboard and the canvas must answer the same: hiding the
 * selector and still opening the card by click would be the same screen in two
 * different states.
 */
export function canSeePeerDetails(profile: Profile): boolean {
  return profile === "admin";
}
