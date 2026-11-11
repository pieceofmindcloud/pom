/**
 * A visit to a demonstration node (`DEMO_MODE=1`, `crates/node/src/backend/demo_slots.rs`).
 *
 * The node opens to `DEMO_MAX_SESSIONS` people at a time for `DEMO_SESSION_TIME`
 * minutes each. This file draws the three moments of a visit: the queue while
 * every seat is taken, the welcome dialog once a seat is held, and the clock in
 * the top bar until it runs out. It also holds the read-only network view a
 * visitor gets in place of the Network settings. The node enforces all of it;
 * this is only what the person sees.
 */
import { Clock, Globe2, Hourglass, LogOut, MessageCircle, Star, Users } from "lucide-react";
import { useEffect, useState } from "react";
import { api, ApiError } from "../lib/api";
import { useCluster, useNetworkInfo, useNode } from "../lib/queries";
import { isDemoUser, sessionInfo, signOut, type DemoSeat } from "../lib/session";
import { useI18n } from "../i18n/provider";
import { Modal } from "./Confirm";
import { Badge, Button, KeyValue, Panel, PanelHeader } from "./ui";
import { peerDisplayName } from "../lib/peerName";
import pomMark from "../assets/pom-mark-dark.png";

/** The public POM repository, where a visitor who liked it can leave a star. */
const POM_REPOSITORY = "https://github.com/pieceofmindcloud/pom";
/** The POM community on Discord, offered in the welcome dialog. */
const POM_DISCORD = "https://discord.gg/ptGMZbaDkN";
const EXPIRED_FLAG = "pom.demo.expired";
const WELCOMED_FLAG = "pom.demo.welcomed";

function storage(): Storage | null {
  try {
    return window.sessionStorage;
  } catch {
    return null;
  }
}

/**
 * `DEMO_NO_AUTH`: why a guest is not sent straight back in. The node answers
 * `/?demo_cooldown=<secs>` while the same visitor waits after their time ran
 * out; a tab whose time just ran out does not re-enter on its own either.
 */
export function guestHold(): { cooldownSecs: number } | { expired: true } | { stuck: true } | null {
  const raw = new URLSearchParams(window.location.search).get("demo_cooldown");
  const secs = raw ? Number(raw) : NaN;
  if (Number.isFinite(secs) && secs > 0) return { cooldownSecs: secs };
  if (takeDemoExpired()) return { expired: true };
  return recentGuestStart() ? { stuck: true } : null;
}

const GUEST_START_FLAG = "pom.demo.guestStart";
/** A guest start this tab made this long ago and came back without a session is a loop, not a visit. */
const GUEST_LOOP_MS = 15_000;

function recentGuestStart(): boolean {
  try {
    const at = Number(window.sessionStorage.getItem(GUEST_START_FLAG));
    return Number.isFinite(at) && at > 0 && Date.now() - at < GUEST_LOOP_MS;
  } catch {
    return false;
  }
}

/** Sends the tab to the guest start, noting when, so a start that does not
 *  stick (no session after it) stops at the gate instead of reloading forever. */
export function startGuestVisit(url: string) {
  try {
    window.sessionStorage.setItem(GUEST_START_FLAG, String(Date.now()));
  } catch {
    // without storage there is no guard, but the visit still opens
  }
  window.location.replace(url);
}

/** Whether the last visit of this tab ended because its time ran out. */
export function takeDemoExpired(): boolean {
  const store = storage();
  const expired = store?.getItem(EXPIRED_FLAG) === "1";
  store?.removeItem(EXPIRED_FLAG);
  return expired;
}

/**
 * The seat of this visit, polled: every 5 s while queued (polling is what
 * keeps the place), every 30 s while seated. A refusal means the time ran out
 * and the node ended the session: back to the sign-in screen, which says so.
 */
export function useDemoSeat(enabled: boolean) {
  const [seat, setSeat] = useState<DemoSeat | undefined>(() => sessionInfo()?.demo);
  useEffect(() => {
    if (!enabled) return;
    let live = true;
    let timer: number | undefined;
    const poll = async () => {
      let next: DemoSeat | undefined;
      try {
        next = await api.get<DemoSeat>("/api/ui/demo/session");
        if (live) setSeat(next);
      } catch (error) {
        if (error instanceof ApiError && error.status === 401) {
          storage()?.setItem(EXPIRED_FLAG, "1");
          window.location.assign("/");
          return;
        }
      }
      if (live) timer = window.setTimeout(poll, next?.state === "queued" ? 5_000 : 30_000);
    };
    void poll();
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
  }, [enabled]);
  return seat;
}

