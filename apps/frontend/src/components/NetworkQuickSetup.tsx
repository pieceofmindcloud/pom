/**
 * Quick setup of this machine's own network, opened from the Networks screen.
 *
 * A new installation starts with discovery off, and the full Network settings
 * screen asks everything at once. This window walks the principal through the
 * few choices that publish the network - discovery, what people read about
 * it, who may join and where it is - one step at a time, and saves through the
 * same `POST /api/ui/network/settings` the settings screen uses. The node
 * stays the authority: it validates the place and refuses a caller that is
 * not the principal. Its relay step is this machine's own setting
 * (`/api/ui/network/relay`), asked here to encourage relaying for any
 * network; it is saved after the network, and only when it changed.
 */
import { Check, ChevronLeft, ChevronRight, EyeOff, Globe2, Monitor, MapPin, MessagesSquare, Rocket, Server, ShieldAlert, ShieldQuestion, Users, WifiOff } from "lucide-react";
import { useEffect, useState } from "react";
import type { ReactNode } from "react";
import { Modal } from "./Confirm";
import { LocationEditor } from "./LocationEditor";
import { canRelay, RelayChoices, relayStateKey, useRelayStatus, type RelayChoice, type RelayStatus } from "./RelayOptions";
import { Button, Field, Input } from "./ui";
import { api } from "../lib/api";
import { NETWORK_GEO, placeLabel } from "../lib/geo";
import { ENROLLMENT_ROLES } from "../lib/types";
import type { EnrollmentRole, NetworkInfoView, NetworkSettings } from "../lib/types";

type Translate = (key: string, values?: Record<string, string | number>) => string;
type Step = "discovery" | "info" | "access" | "location" | "relay" | "review";
type Update = (patch: Partial<NetworkSettings>) => void;

/** The icon of each discovery choice, the same on the Networks screen and in Network settings. */
export const VISIBILITY_ICONS: Record<NetworkSettings["visibility"], typeof Globe2> = {
  public: Globe2,
  private: EyeOff,
  local: WifiOff,
};

/** The short name of each discovery choice. */
export const VISIBILITY_LABELS: Record<NetworkSettings["visibility"], string> = {
  public: "quickSetup.discoveryPublic",
  private: "quickSetup.discoveryHidden",
  local: "quickSetup.discoveryOff",
};

/** The icon of each role a machine can ask to join with. */
export const ENROLLMENT_ICONS: Record<EnrollmentRole, typeof Server> = {
  node: Server,
  infer: MessagesSquare,
};

/**
 * The steps a draft goes through: a network nobody can reach skips who may
 * join and where it is. The relay step only shows on a machine the internet
 * reaches: behind a NAT it cannot relay for anyone.
 */
export function quickSetupSteps(visibility: NetworkSettings["visibility"], relay = true): Step[] {
  const steps: Step[] = visibility === "local"
    ? ["discovery", "info"]
    : visibility === "private"
      ? ["discovery", "info", "access"]
      : ["discovery", "info", "access", "location"];
  return [...steps, ...(relay ? ["relay" as const] : []), "review"];
}

/** The wizard's suggestion: relaying for any network, unless the command line fixed it. */
function suggestedRelay(status: RelayStatus): RelayChoice {
  return {
    everyone: status.fixed.everyone ? status.everyone : true,
    cluster: status.cluster,
    transfers: status.transfers,
  };
}

