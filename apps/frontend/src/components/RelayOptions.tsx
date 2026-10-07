/**
 * Whom this machine relays for. Three switches saved one at a time and
 * applied live by the node (`/api/ui/network/relay`); an option the command
 * line fixed for this run is shown but cannot be changed here.
 */
import { useEffect, useState } from "react";
import { KeyValue, Panel, PanelHeader } from "./ui";
import { api } from "../lib/api";
import { useToast } from "../lib/toast";
import { useI18n } from "../i18n/provider";

export interface RelayStatus {
  everyone: boolean;
  cluster: boolean;
  transfers: boolean;
  fixed: { everyone: boolean; no_cluster: boolean; no_transfers: boolean };
  reachable: boolean;
  serving: boolean;
}

type RelayOption = "everyone" | "cluster" | "transfers";

/** What the node is doing with the options, in one line. */
export function relayStateKey(status: RelayStatus): string {
  if (status.everyone) return "network.relayStateEveryone";
  if (!status.cluster) return "network.relayStateOff";
  return status.reachable ? "network.relayStateCluster" : "network.relayStateUnreachable";
}

export function RelayOptions() {
  const { t } = useI18n();
  const toast = useToast();
  const [status, setStatus] = useState<RelayStatus | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void api
      .get<RelayStatus>("/api/ui/network/relay")
      .then((loaded) => {
        if (!cancelled) setStatus(loaded);
      })
      // Without an answer there is nothing true to show; the panel stays out.
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, []);

  if (!status) return null;

  async function save(option: RelayOption, value: boolean) {
    if (!status) return;
    setSaving(true);
    try {
      const saved = await api.post<RelayStatus>("/api/ui/network/relay", {
        everyone: status.everyone,
        cluster: status.cluster,
        transfers: status.transfers,
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

  const rows: { option: RelayOption; label: string; hint: string; fixed: boolean; disabled: boolean }[] = [
    {
      option: "cluster",
      label: t("network.relayCluster"),
      hint: t("network.relayClusterHint"),
      fixed: status.fixed.no_cluster,
      disabled: false,
    },
    {
      option: "transfers",
      label: t("network.relayTransfers"),
      hint: t("network.relayTransfersHint"),
      fixed: status.fixed.no_transfers,
      // Nothing to allow when the machine relays for nobody of its network.
      disabled: !status.cluster,
    },
    {
      option: "everyone",
      label: t("network.relayEveryone"),
      hint: t("network.relayEveryoneHint"),
      fixed: status.fixed.everyone,
      disabled: false,
    },
  ];

  return (
    <Panel>
      <PanelHeader title={t("network.relayTitle")} subtitle={t("network.relaySubtitle")} />
      <div className="space-y-3 px-5 py-4">
        {rows.map((row) => (
          <div key={row.option}>
            <label className="flex items-center gap-2 text-sm text-fg">
              <input
                type="checkbox"
                data-testid={`relay-${row.option}`}
                checked={status[row.option]}
                disabled={saving || row.fixed || row.disabled}
                onChange={(event) => void save(row.option, event.target.checked)}
              />
              {row.label}
            </label>
            <span className="mt-1.5 block text-xs text-faint">
              {row.fixed ? t("network.relayFixed") : row.hint}
            </span>
          </div>
        ))}
        <KeyValue label={t("network.relayStatus")}>
          <span data-testid="relay-state">{t(relayStateKey(status))}</span>
        </KeyValue>
      </div>
    </Panel>
  );
}
