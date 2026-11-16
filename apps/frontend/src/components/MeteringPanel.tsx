/**
 * GPU work metering on the Network screen: the principal turns it on or off
 * at run time and sets the payout cycle, the grace before a cycle closes, how
 * much each machine is sampled and checked, the penalty, the verifier and
 * whether plugin engines are re-executed. Anyone else reads it.
 *
 * The settings are `NetworkSettings.metering`, saved through
 * `/api/ui/network/settings` like the rest of the network and fanned out to
 * every member; the nodes pick them up within seconds
 * (`crates/node/src/backend/metering.rs`, `spawn_refresher`). The ledger itself
 * is the metering plugin on one admin, so the panel says when no plugin runs
 * here.
 */
import { Gauge, Save } from "lucide-react";
import { useEffect, useState } from "react";
import { Badge, Button, Field, Input, Panel, PanelHeader, Select } from "./ui";
import { api } from "../lib/api";
import { useCluster, usePlugins } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { MeteringSettings, NetworkInfoView, NetworkSettings } from "../lib/types";
import { useI18n } from "../i18n/provider";

type T = ReturnType<typeof useI18n>["t"];

/** Every field the node fills when the document has none (its defaults). */
export const METERING_DEFAULTS: MeteringSettings = {
  enabled: false,
  cycle_secs: 7 * 86_400,
  retain_cycles: 2,
  samples_per_node: 30,
  challenges_per_node: 5,
  penalty_factor: 2,
  draft_weight: 0.5,
  verifier: null,
  gateway_fee: { per_request: 0, per_prompt_token: 0 },
  grace_secs: 3_600,
  verify_plugin_engines: false,
};

const CYCLES = [3_600, 86_400, 7 * 86_400, 30 * 86_400];

function cycleText(t: T, secs: number): string {
  if (secs % 86_400 === 0) return t("metering.days", { count: secs / 86_400 });
  return t("metering.hours", { count: Math.round(secs / 3_600) });
}

