import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { ArrowLeft, Dices, TriangleAlert } from "lucide-react";
import clsx from "clsx";
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

const STEP_LABEL: Record<WizardStepId, string> = {
  names: "firstWizard.stepNames",
  bootstrap: "firstWizard.stepGuide",
};

/**
 * Which wizard screen a machine on its first run is due, if any. `loading`
 * holds the paint: guessing would flash the panel before the wizard.
 */
export function useFirstRunWizard(enabled: boolean): { loading: boolean; step: WizardStepId | null } {
  const guide = useGuide(enabled);
  const step = enabled ? currentGuideStep(guide.data) : null;
  return { loading: enabled && guide.isLoading, step: isWizardStep(step) ? step : null };
}

/**
 * The first run wizard: a machine that has just created its own network asks
 * for the machine's name and the network's on one screen, and hands over to
 * the hints (`FirstRunGuide`). There is nothing to choose about the network:
 * it is the machine's own, and joining another one comes later, from the
 * Networks screen. Each name saves through the route its settings screen
 * uses.
 */
export function FirstRunWizard({ step }: { step: WizardStepId }) {
  const { t } = useI18n();
  const node = useNode();
  const actions = useGuideActions();
  const invalidate = useInvalidate();
  // Going back is local: the node only knows how far the operator got.
  const [stage, setStage] = useState<WizardStepId>(step);
  useEffect(() => setStage(step), [step]);

  const [machineName, setMachineName] = useState<string | null>(null);
  const [network, setNetwork] = useState<NetworkInfoView["network"] | null>(null);
  const [networkName, setNetworkName] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Filled once: the machine goes by its hostname until it is given a name.
  useEffect(() => {
    if (machineName !== null || !node.data) return;
    setMachineName(node.data.node_name?.trim() || node.data.hostname || "");
  }, [node.data, machineName]);

  useEffect(() => {
    let cancelled = false;
    api
      .get<NetworkInfoView>("/api/ui/network/info")
      .then((info) => {
        if (cancelled) return;
        setNetwork(info.network);
        setNetworkName((known) => known ?? info.network.public_name);
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

  const index = WIZARD_STEPS.indexOf(stage);
  const machine = (machineName ?? "").trim();
  const networkTrimmed = (networkName ?? "").trim();

  let body: ReactNode;
  let submit: () => void;
  let ready: boolean;
  if (stage === "names") {
    ready = machine.length > 0 && networkTrimmed.length > 0 && network !== null;
    // The machine first: a network save that fails leaves nothing half named
    // that the operator cannot see, both fields stay on the screen.
    submit = () =>
      void run(async () => {
        await api.post("/api/ui/network/node-name", { node_name: machine });
        const saved = await api.post<NetworkInfoView>("/api/ui/network/settings", { ...network, public_name: networkTrimmed });
        setNetwork(saved.network);
      }, "names");
    body = (
      <>
        <h1 className="font-display text-xl font-semibold text-fg">{t("firstWizard.namesTitle")}</h1>
        <p className="mt-2 text-sm leading-relaxed text-muted">{t("firstWizard.namesDetail")}</p>
        <NameField testId="wizard-machine-name" label={t("firstWizard.machineLabel")} hint={t("firstWizard.machineDetail")} value={machineName ?? ""} onChange={setMachineName} autoFocus t={t} />
        <NameField testId="wizard-network-name" label={t("firstWizard.networkLabel")} hint={t("firstWizard.networkDetail")} value={networkName ?? ""} onChange={setNetworkName} t={t} />
        <p className="mt-4 flex items-start gap-2 rounded-xl border border-amber/40 bg-amber/10 px-3 py-2.5 text-xs leading-relaxed text-amber" role="note">
          <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
          <span>{t("firstWizard.networkWarning")}</span>
        </p>
      </>
    );
  } else {
    ready = true;
    submit = () => void run(async () => undefined, "bootstrap");
    body = (
      <>
        <h1 className="font-display text-xl font-semibold text-fg">{t("firstWizard.guideTitle")}</h1>
        <p className="mt-2 text-sm leading-relaxed text-muted">{t("firstWizard.guideDetail")}</p>
      </>
    );
  }

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
          <nav aria-label={t("wizard.progressLabel")} className="pom-setup-trail">
            <ol>
              {WIZARD_STEPS.map((id, position) => (
                <li key={id} className={clsx(position < index && "is-done", position === index && "is-active")} aria-current={position === index ? "step" : undefined}>
                  <i />
                  <span>{t(STEP_LABEL[id])}</span>
                </li>
              ))}
            </ol>
          </nav>
        </header>
        <form onSubmit={onSubmit}>
          <div className="min-h-[200px]">
            {body}
            {error && <p className="pom-login-error mt-3" role="alert">{error}</p>}
          </div>
          <footer className="pom-setup-footer">
            <button
              type="button"
              className={clsx("pom-setup-ghost", index === 0 && "invisible")}
              onClick={() => {
                setError(null);
                setStage(WIZARD_STEPS[index - 1]);
              }}
              disabled={index === 0 || busy}
              aria-hidden={index === 0}
            >
              <ArrowLeft className="h-4 w-4" />
              {t("wizard.back")}
            </button>
            <button type="submit" className="pom-login-button" disabled={busy || !ready}>
              {busy && <span className="pom-login-spinner" aria-hidden="true" />}
              {t(stage === "bootstrap" ? "firstWizard.start" : "firstWizard.continue")}
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
