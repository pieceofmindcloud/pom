/**
 * Whom this machine relays for. Switches applied live by the node
 * (`/api/ui/network/relay`); an option the command line fixed for this run is
 * shown but cannot be changed here. The choices are drawn as the quick setup
 * draws its questions: the same cards in the wizard, which saves them at the
 * end, and in Network settings, which saves each change at once. Both draw
 * nothing while the internet does not reach this machine (`reachable`, from
 * the NAT report or a configured public address): behind a NAT it cannot relay for
 * anyone, so there is nothing to choose. A visitor gets a fourth choice:
 * relaying anonymously for its network (`anonymous`), where the relay admits
 * anyone holding an admin's pass and never learns who. An admin sees how
 * many visitors relay that way.
 */
import { ArrowLeftRight, Check, EyeOff, Globe2, Network } from "lucide-react";
import { useEffect, useState } from "react";
import type { ReactNode } from "react";
import { KeyValue, Panel, PanelHeader } from "./ui";
import { api } from "../lib/api";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";
import { useProfile } from "../lib/profile";

export interface RelayStatus {
  everyone: boolean;
  cluster: boolean;
  transfers: boolean;
  fixed: { everyone: boolean; no_cluster: boolean; no_transfers: boolean };
  reachable: boolean;
  serving: boolean;
  /** Relays for anyone holding a pass of its network, without knowing who. */
  anonymous: boolean;
  anonymous_serving: boolean;
  /** The visitors' anonymous relays this node holds passes for. */
  anonymous_relays: number;
}

export type RelayOption = "everyone" | "cluster" | "transfers" | "anonymous";
export type RelayChoice = Pick<RelayStatus, RelayOption>;

type Translate = (key: string, values?: Record<string, string | number>) => string;

/** What the node is doing with the options, in one line. */
export function relayStateKey(status: RelayStatus): string {
  if (status.everyone) return "network.relayStateEveryone";
  if (!status.cluster) return "network.relayStateOff";
  return status.reachable ? "network.relayStateCluster" : "network.relayStateUnreachable";
}

/** Whether this machine can act as a relay at all: the internet reaches it. */
export function canRelay(status: RelayStatus | null): status is RelayStatus {
  return Boolean(status?.reachable);
}

/** The relay status as this machine reports it, or `null` while unknown or unavailable. */
export function useRelayStatus(enabled = true) {
  const [status, setStatus] = useState<RelayStatus | null>(null);
  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    void api
      .get<RelayStatus>("/api/ui/network/relay")
      .then((loaded) => {
        if (!cancelled) setStatus(loaded);
      })
      // Without an answer there is nothing true to show; the choices stay out.
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [enabled]);
  return [status, setStatus] as const;
}

/**
 * The three relay choices as cards. Relaying for any network carries the
 * "Recommended" badge: the more machines relay, the better networks behind
 * routers reach each other.
 */
