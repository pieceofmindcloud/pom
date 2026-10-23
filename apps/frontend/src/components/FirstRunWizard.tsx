import { useEffect, useState, type CSSProperties, type FormEvent } from "react";
import { Dices, TriangleAlert } from "lucide-react";
import { api } from "../lib/api";
import { currentGuideStep, isWizardStep, useGuide, useGuideActions, WIZARD_STEPS, type WizardStepId } from "../lib/guide";
import { useInvalidate, useNode } from "../lib/queries";
import { randomSlug } from "../lib/randomName";
import type { NetworkInfoView } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { BrandLanguageSwitch } from "./BrandLanguageSwitch";
import pomMark from "../assets/pom-mark-dark.png";

/** The longest name the node takes for a machine, and what a network gets at creation. */
const NAME_MAX = 64;

/**
 * Which wizard screen a machine on its first run is due, if any. `loading`
 * holds the paint: guessing would flash the panel before the wizard.
 */
export function useFirstRunWizard(enabled: boolean): { loading: boolean; step: WizardStepId | null } {
  const guide = useGuide(enabled);
  const step = enabled ? currentGuideStep(guide.data) : null;
  return { loading: enabled && guide.isLoading, step: isWizardStep(step) ? step : null };
}

/** How long the welcome stays before the hints take over. */
export const WELCOME_MS = 3600;

/**
 * The first run wizard: a machine that has just created its own network asks
 * for one name, given to the machine and to that network alike, then greets
 * the operator and hands over to the hints (`FirstRunGuide`). There is nothing
 * to choose about the network: it is the machine's own, and joining another
 * one comes later, from the Networks screen. The name saves through the routes
 * the two settings screens use, so either can be renamed there later.
 */