export function MeteringPanel({
  network,
  editable,
  onSaved,
}: {
  network: NetworkSettings;
  editable: boolean;
  /** The saved network, so the screen's own draft does not put back the old values. */
  onSaved: (saved: NetworkSettings) => void;
}) {
  const { t } = useI18n();
  const toast = useToast();
  const cluster = useCluster();
  const plugins = usePlugins();
  const current = { ...METERING_DEFAULTS, ...(network.metering ?? {}) };
  const [draft, setDraft] = useState<MeteringSettings>(current);
  const [saving, setSaving] = useState(false);

  // A change from another screen or machine replaces the draft while it is unchanged.
  const currentKey = JSON.stringify(current);
  useEffect(() => setDraft({ ...METERING_DEFAULTS, ...(network.metering ?? {}) }), [currentKey]); // eslint-disable-line react-hooks/exhaustive-deps

  const changed = JSON.stringify(draft) !== currentKey;
  const ledgerHere = (plugins.data ?? []).some((p) => p.code === "metering" && p.active);
  const peers = cluster.data?.peers ?? [];
  const update = (patch: Partial<MeteringSettings>) => setDraft((d) => ({ ...d, ...patch }));

  async function save(next: MeteringSettings) {
    setSaving(true);
    try {
      const saved = await api.post<NetworkInfoView>("/api/ui/network/settings", { ...network, metering: next });
      onSaved(saved.network);
      toast.success(next.enabled ? t("metering.savedOn") : t("metering.savedOff"));
    } catch (error) {
      toast.fromError(t("metering.saveFailed"), error);
    } finally {
      setSaving(false);
    }
  }

  const status = (
    <Badge tone={current.enabled ? "aqua" : "neutral"}>{current.enabled ? t("metering.on") : t("metering.off")}</Badge>
  );

  if (!editable) {
    return (
      <Panel data-testid="metering-panel">
        <PanelHeader title={t("metering.title")} subtitle={t("metering.subtitle")} actions={status} />
        {current.enabled && (
          <p className="px-4 pb-4 text-sm text-muted">
            {t("metering.readOnly", { cycle: cycleText(t, current.cycle_secs), samples: current.samples_per_node })}
          </p>
        )}
      </Panel>
    );
  }

  return (
    <Panel data-testid="metering-panel">
      <PanelHeader title={t("metering.title")} subtitle={t("metering.subtitle")} actions={status} />
      <div className="grid gap-3 px-4 pb-4">
        <label className="flex items-start gap-3 rounded-lg border border-line px-3 py-2.5">
          <input
            type="checkbox"
            className="mt-1"
            data-testid="metering-enabled"
            checked={draft.enabled}
            onChange={(e) => update({ enabled: e.target.checked })}
          />
          <span>
            <span className="block text-sm text-fg">{t("metering.enable")}</span>
            <span className="block text-xs text-faint">{t("metering.enableHint")}</span>
          </span>
        </label>
        {draft.enabled && !ledgerHere && (
          <p className="rounded-lg border border-amber/40 bg-amber/10 px-3 py-2 text-xs text-muted" data-testid="metering-no-ledger">
            <Gauge className="mr-1 inline h-3.5 w-3.5 text-amber" />
            {t("metering.noLedgerHere")}
          </p>
        )}
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label={t("metering.cycle")} hint={t("metering.cycleHint")}>
            <Select
              name="metering-cycle"
              value={String(draft.cycle_secs)}
              onChange={(e) => update({ cycle_secs: Number(e.target.value) })}
            >
              {[...new Set([...CYCLES, draft.cycle_secs])].sort((a, b) => a - b).map((secs) => (
                <option key={secs} value={secs}>{cycleText(t, secs)}</option>
              ))}
            </Select>
          </Field>
          <Field label={t("metering.grace")} hint={t("metering.graceHint")}>
            <Input
              name="metering-grace"
              inputMode="numeric"
              value={String(Math.round(draft.grace_secs / 60))}
              onChange={(e) => update({ grace_secs: Math.max(0, Number(e.target.value) || 0) * 60 })}
            />
          </Field>
          <Field label={t("metering.samples")} hint={t("metering.samplesHint")}>
            <Input
              name="metering-samples"
              inputMode="numeric"
              value={String(draft.samples_per_node)}
              onChange={(e) => update({ samples_per_node: Math.max(0, Number(e.target.value) || 0) })}
            />
          </Field>
          <Field label={t("metering.challenges")} hint={t("metering.challengesHint")}>
            <Input
              name="metering-challenges"
              inputMode="numeric"
              value={String(draft.challenges_per_node)}
              onChange={(e) => update({ challenges_per_node: Math.max(0, Number(e.target.value) || 0) })}
            />
          </Field>
          <Field label={t("metering.penalty")} hint={t("metering.penaltyHint")}>
            <Input
              name="metering-penalty"
              inputMode="decimal"
              value={String(draft.penalty_factor)}
              onChange={(e) => update({ penalty_factor: Math.max(0, Number(e.target.value) || 0) })}
            />
          </Field>
          <Field label={t("metering.verifier")} hint={t("metering.verifierHint")}>
            <Select
              name="metering-verifier"
              value={draft.verifier ?? ""}
              onChange={(e) => update({ verifier: e.target.value || null })}
            >
              <option value="">{t("metering.verifierLedger")}</option>
              {peers.map((peer) => (
                <option key={peer.node_id} value={peer.node_id}>
                  {peer.node?.node_name || peer.node?.hostname || peer.short_id}
                </option>
              ))}
              {draft.verifier && !peers.some((p) => p.node_id === draft.verifier) && (
                <option value={draft.verifier}>{draft.verifier.slice(0, 12)}</option>
              )}
            </Select>
          </Field>
        </div>
        <label className="flex items-start gap-3 rounded-lg border border-line px-3 py-2.5">
          <input
            type="checkbox"
            className="mt-1"
            checked={draft.verify_plugin_engines}
            onChange={(e) => update({ verify_plugin_engines: e.target.checked })}
          />
          <span>
            <span className="block text-sm text-fg">{t("metering.pluginEngines")}</span>
            <span className="block text-xs text-faint">{t("metering.pluginEnginesHint")}</span>
          </span>
        </label>
        <div>
          <Button
            variant="primary"
            icon={<Save className="h-4 w-4" />}
            loading={saving}
            disabled={!changed}
            onClick={() => void save(draft)}
          >
            {t("metering.save")}
          </Button>
        </div>
      </div>
    </Panel>
  );
}