export function NetworkQuickSetup({ open, network, onClose, onSaved, t }: {
  open: boolean;
  /** The settings as the node has them now; the window starts from them. */
  network: NetworkSettings | null;
  onClose: () => void;
  /** `relayFailed`: the network was saved but this machine's relay choice was not. */
  onSaved: (saved: NetworkInfoView, relayFailed: boolean) => void;
  t: Translate;
}) {
  const [draft, setDraft] = useState<NetworkSettings | null>(null);
  const [step, setStep] = useState<Step>("discovery");
  const [saving, setSaving] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [relayStatus] = useRelayStatus(open);
  const [relay, setRelay] = useState<RelayChoice | null>(null);

  // Every opening starts over from what the node has, suggesting the public
  // directory: publishing is what this window is for.
  useEffect(() => {
    if (!open || !network) return;
    setDraft({
      ...network,
      visibility: network.visibility === "local" ? "public" : network.visibility,
      access_policy: network.access_policy === "local_only" ? "authorized" : network.access_policy,
      accepted_enrollments: network.accepted_enrollments.length ? network.accepted_enrollments : [...ENROLLMENT_ROLES],
    });
    setStep("discovery");
    setFailure(null);
  }, [open, network]);

  useEffect(() => {
    setRelay(open && canRelay(relayStatus) ? suggestedRelay(relayStatus) : null);
  }, [open, relayStatus]);

  if (!open || !draft) return null;
  const update = (patch: Partial<NetworkSettings>) => setDraft({ ...draft, ...patch });
  const steps = quickSetupSteps(draft.visibility, Boolean(relayStatus && relay));
  const index = Math.max(0, steps.indexOf(step));
  const last = index === steps.length - 1;
  const publicNetwork = draft.visibility === "public";
  const blocked = step === "info" && publicNetwork && !draft.public_name.trim()
    ? t("quickSetup.nameRequired")
    : step === "location" && draft.city_id == null
      ? t("location.required")
      : null;

  async function save() {
    if (!draft) return;
    setSaving(true);
    setFailure(null);
    try {
      const saved = await api.post<NetworkInfoView>("/api/ui/network/settings", draft);
      let relayFailed = false;
      if (relayStatus && relay && (["everyone", "cluster", "transfers"] as const).some((option) => relay[option] !== relayStatus[option])) {
        try {
          await api.post<RelayStatus>("/api/ui/network/relay", relay);
        } catch {
          // The network is saved either way; the relay can be set in Network settings.
          relayFailed = true;
        }
      }
      onSaved(saved, relayFailed);
    } catch (reason) {
      setFailure(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setSaving(false);
    }
  }

  const footer = (
    <div className="flex w-full items-center justify-between gap-2">
      <Button variant="ghost" onClick={onClose} disabled={saving}>{t("common.cancel")}</Button>
      <div className="flex items-center gap-2">
        {index > 0 && <Button onClick={() => setStep(steps[index - 1])} disabled={saving} icon={<ChevronLeft className="h-3.5 w-3.5" />}>{t("common.previous")}</Button>}
        {last
          ? <Button variant="primary" loading={saving} onClick={() => void save()} icon={<Rocket className="h-3.5 w-3.5" />}>{t(publicNetwork ? "quickSetup.publish" : "quickSetup.save")}</Button>
          : <Button variant="primary" disabled={Boolean(blocked)} title={blocked ?? undefined} onClick={() => setStep(steps[index + 1])}>{t("common.next")}<ChevronRight className="h-3.5 w-3.5" /></Button>}
      </div>
    </div>
  );

  return (
    <Modal open size="xl" title={t("quickSetup.title")} onClose={() => !saving && onClose()} footer={footer}>
      <div className="space-y-5" data-testid="network-quick-setup">
        <ol className="flex flex-wrap items-center gap-1.5" aria-label={t("quickSetup.progress")}>
          {steps.map((name, position) => (
            <li key={name} aria-current={name === step ? "step" : undefined} className={`flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-[0.7rem] ${name === step ? "border-iris/50 bg-iris/15 text-fg" : position < index ? "border-lime/30 text-lime" : "border-line text-faint"}`}>
              {position < index ? <Check className="h-3 w-3" /> : <span className="font-mono">{position + 1}</span>}
              {t(`quickSetup.step.${name}`)}
            </li>
          ))}
        </ol>
        <div>
          <h3 className="font-display text-base font-semibold text-fg">{t(`quickSetup.heading.${step}`)}</h3>
          <p className="mt-1 text-sm text-muted">{t(`quickSetup.lead.${step}`)}</p>
        </div>
        {step === "discovery" && <DiscoveryChoices draft={draft} update={update} t={t} />}
        {step === "info" && <InfoFields draft={draft} update={update} t={t} autoFocus />}
        {step === "access" && <AccessFields draft={draft} update={update} t={t} />}
        {step === "location" && <LocationEditor value={draft} source={NETWORK_GEO} required t={t} onChange={(place) => update(place)} />}
        {step === "relay" && relayStatus && relay && <RelayChoices value={relay} status={relayStatus} onChange={(option, checked) => setRelay({ ...relay, [option]: checked })} t={t} />}
        {step === "review" && (
          <dl className="grid gap-x-6 gap-y-3 rounded-lg border border-line bg-raised/40 p-4 sm:grid-cols-2" data-testid="quick-setup-review">
            <Summary label={t("network.visibility")}>{t(VISIBILITY_LABELS[draft.visibility])}</Summary>
            <Summary label={t("network.publicName")}>{draft.public_name || t("network.notProvided")}</Summary>
            <Summary label={t("network.organization")}>{draft.organization || t("network.notProvided")}</Summary>
            {draft.visibility !== "local" && <Summary label={t("network.accessPolicy")}>{t(draft.access_policy === "auto_approve" ? "network.accessAutoApprove" : draft.access_policy === "local_only" ? "network.accessLocalOnly" : "network.accessAuthorized")}</Summary>}
            {draft.visibility !== "local" && (
              <Summary label={t("network.acceptedEnrollments")}>
                {draft.accepted_enrollments.length === 0
                  ? t("network.enrollmentClosed")
                  : <span className="flex flex-wrap gap-x-3 gap-y-1">{draft.accepted_enrollments.map((role) => { const Icon = ENROLLMENT_ICONS[role]; return <span key={role} className="flex items-center gap-1"><Icon className="h-3.5 w-3.5 text-lime" />{t(`network.enrollment.${role}`)}</span>; })}</span>}
              </Summary>
            )}
            {publicNetwork && <Summary label={t("location.title")}><span className="flex items-center gap-1"><MapPin className="h-3.5 w-3.5 text-faint" />{placeLabel(draft) || t("network.notProvided")}</span></Summary>}
            {publicNetwork && <Summary label={t("network.publicStats")}>{t(draft.public_stats ? "network.publicStatsOn" : "network.publicStatsOff")}</Summary>}
            {relayStatus && relay && <Summary label={t("network.relayTitle")}>{t(relayStateKey({ ...relayStatus, ...relay }))}</Summary>}
          </dl>
        )}
        {step === "review" && <p className="text-xs text-faint">{t(publicNetwork ? "quickSetup.publishNote" : "quickSetup.saveNote")}</p>}
        {failure && <p role="alert" className="rounded-md border border-rose/30 bg-rose/[0.06] px-3 py-2 text-xs text-rose">{t("network.infoSaveFailed")}: {failure}</p>}
      </div>
    </Modal>
  );
}

/** A heading and lead for one question, the same words the wizard uses for its step. */
export function SetupSection({ step, number, t, children }: { step: Exclude<Step, "review">; number: number; t: Translate; children: ReactNode }) {
  return (
    <section className="min-w-0 space-y-3" data-testid={`network-section-${step}`}>
      <div className="flex items-start gap-3">
        <span className="mt-0.5 flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-iris/40 bg-iris/10 font-mono text-[0.7rem] text-iris">{number}</span>
        <div className="min-w-0">
          <h3 className="font-display text-base font-semibold text-fg">{t(`quickSetup.heading.${step}`)}</h3>
          <p className="mt-0.5 text-sm text-muted">{t(`quickSetup.lead.${step}`)}</p>
        </div>
      </div>
      <div className="min-w-0 sm:pl-9">{children}</div>
    </section>
  );
}

export function DiscoveryChoices({ draft, update, t }: { draft: NetworkSettings; update: Update; t: Translate }) {
  return (
    <div className="grid gap-2" role="radiogroup" aria-label={t("network.visibility")}>
      <Choice selected={draft.visibility === "public"} onSelect={() => update({ visibility: "public" })} icon={<Globe2 className="h-5 w-5" />} title={t("quickSetup.discoveryPublic")} detail={t("quickSetup.discoveryPublicDetail")} badge={t("quickSetup.recommended")} />
      <Choice selected={draft.visibility === "private"} onSelect={() => update({ visibility: "private" })} icon={<EyeOff className="h-5 w-5" />} title={t("quickSetup.discoveryHidden")} detail={t("quickSetup.discoveryHiddenDetail")} />
      <Choice selected={draft.visibility === "local"} onSelect={() => update({ visibility: "local" })} icon={<WifiOff className="h-5 w-5" />} title={t("quickSetup.discoveryOff")} detail={t("quickSetup.discoveryOffDetail")} />
    </div>
  );
}

export function InfoFields({ draft, update, t, autoFocus = false }: { draft: NetworkSettings; update: Update; t: Translate; autoFocus?: boolean }) {
  const publicNetwork = draft.visibility === "public";
  return (
    <div className="grid gap-4 md:grid-cols-2">
      <Field label={t("network.publicName")} hint={publicNetwork ? t("network.publicNameRequired") : undefined}>
        <Input autoFocus={autoFocus} required={publicNetwork} value={draft.public_name} onChange={(event) => update({ public_name: event.target.value })} />
      </Field>
      <Field label={t("network.organization")} hint={t("quickSetup.optional")}>
        <Input value={draft.organization} onChange={(event) => update({ organization: event.target.value })} />
      </Field>
      <Field label={t("network.description")} hint={t("quickSetup.descriptionHint")} className="md:col-span-2">
        <TextArea rows={4} value={draft.description} onChange={(description) => update({ description })} />
      </Field>
      <Field label={t("network.rules")} hint={t("quickSetup.rulesHint")} className="md:col-span-2">
        <TextArea rows={3} value={draft.rules} onChange={(rules) => update({ rules })} />
      </Field>
      <Field label={t("network.channels")} hint={t("quickSetup.optional")} className="md:col-span-2">
        <Input value={draft.channels} onChange={(event) => update({ channels: event.target.value })} />
      </Field>
    </div>
  );
}

/**
 * Who may join and with which roles. `full` also offers "This machine only"
 * and keeps the legacy open mode visible on a network already on it, so
 * saving another field never changes the policy behind the operator's back.
 */
export function AccessFields({ draft, update, t, full = false }: { draft: NetworkSettings; update: Update; t: Translate; full?: boolean }) {
  const publicNetwork = draft.visibility === "public";
  return (
    <div className="space-y-5">
      <div className="grid gap-2" role="radiogroup" aria-label={t("network.accessPolicy")}>
        <Choice selected={draft.access_policy === "authorized"} onSelect={() => update({ access_policy: "authorized" })} icon={<Users className="h-5 w-5" />} title={t("network.accessAuthorized")} detail={t("quickSetup.accessAuthorizedDetail")} badge={t("quickSetup.recommended")} />
        <Choice selected={draft.access_policy === "auto_approve"} onSelect={() => update({ access_policy: "auto_approve" })} icon={<ShieldAlert className="h-5 w-5" />} title={t("network.accessAutoApprove")} detail={t("network.accessAutoApproveWarning")} warning />
        {full && <Choice selected={draft.access_policy === "local_only"} onSelect={() => update({ access_policy: "local_only" })} icon={<Monitor className="h-5 w-5" />} title={t("network.accessLocalOnly")} detail={t("quickSetup.accessLocalOnlyDetail")} />}
        {full && draft.access_policy === "anyone_with_anchor" && <Choice selected onSelect={() => undefined} icon={<ShieldQuestion className="h-5 w-5" />} title={t("network.accessAnyoneWithAnchor")} detail={t("quickSetup.accessLegacyDetail")} warning />}
      </div>
      <section>
        <p className="eyebrow mb-1.5">{t("network.acceptedEnrollments")}</p>
        <div className="grid gap-2 sm:grid-cols-2">
          {ENROLLMENT_ROLES.map((role) => {
            const Icon = ENROLLMENT_ICONS[role];
            const checked = draft.accepted_enrollments.includes(role);
            return (
              <label key={role} className={`flex cursor-pointer items-center gap-3 rounded-lg border px-3 py-2.5 ${checked ? "border-lime/40 bg-lime/[0.06]" : "border-line"}`}>
                <input type="checkbox" checked={checked} onChange={(event) => update({ accepted_enrollments: ENROLLMENT_ROLES.filter((item) => item === role ? event.target.checked : draft.accepted_enrollments.includes(item)) })} />
                <Icon className={`h-4 w-4 shrink-0 ${checked ? "text-lime" : "text-faint"}`} />
                <span className="min-w-0">
                  <span className="block text-sm text-fg">{t(`network.enrollment.${role}`)}</span>
                  <span className="block text-[0.7rem] text-faint">{t(`quickSetup.role.${role}`)}</span>
                </span>
              </label>
            );
          })}
        </div>
        <p className="mt-1.5 text-xs text-faint">{t("network.acceptedEnrollmentsHint")}</p>
      </section>
      {publicNetwork && (
        <label className="flex items-start gap-3 rounded-lg border border-line px-3 py-2.5">
          <input type="checkbox" className="mt-1" checked={Boolean(draft.public_stats)} onChange={(event) => update({ public_stats: event.target.checked })} />
          <span>
            <span className="block text-sm text-fg">{t("network.publicStatsEnable")}</span>
            <span className="block text-xs text-faint">{t("network.publicStatsHint")}</span>
          </span>
        </label>
      )}
    </div>
  );
}

function Choice({ selected, onSelect, icon, title, detail, badge, warning = false }: { selected: boolean; onSelect: () => void; icon: ReactNode; title: string; detail: string; badge?: string; warning?: boolean }) {
  const tone = selected ? (warning ? "border-amber/50 bg-amber/[0.07]" : "border-iris/50 bg-iris/[0.08]") : "border-line hover:border-line-strong";
  return (
    <button type="button" role="radio" aria-checked={selected} onClick={onSelect} className={`flex w-full items-start gap-3 rounded-lg border px-3 py-3 text-left transition-colors ${tone}`}>
      <span className={`mt-0.5 shrink-0 ${selected ? (warning ? "text-amber" : "text-iris") : "text-faint"}`}>{icon}</span>
      <span className="min-w-0 flex-1">
        <span className="flex flex-wrap items-center gap-2 text-sm font-medium text-fg">{title}{badge && <span className="rounded-full border border-lime/40 px-1.5 py-px text-[0.62rem] font-normal text-lime">{badge}</span>}</span>
        <span className="mt-0.5 block text-xs leading-relaxed text-muted">{detail}</span>
      </span>
      <span className={`mt-1 flex h-4 w-4 shrink-0 items-center justify-center rounded-full border ${selected ? "border-iris bg-iris text-white" : "border-line"}`}>{selected && <Check className="h-3 w-3" />}</span>
    </button>
  );
}

function TextArea({ value, onChange, rows }: { value: string; onChange: (value: string) => void; rows: number }) {
  return <textarea rows={rows} value={value} onChange={(event) => onChange(event.target.value)} className="w-full min-w-0 resize-y rounded-lg border border-line bg-ink px-3 py-2 text-sm text-fg placeholder:text-faint hover:border-line-strong focus:border-iris focus:outline-none focus:ring-2 focus:ring-iris/25" />;
}

function Summary({ label, children }: { label: string; children: ReactNode }) {
  return <div className="min-w-0"><dt className="eyebrow mb-0.5">{label}</dt><dd className="break-words text-sm text-fg">{children}</dd></div>;
}