export function RelayChoices({ value, status, onChange, disabled = false, anonymousOffered = false, t }: {
  value: RelayChoice;
  /** Offer the anonymous relay (a visitor's choice). */
  anonymousOffered?: boolean;
  /** The node's report: fixed options and whether the internet reaches it. */
  status: RelayStatus;
  onChange: (option: RelayOption, checked: boolean) => void;
  disabled?: boolean;
  t: Translate;
}) {
  const rows: { option: RelayOption; icon: ReactNode; label: string; hint: string; fixed: boolean; off: boolean; badge?: string }[] = [
    { option: "everyone", icon: <Globe2 className="h-5 w-5" />, label: t("network.relayEveryone"), hint: t("network.relayEveryoneHint"), fixed: status.fixed.everyone, off: false, badge: t("quickSetup.recommended") },
    { option: "cluster", icon: <Network className="h-5 w-5" />, label: t("network.relayCluster"), hint: t("network.relayClusterHint"), fixed: status.fixed.no_cluster, off: false },
    // Nothing to allow when the machine relays for nobody of its network.
    { option: "transfers", icon: <ArrowLeftRight className="h-5 w-5" />, label: t("network.relayTransfers"), hint: t("network.relayTransfersHint"), fixed: status.fixed.no_transfers, off: !value.cluster },
    ...(anonymousOffered
      ? [{ option: "anonymous" as const, icon: <EyeOff className="h-5 w-5" />, label: t("network.relayAnonymous"), hint: t("network.relayAnonymousHint"), fixed: false, off: false }]
      : []),
  ];
  return (
    <div className="space-y-2" data-testid="relay-choices">
      {rows.map((row) => {
        const checked = value[row.option];
        const locked = disabled || row.fixed || row.off;
        return (
          <label key={row.option} className={`flex items-start gap-3 rounded-lg border px-3 py-3 transition-colors ${row.option === "transfers" ? "sm:ml-8" : ""} ${checked ? "border-iris/50 bg-iris/[0.08]" : "border-line"} ${locked ? "opacity-60" : "cursor-pointer hover:border-line-strong"}`}>
            <input type="checkbox" className="sr-only" data-testid={`relay-${row.option}`} checked={checked} disabled={locked} onChange={(event) => onChange(row.option, event.target.checked)} />
            <span className={`mt-0.5 shrink-0 ${checked ? "text-iris" : "text-faint"}`}>{row.icon}</span>
            <span className="min-w-0 flex-1">
              <span className="flex flex-wrap items-center gap-2 text-sm font-medium text-fg">{row.label}{row.badge && <span className="rounded-full border border-lime/40 px-1.5 py-px text-[0.62rem] font-normal text-lime">{row.badge}</span>}</span>
              <span className="mt-0.5 block text-xs leading-relaxed text-muted">{row.fixed ? t("network.relayFixed") : row.hint}</span>
            </span>
            <span className={`mt-1 flex h-4 w-4 shrink-0 items-center justify-center rounded border ${checked ? "border-iris bg-iris text-white" : "border-line"}`}>{checked && <Check className="h-3 w-3" />}</span>
          </label>
        );
      })}
      <KeyValue label={t("network.relayStatus")}>
        <span data-testid="relay-state">{t(relayStateKey({ ...status, ...value }))}</span>
      </KeyValue>
    </div>
  );
}

/**
 * The live editor: each change goes to the node at once. `section` replaces
 * the panel frame with the caller's own section, drawn only once the node
 * answered, so a form never shows a question with nothing under it.
 */
export function RelayOptions({ section }: { section?: (content: ReactNode) => ReactNode }) {
  const { t } = useI18n();
  const toast = useToast();
  const { profile } = useProfile();
  const [status, setStatus] = useRelayStatus();
  const [saving, setSaving] = useState(false);

  // An admin behind a NAT relays for nobody, but still learns how many
  // visitors relay anonymously for its network.
  const visitorRelayCount = status?.anonymous_relays ?? 0;
  const visitorRelays = profile === "admin" && status ? (
    <KeyValue label={t("network.relayVisitors")}>
      <span data-testid="relay-visitors">{t("network.relayVisitorsCount", { count: visitorRelayCount })}</span>
    </KeyValue>
  ) : null;
  if (!canRelay(status)) {
    if (!visitorRelays || visitorRelayCount === 0) return null;
    return section ? section(visitorRelays) : <Panel><PanelHeader title={t("network.relayTitle")} subtitle={t("network.relaySubtitle")} /><div className="px-5 py-4">{visitorRelays}</div></Panel>;
  }

  async function save(option: RelayOption, value: boolean) {
    if (!status) return;
    setSaving(true);
    try {
      const saved = await api.post<RelayStatus>("/api/ui/network/relay", {
        everyone: status.everyone,
        cluster: status.cluster,
        transfers: status.transfers,
        anonymous: status.anonymous ?? false,
        [option]: value,
      });
      setStatus(saved);
      toast.success(t("network.relaySaved"), t("network.relaySavedDetail"));
    } catch (e) {
      toast.fromError(t("network.relaySaveFailed"), e);
    } finally {
      setSaving(false);
    }
  }

  const choices = (
    <>
      <RelayChoices value={status} status={status} disabled={saving} anonymousOffered={profile === "visitor"} onChange={(option, checked) => void save(option, checked)} t={t} />
      {visitorRelays}
    </>
  );
  if (section) {
    return section(
      <div className="space-y-2">
        {choices}
        <p className="text-xs text-faint">{t("quickSetup.relayLive")}</p>
      </div>,
    );
  }
  return (
    <Panel>
      <PanelHeader title={t("network.relayTitle")} subtitle={t("network.relaySubtitle")} />
      <div className="px-5 py-4">{choices}</div>
    </Panel>
  );
}