/** Seconds left until `expiresAt` (unix seconds), ticking every second. */
function useCountdown(expiresAt: number | undefined) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1_000);
    return () => window.clearInterval(timer);
  }, []);
  if (!expiresAt) return null;
  return Math.max(0, Math.round(expiresAt - now / 1000));
}

function clock(seconds: number) {
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}

/** The estimate as minutes, never "0": the next poll may still be a few seconds away. */
function minutes(seconds: number) {
  return Math.max(1, Math.ceil(seconds / 60));
}

/** The top bar's session clock, beside the quota. Amber in the last five minutes. */
export function DemoClock() {
  const { t } = useI18n();
  const seat = useDemoSeat(isDemoUser());
  const left = useCountdown(seat?.state === "active" ? seat.expires_at : undefined);
  useEffect(() => {
    if (left === 0) {
      storage()?.setItem(EXPIRED_FLAG, "1");
      window.location.assign("/");
    }
  }, [left]);
  if (!isDemoUser() || left === null) return null;
  const tone = left <= 60 ? "rose" : left <= 300 ? "amber" : "iris";
  return (
    <Badge tone={tone} data-testid="demo-clock">
      <span title={t("demo.clockTitle")} aria-label={t("demo.clockLabel", { time: clock(left) })} className="flex items-center gap-1">
        <Clock className="h-3 w-3" aria-hidden="true" />
        <span className="font-mono">{clock(left)}</span>
      </span>
    </Badge>
  );
}

/** In place of the whole panel while every seat is taken. */
export function DemoQueue({ seat }: { seat: DemoSeat }) {
  const { t } = useI18n();
  const position = seat.position ?? 1;
  const waiting = seat.waiting ?? position;
  return (
    <main className="pom-login" data-testid="demo-queue">
      <section className="pom-login-panel">
        <img className="pom-login-mark" src={pomMark} alt="POM" width={815} height={320} />
        <p className="pom-login-sub">PIECE OF MIND</p>
        <span className="pom-login-link" aria-hidden="true"><i /><i /><i /></span>
        <div className="pom-login-form" aria-live="polite">
          <p className="pom-login-error" style={{ opacity: 0.9 }}>{t("demo.queueTitle")}</p>
          <p className="pom-login-error" style={{ opacity: 0.75 }}>
            {t("demo.queueDetail", { seats: seat.max_sessions ?? 0, minutes: Math.round((seat.session_secs ?? 0) / 60) })}
          </p>
          <div className="flex flex-col gap-2 py-2 text-sm text-white/85">
            <span className="flex items-center gap-2" data-testid="demo-queue-position">
              <Users className="h-4 w-4" aria-hidden="true" />
              {t("demo.queuePosition", { position, waiting })}
            </span>
            <span className="flex items-center gap-2" data-testid="demo-queue-eta">
              <Hourglass className="h-4 w-4" aria-hidden="true" />
              {t("demo.queueEta", { minutes: minutes(seat.eta_secs ?? 0) })}
            </span>
          </div>
          <p className="pom-login-error" style={{ opacity: 0.6 }}>{t("demo.queueKeepOpen")}</p>
          <button className="pom-login-button" type="button" onClick={() => void signOut()}>
            {t("demo.leaveQueue")}
          </button>
        </div>
      </section>
    </main>
  );
}