export function FirstRunWizard({ step, welcomeMs = WELCOME_MS }: { step: WizardStepId; welcomeMs?: number }) {
  const { t } = useI18n();
  const node = useNode();
  const actions = useGuideActions();
  const invalidate = useInvalidate();
  const [stage, setStage] = useState<WizardStepId>(step);
  useEffect(() => setStage(step), [step]);

  const [name, setName] = useState<string | null>(null);
  const [network, setNetwork] = useState<NetworkInfoView["network"] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Filled once: the machine goes by its hostname until it is given a name.
  useEffect(() => {
    if (name !== null || !node.data) return;
    setName(node.data.node_name?.trim() || node.data.hostname || "");
  }, [node.data, name]);

  useEffect(() => {
    let cancelled = false;
    api
      .get<NetworkInfoView>("/api/ui/network/info")
      .then((info) => {
        if (!cancelled) setNetwork(info.network);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, []);

  async function run(save: () => Promise<void>, done: WizardStepId) {
    setBusy(true);
    setError(null);
    try {
      await save();
      await actions.complete(done);
      invalidate();
      const next = WIZARD_STEPS[WIZARD_STEPS.indexOf(done) + 1];
      if (next) setStage(next);
    } catch (e) {
      setError(t("firstWizard.failed", { error: e instanceof Error ? e.message : String(e) }));
    } finally {
      setBusy(false);
    }
  }

  // The welcome ends by itself; a failed save keeps it with a button to retry.
  const welcome = stage === "bootstrap";
  useEffect(() => {
    if (!welcome) return;
    const timer = window.setTimeout(() => void run(async () => undefined, "bootstrap"), welcomeMs);
    return () => window.clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [welcome, welcomeMs]);

  if (welcome) {
    return (
      <main className="pom-login pom-setup" data-testid="first-run-wizard" data-wizard-step={stage}>
        <section className="pom-login-panel pom-welcome" data-testid="wizard-welcome" style={{ "--pom-welcome-ms": `${welcomeMs}ms` } as CSSProperties}>
          <img className="pom-login-mark pom-welcome-mark" src={pomMark} alt="POM" width={815} height={320} />
          <h1 className="pom-welcome-title">{t("firstWizard.welcomeTitle")}</h1>
          <div className="pom-login-link pom-welcome-link" aria-hidden="true">
            <i />
            <i />
            <i />
          </div>
          <p className="pom-welcome-detail">{t("firstWizard.welcomeDetail")}</p>
          {error ? (
            <>
              <p className="pom-login-error mt-3" role="alert">{error}</p>
              <button type="button" className="pom-login-button mt-4" disabled={busy} onClick={() => void run(async () => undefined, "bootstrap")}>
                {busy && <span className="pom-login-spinner" aria-hidden="true" />}
                {t("firstWizard.start")}
              </button>
            </>
          ) : (
            <span className="pom-welcome-progress" aria-hidden="true" />
          )}
        </section>
      </main>
    );
  }

  const trimmed = (name ?? "").trim();
  const ready = trimmed.length > 0 && network !== null;
  // The machine first: a network save that fails leaves the field on the
  // screen, and saving again names both.
  const submit = () =>
    void run(async () => {
      await api.post("/api/ui/network/node-name", { node_name: trimmed });
      const saved = await api.post<NetworkInfoView>("/api/ui/network/settings", { ...network, public_name: trimmed });
      setNetwork(saved.network);
    }, "names");

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    if (ready && !busy) submit();
  }

  return (
    <main className="pom-login pom-setup" data-testid="first-run-wizard" data-wizard-step={stage}>
      <BrandLanguageSwitch />
      <section className="pom-login-panel pom-setup-panel pom-setup-scope">
        <header className="pom-setup-brand">
          <img className="pom-login-mark pom-setup-mark" src={pomMark} alt="POM" width={815} height={320} />
          <p className="pom-login-sub pom-setup-sub">PIECE OF MIND</p>
        </header>
        <form onSubmit={onSubmit}>
          <div className="min-h-[200px]">
            <h1 className="font-display text-xl font-semibold text-fg">{t("firstWizard.namesTitle")}</h1>
            <p className="mt-2 text-sm leading-relaxed text-muted">{t("firstWizard.namesDetail")}</p>
            <NameField testId="wizard-name" label={t("firstWizard.nameLabel")} hint={t("firstWizard.nameDetail")} value={name ?? ""} onChange={setName} autoFocus t={t} />
            <p className="mt-4 flex items-start gap-2 rounded-xl border border-amber/40 bg-amber/10 px-3 py-2.5 text-xs leading-relaxed text-amber" role="note">
              <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
              <span>{t("firstWizard.networkWarning")}</span>
            </p>
            {error && <p className="pom-login-error mt-3" role="alert">{error}</p>}
          </div>
          <footer className="pom-setup-footer">
            <span />
            <button type="submit" className="pom-login-button" disabled={busy || !ready}>
              {busy && <span className="pom-login-spinner" aria-hidden="true" />}
              {t("firstWizard.continue")}
            </button>
          </footer>
        </form>
      </section>
    </main>
  );
}

function NameField({ testId, label, hint, value, onChange, autoFocus, t }: {
  testId: string;
  label: string;
  hint: string;
  value: string;
  onChange: (value: string) => void;
  autoFocus?: boolean;
  t: (key: string) => string;
}) {
  return (
    <>
      <label className="mt-5 block text-xs font-semibold uppercase tracking-wider text-muted" htmlFor={testId}>
        {label}
      </label>
      <div className="mt-2 flex items-stretch gap-2">
        <input
          id={testId}
          data-testid={testId}
          className="pom-login-input min-w-0 flex-1 !text-left"
          value={value}
          maxLength={NAME_MAX}
          autoFocus={autoFocus}
          autoComplete="off"
          spellCheck={false}
          onChange={(event) => onChange(event.target.value)}
        />
        <button type="button" className="pom-setup-ghost shrink-0" title={t("firstWizard.randomTitle")} aria-label={`${label}: ${t("firstWizard.randomTitle")}`} onClick={() => onChange(randomSlug())}>
          <Dices className="h-4 w-4" />
          {t("firstWizard.random")}
        </button>
      </div>
      <p className="mt-1.5 text-xs leading-relaxed text-faint">{hint}</p>
    </>
  );
}
