import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import { ArrowLeft, ArrowRight } from "lucide-react";
import type { ReactNode } from "react";
import { useI18n } from "../../i18n/provider";
import { LanguageFlags } from "../LanguageFlags";
import { Button } from "../ui";
import type { WizardStage, WizardStep } from "../../routes/setupWizardTypes";

export function WizardLayout({
  stage,
  steps,
  children,
  canGoBack,
  onBack,
  footer,
}: {
  stage: WizardStage;
  steps: WizardStep[];
  children: ReactNode;
  canGoBack: boolean;
  onBack: () => void;
  footer?: ReactNode;
}) {
  const { t } = useI18n();
  const activeIndex = Math.max(0, steps.findIndex((step) => step.id === stage));
  const activeStep = steps[activeIndex];
  return (
    <div className="flex h-full min-h-0 flex-col bg-ink">
      <div className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden">
        <div className="mx-auto flex min-h-full w-full max-w-[1100px] flex-col px-4 py-6 sm:px-6 sm:py-8 lg:px-8">
          <nav aria-label={t("wizard.progressLabel")} className="mb-7">
            <div className="flex items-center justify-between gap-3">
              <p className="eyebrow">{t("wizard.stepCount", { current: activeIndex + 1, total: steps.length })}</p>
              <p className="truncate text-xs text-muted">{activeStep ? t(activeStep.labelKey) : t("wizard.welcome")}</p>
            </div>
            <ol className="mt-3 flex items-center gap-1.5" aria-label={t("wizard.progressLabel")}>
              {steps.map((step, index) => (
                <li key={step.id} className="flex min-w-0 flex-1 items-center gap-1.5">
                  <span
                    aria-current={step.id === stage ? "step" : undefined}
                    title={t(step.labelKey)}
                    className={clsx(
                      "h-1.5 min-w-0 flex-1 rounded-full transition-colors duration-200",
                      index <= activeIndex ? "bg-iris" : "bg-line",
                    )}
                  />
                </li>
              ))}
            </ol>
          </nav>

          <AnimatePresence mode="wait" initial={false}>
            <motion.main
              key={stage}
              initial={{ opacity: 0, y: 8 }}
              animate={{ opacity: 1, y: 0 }}
              exit={{ opacity: 0, y: -6 }}
              transition={{ duration: 0.18, ease: [0.22, 1, 0.36, 1] }}
              className="min-w-0 flex-1"
            >
              {children}
            </motion.main>
          </AnimatePresence>

          <div className="mt-8 flex flex-wrap items-center justify-between gap-3 border-t border-line pt-4">
            <div className="flex min-w-0 items-center gap-3">
              <LanguageFlags />
              <Button
                variant="ghost"
                icon={<ArrowLeft className="h-4 w-4" />}
                onClick={onBack}
                disabled={!canGoBack}
                aria-hidden={!canGoBack}
                className={!canGoBack ? "invisible" : ""}
              >
                {t("wizard.back")}
              </Button>
            </div>
            <div className="flex min-w-0 flex-wrap justify-end gap-2">
              {footer}
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}

export function ContinueButton({
  onClick,
  label = "wizard.continue",
  disabled = false,
  loading = false,
}: {
  onClick: () => void;
  label?: string;
  disabled?: boolean;
  loading?: boolean;
}) {
  const { t } = useI18n();
  return (
    <Button
      variant="primary"
      icon={<ArrowRight className="h-4 w-4" />}
      onClick={onClick}
      disabled={disabled}
      loading={loading}
    >
      {t(label)}
    </Button>
  );
}