/** Once per seat: what a demonstration node is, and that the visit is timed. */
export function DemoWelcome({ seat }: { seat: DemoSeat | undefined }) {
  const { t } = useI18n();
  const key = seat?.state === "active" ? `${sessionInfo()?.user?.user_id}:${seat.expires_at}` : null;
  const [open, setOpen] = useState(false);
  useEffect(() => {
    if (!key) return;
    if (storage()?.getItem(WELCOMED_FLAG) === key) return;
    setOpen(true);
  }, [key]);
  const close = () => {
    if (key) storage()?.setItem(WELCOMED_FLAG, key);
    setOpen(false);
  };
  const total = Math.round((seat?.session_secs ?? 0) / 60);
  return (
    <Modal
      open={open}
      title={t("demo.welcomeTitle")}
      onClose={close}
      footer={<Button variant="primary" onClick={close}>{t("demo.welcomeStart")}</Button>}
    >
      <div className="space-y-3 text-sm text-muted" data-testid="demo-welcome">
        <p>{t("demo.welcomeIntro")}</p>
        <p>{t("demo.welcomeDisabled")}</p>
        <p className="font-medium text-fg">{t("demo.welcomeTime", { minutes: total })}</p>
        <p className="flex items-start gap-2 rounded-lg border border-iris/35 bg-iris/10 px-3 py-2 text-fg">
          <MessageCircle className="mt-0.5 h-4 w-4 shrink-0 text-iris" aria-hidden="true" />
          <span>
            {t("demo.welcomeDiscord")}{" "}
            <a className="font-medium text-iris underline" href={POM_DISCORD} target="_blank" rel="noreferrer" data-testid="demo-discord">
              discord.gg/ptGMZbaDkN
            </a>
          </span>
        </p>
        <p className="flex items-start gap-2 rounded-lg border border-amber/35 bg-amber/10 px-3 py-2 text-fg">
          <Star className="mt-0.5 h-4 w-4 shrink-0 text-amber" aria-hidden="true" />
          <span>
            {t("demo.welcomeStar")}{" "}
            <a className="font-medium text-iris underline" href={POM_REPOSITORY} target="_blank" rel="noreferrer" data-testid="demo-github">
              {t("demo.welcomeStarLink")}
            </a>
          </span>
        </p>
      </div>
    </Modal>
  );
}

/** The sign-in screen's notice after a visit ran out of time. */
export function DemoExpiredNotice() {
  const { t } = useI18n();
  const [expired] = useState(takeDemoExpired);
  if (!expired) return null;
  return (
    <p role="status" className="pom-login-error" data-testid="demo-expired">
      <LogOut className="mr-1 inline h-3.5 w-3.5" aria-hidden="true" />
      {t("demo.expired")}
    </p>
  );
}

/**
 * A visitor's Network screen: the network this node is in, as a listing.
 * No other network, no settings, no participants' controls.
 */
export function DemoNetwork() {
  const { t } = useI18n();
  const info = useNetworkInfo();
  const node = useNode();
  const cluster = useCluster();
  const network = info.data?.network;
  const peers = cluster.data?.peers ?? [];
  return (
    <div className="space-y-4" data-testid="demo-network">
      <Panel>
        <PanelHeader
          eyebrow={t("demo.networkEyebrow")}
          title={network?.public_name || node.data?.network_name || "—"}
          subtitle={t("demo.readOnly")}
        />
        <div className="grid gap-3 px-5 pb-5 sm:grid-cols-2">
          <KeyValue label={t("demo.networkVisibility")}>
            <span className="flex items-center gap-1"><Globe2 className="h-3.5 w-3.5" aria-hidden="true" />{network?.visibility ?? "—"}</span>
          </KeyValue>
          {network?.organization && <KeyValue label={t("demo.networkOrganization")}>{network.organization}</KeyValue>}
          {(network?.city || network?.country_name) && (
            <KeyValue label={t("demo.networkLocation")}>{[network?.city, network?.country_name].filter(Boolean).join(", ")}</KeyValue>
          )}
          <KeyValue label={t("demo.networkMachines")}>{peers.length}</KeyValue>
          {cluster.data?.active_model && <KeyValue label={t("demo.networkModel")}>{cluster.data.active_model}</KeyValue>}
        </div>
        {network?.description && <p className="px-5 pb-5 text-sm text-muted">{network.description}</p>}
      </Panel>
      {peers.length > 0 && (
        <Panel>
          <PanelHeader title={t("demo.networkMachines")} />
          <ul className="divide-y divide-line">
            {peers.map((peer) => (
              <li key={peer.node_id} className="flex flex-wrap items-center justify-between gap-2 px-5 py-3 text-sm">
                <span className="font-medium text-fg">{peerDisplayName(peer)}</span>
                <span className="font-mono text-xs text-muted">
                  {peer.model ? `${peer.model} · ${peer.start_layer}-${peer.end_layer}` : t("demo.networkNoSlice")}
                </span>
              </li>
            ))}
          </ul>
        </Panel>
      )}
    </div>
  );
}
