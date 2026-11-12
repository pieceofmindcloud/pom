/**
 * Who this browser session is: the node's operator, or a person who signed
 * in through a plugin's provider (`crates/node/src/backend/user_session.rs`).
 *
 * The node answers `GET /api/ui/me`. A person reaches only the user surface
 * (the dashboard, the Chat, their own logs and keys); the server enforces it,
 * and this module only keeps the interface from offering the rest.
 */
export interface SessionUser {
  user_id: string;
  label: string;
  email?: string | null;
  provider: string;
  plugin: string;
}

/**
 * A person's seat on a demonstration node (`crates/node/src/backend/demo_slots.rs`):
 * holding one for `remaining_secs`, or waiting in the queue at `position`.
 */
export interface DemoSeat {
  state: "active" | "queued" | "expired";
  remaining_secs?: number;
  expires_at?: number;
  position?: number;
  waiting?: number;
  eta_secs?: number;
  session_secs?: number;
  max_sessions?: number;
}

export interface SessionInfo {
  kind: "operator" | "user";
  demo_mode: boolean;
  can_mint_keys?: boolean;
  user?: SessionUser;
  demo?: DemoSeat;
}

let current: SessionInfo | null = null;

export function sessionInfo(): SessionInfo | null {
  return current;
}

export function isUserSession(): boolean {
  return current?.kind === "user";
}

/** A person signed in on a demonstration node: a read-only, timed visit. */
export function isDemoUser(): boolean {
  return current?.kind === "user" && current.demo_mode === true;
}

export function setSessionInfo(info: SessionInfo | null) {
  current = info;
}

/** Reads `/api/ui/me`; an older node without it is an operator session. */
export async function loadSession(): Promise<SessionInfo> {
  try {
    // Imported here: `api.ts` reads localStorage on load, and the route
    // table (`access.ts`) that reads this module runs where there is none.
    const { api } = await import("./api");
    const info = await api.get<SessionInfo>("/api/ui/me");
    current = info?.kind === "user" ? info : { kind: "operator", demo_mode: Boolean(info?.demo_mode) };
  } catch {
    current = { kind: "operator", demo_mode: false };
  }
  return current;
}

export async function signOut() {
  try {
    const { api } = await import("./api");
    await api.post("/api/ui/logout", {});
  } finally {
    current = null;
    window.location.assign("/");
  }
}

/** Routes a signed-in person may open. */
export const USER_ROUTES = ["/", "/chat", "/logs", "/statistics", "/documentation", "/config/my-keys"] as const;

/**
 * What a demonstration node also shows a person, read-only: the plugins, the
 * models and slices, and the network it is in. Searching and downloading
 * models (the Hugging Face plugin's screen) is not among them.
 */
export const DEMO_USER_ROUTES = [
  "/plugins",
  "/models",
  "/models/slice",
  "/shards",
  "/config/network",
  // Networks read-only (its quick setup walks but never saves), the network
  // Chat, and Deploy to play with: never deploys, and never the history.
  "/public-networks",
  "/network-chat",
  "/deploy",
] as const;
