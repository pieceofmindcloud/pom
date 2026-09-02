/**
 * Preferências locais deste nó: proteção térmica e credencial opcional do
 * provedor Hugging Face.
 */
import { Thermometer } from "lucide-react";
import { useEffect, useState } from "react";
import { HuggingFaceSettingsPanel } from "../components/HuggingFaceSettingsPanel";
import {
  Button,
  Field,
  Footnote,
  Input,
  PageHeader,
  Panel,
  PanelHeader,
} from "../components/ui";
import { api } from "../lib/api";
import { useInvalidate, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { ThermalPolicy } from "../lib/types";
import { useI18n } from "../i18n/provider";

const DEFAULT_THERMAL_POLICY: ThermalPolicy = {
  limit_c: 85,
  hysteresis_c: 8,
  cooldown_secs: 30,
};

export function ThermalPanel({
  policy,
  onSave,
  busy = false,
}: {
  policy?: ThermalPolicy;
  onSave: (next: ThermalPolicy) => void;
  busy?: boolean;
}) {
  const { t } = useI18n();
  const [limit, setLimit] = useState(String(policy?.limit_c ?? DEFAULT_THERMAL_POLICY.limit_c));
  const [hysteresis, setHysteresis] = useState(
    String(policy?.hysteresis_c ?? DEFAULT_THERMAL_POLICY.hysteresis_c),
  );
  const [cooldown, setCooldown] = useState(
    String(policy?.cooldown_secs ?? DEFAULT_THERMAL_POLICY.cooldown_secs),
  );
  const [thermalLoaded, setThermalLoaded] = useState(false);

  useEffect(() => {
    if (thermalLoaded || !policy) return;
    setLimit(String(policy.limit_c));
    setHysteresis(String(policy.hysteresis_c));
    setCooldown(String(policy.cooldown_secs));
    setThermalLoaded(true);
  }, [policy, thermalLoaded]);

  const nextPolicy = (): ThermalPolicy => ({
    limit_c: Number(limit),
    hysteresis_c: Number(hysteresis),
    cooldown_secs: Number(cooldown),
  });

  const resumeAt = Number(limit) - Number(hysteresis);
  return (
    <Panel>
      <PanelHeader
        title={t("network.thermalTitle")}
        subtitle={t("network.thermalSubtitle")}
        actions={
          <Button
            variant="primary"
            icon={<Thermometer className="h-3.5 w-3.5" />}
            loading={busy}
            onClick={() => onSave(nextPolicy())}
          >
            {t("network.thermalSave")}
          </Button>
        }
      />
      <div className="grid gap-4 px-5 py-4 sm:grid-cols-3">
        <Field label={t("network.thermalLimitLabel")}>
          <Input type="number" min="1" max="150" step="0.5" value={limit} onChange={(e) => setLimit(e.target.value)} />
        </Field>
        <Field label={t("network.thermalHysteresisLabel")}>
          <Input type="number" min="0.1" max="149" step="0.5" value={hysteresis} onChange={(e) => setHysteresis(e.target.value)} />
        </Field>
        <Field label={t("network.thermalCooldownLabel")}>
          <Input type="number" min="1" step="1" value={cooldown} onChange={(e) => setCooldown(e.target.value)} />
        </Field>
        <Footnote className="sm:col-span-3" icon={<Thermometer className="h-3.5 w-3.5" />}>
          {t("network.thermalHint")} {Number.isFinite(resumeAt) && ` ${t("network.thermalResumeAt", { value: resumeAt })}`}
        </Footnote>
      </div>
    </Panel>
  );
}

export function Preferences() {
  const { t } = useI18n();
  const node = useNode();
  const toast = useToast();
  const invalidate = useInvalidate();
  const [thermalSaving, setThermalSaving] = useState(false);

  async function saveThermal(next: ThermalPolicy) {
    setThermalSaving(true);
    try {
      await api.post<ThermalPolicy>("/api/ui/thermal", next);
      toast.success(t("network.thermalSaved"), t("network.thermalSavedDetail"));
      invalidate();
    } catch (e) {
      toast.fromError(t("network.thermalSaveFailed"), e);
    } finally {
      setThermalSaving(false);
    }
  }

  return (
    <>
      <PageHeader
        title={t("preferences.title")}
        subtitle={t("preferences.subtitle")}
      />
      <div className="space-y-5">
        <ThermalPanel
          policy={node.data?.thermal}
          busy={thermalSaving}
          onSave={(next) => void saveThermal(next)}
        />
        <HuggingFaceSettingsPanel />
      </div>
    </>
  );
}
