/**
 * Preferências locais deste nó: fronteira administrativa, TLS entre nós,
 * proteção térmica e credencial opcional do provedor Hugging Face.
 */
import { Ban, Check, Copy, Lock, MessagesSquare, ScrollText, ShieldCheck, ShieldOff, Thermometer } from "lucide-react";
import { useEffect, useState } from "react";
import { HuggingFaceSettingsPanel } from "../components/HuggingFaceSettingsPanel";
import {
  Badge,
  Button,
  Field,
  Footnote,
  Input,
  KeyValue,
  PageHeader,
  Panel,
  PanelHeader,
  Select,
} from "../components/ui";
import { api, clearAdminAccessToken, setAdminAccessToken } from "../lib/api";
import { useAdminAccessInfo, useInvalidate, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { AdminAccessInfo, AdminExposure, ThermalPolicy } from "../lib/types";
import { useI18n } from "../i18n/provider";

const DEFAULT_THERMAL_POLICY: ThermalPolicy = {
  limit_c: 85,
  hysteresis_c: 8,
  cooldown_secs: 30,
};

export function TlsPanel({
  tlsRequired,
  authority,
  busy = false,
  onToggle,
}: {
  tlsRequired: boolean;
  authority: boolean;
  busy?: boolean;
  onToggle?: (next: boolean) => void;
}) {
  const { t } = useI18n();
  return (
    <Panel>
      <PanelHeader
        title={t("network.tlsTitle")}
        subtitle={t("network.tlsSubtitle")}
        actions={
          <Button
            variant={tlsRequired ? "danger" : "primary"}
            role="switch"
            aria-checked={tlsRequired}
            disabled={!authority || busy}
            loading={busy}
            icon={
              tlsRequired ? (
                <ShieldOff className="h-3.5 w-3.5" />
              ) : (
                <Lock className="h-3.5 w-3.5" />
              )
            }
            onClick={() => onToggle?.(!tlsRequired)}
          >
            {t(tlsRequired ? "network.tlsDisable" : "network.tlsEnable")}
          </Button>
        }
      />
      <div className="space-y-3 px-5 py-4">
        <KeyValue label={t("network.tlsState")}>
          <Badge tone={tlsRequired ? "aqua" : "amber"}>
            {tlsRequired ? (
              <ShieldCheck className="h-3.5 w-3.5" />
            ) : (
              <ShieldOff className="h-3.5 w-3.5" />
            )}
            {t(tlsRequired ? "network.tlsOn" : "network.tlsOff")}
          </Badge>
        </KeyValue>
        <Footnote icon={<ShieldCheck className="h-3.5 w-3.5" />}>
          {t("network.tlsHowItWorks")}
        </Footnote>
        {!authority && (
          <Footnote icon={<Ban className="h-3.5 w-3.5" />}>
            {t("network.tlsNotAuthority")}
          </Footnote>
        )}
      </div>
    </Panel>
  );
}

export function ClusterLogsPanel({
  enabled,
  authority,
  busy = false,
  onToggle,
}: {
  enabled: boolean;
  authority: boolean;
  busy?: boolean;
  onToggle?: (next: boolean) => void;
}) {
  const { t } = useI18n();
  return (
    <Panel>
      <PanelHeader
        title={t("network.logsTitle")}
        subtitle={t("network.logsSubtitle")}
        actions={
          <Button
            variant={enabled ? "danger" : "primary"}
            role="switch"
            aria-checked={enabled}
            disabled={!authority || busy}
            loading={busy}
            icon={<ScrollText className="h-3.5 w-3.5" />}
            onClick={() => onToggle?.(!enabled)}
          >
            {t(enabled ? "network.logsDisable" : "network.logsEnable")}
          </Button>
        }
      />
      <div className="space-y-3 px-5 py-4">
        <KeyValue label={t("network.logsState")}>
          <Badge tone={enabled ? "aqua" : "amber"}>
            <ScrollText className="h-3.5 w-3.5" />
            {t(enabled ? "network.logsOn" : "network.logsOff")}
          </Badge>
        </KeyValue>
        <Footnote icon={<ScrollText className="h-3.5 w-3.5" />}>
          {t("network.logsHowItWorks")}
        </Footnote>
        {!authority && (
          <Footnote icon={<Ban className="h-3.5 w-3.5" />}>
            {t("network.logsNotAuthority")}
          </Footnote>
        )}
      </div>
    </Panel>
  );
}

export function NetworkChatPanel({
  enabled,
  authority,
  busy = false,
  onToggle,
}: {
  enabled: boolean;
  authority: boolean;
  busy?: boolean;
  onToggle?: (next: boolean) => void;
}) {
  const { t } = useI18n();
  return (
    <Panel>
      <PanelHeader
        title={t("network.networkChatTitle")}
        subtitle={t("network.networkChatSubtitle")}
        actions={
          <Button
            variant={enabled ? "danger" : "primary"}
            role="switch"
            aria-checked={enabled}
            disabled={!authority || busy}
            loading={busy}
            icon={<MessagesSquare className="h-3.5 w-3.5" />}
            onClick={() => onToggle?.(!enabled)}
          >
            {t(enabled ? "network.networkChatDisable" : "network.networkChatEnable")}
          </Button>
        }
      />
      <div className="space-y-3 px-5 py-4">
        <KeyValue label={t("network.networkChatState")}>
          <Badge tone={enabled ? "aqua" : "amber"}>
            <MessagesSquare className="h-3.5 w-3.5" />
            {t(enabled ? "network.networkChatOn" : "network.networkChatOff")}
          </Badge>
        </KeyValue>
        <Footnote icon={<MessagesSquare className="h-3.5 w-3.5" />}>
          {t("network.networkChatHowItWorks")}
        </Footnote>
        {!authority && (
          <Footnote icon={<Ban className="h-3.5 w-3.5" />}>
            {t("network.networkChatNotAuthority")}
          </Footnote>
        )}
      </div>
    </Panel>
  );
}

function AdminAccessPanel() {
  const { t } = useI18n();
  const access = useAdminAccessInfo();
  const toast = useToast();
  const invalidate = useInvalidate();
  const [exposure, setExposure] = useState<AdminExposure>("localhost");
  const [port, setPort] = useState("8080");
  const [accessToken, setAccessToken] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [saving, setSaving] = useState(false);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (loaded || !access.data) return;
    setExposure(access.data.admin_exposure);
    setPort(String(access.data.gateway_port));
    setAccessToken(access.data.access_token);
    setLoaded(true);
  }, [access.data, loaded]);

  async function save(regenerateToken = false) {
    const gatewayPort = Number(port);
    if (!Number.isInteger(gatewayPort) || gatewayPort < 1 || gatewayPort > 65535) {
      toast.fromError(t("preferences.accessSaveFailed"), new Error(t("preferences.portInvalid")));
      return;
    }
    setSaving(true);
    try {
      const saved = await api.post<AdminAccessInfo & { restart: boolean }>(
        "/api/ui/access",
        {
          admin_exposure: exposure,
          gateway_port: gatewayPort,
          regenerate_token: regenerateToken,
        },
      );
      setAccessToken(saved.access_token);
      if (saved.access_token) setAdminAccessToken(saved.access_token);
      else clearAdminAccessToken();
      toast.success(
        t("preferences.accessSaved"),
        saved.restart ? t("preferences.accessRestarting") : t("preferences.accessSavedDetail"),
      );
      invalidate();
      void access.refetch();
    } catch (error) {
      toast.fromError(t("preferences.accessSaveFailed"), error);
    } finally {
      setSaving(false);
    }
  }

  async function copyToken() {
    if (!accessToken || !navigator.clipboard) return;
    await navigator.clipboard.writeText(accessToken);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1600);
  }

  const networkExposed = exposure === "network";
  return (
    <Panel>
      <PanelHeader
        eyebrow={t("preferences.accessEyebrow")}
        title={t("preferences.accessTitle")}
        subtitle={t("preferences.accessSubtitle")}
        actions={
          <Button
            variant="primary"
            loading={saving}
            onClick={() => void save()}
          >
            {t("preferences.accessSave")}
          </Button>
        }
      />
      <div className="space-y-4 px-5 py-4">
        <div className="grid gap-4 sm:grid-cols-[minmax(0,1fr)_9rem]">
          <Field label={t("preferences.accessModeLabel")} hint={t(networkExposed ? "preferences.accessModeNetworkHint" : "preferences.accessModeLocalHint")}>
            <Select
              aria-label={t("preferences.accessModeLabel")}
              value={exposure}
              onChange={(event) => setExposure(event.target.value as AdminExposure)}
            >
              <option value="localhost">{t("preferences.accessModeLocal")}</option>
              <option value="network">{t("preferences.accessModeNetwork")}</option>
            </Select>
          </Field>
          <Field label={t("preferences.accessPortLabel")} hint={t("preferences.accessPortHint")}>
            <Input
              aria-label={t("preferences.accessPortLabel")}
              type="number"
              min="1"
              max="65535"
              step="1"
              value={port}
              onChange={(event) => setPort(event.target.value)}
            />
          </Field>
        </div>
        {networkExposed ? (
          <div className="space-y-3 rounded-lg border border-amber/25 bg-amber/6 p-3.5">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div className="min-w-0">
                <p className="text-sm font-medium text-fg">{t("preferences.accessTokenTitle")}</p>
                <p className="mt-1 text-xs leading-relaxed text-faint">{t("preferences.accessTokenHint")}</p>
              </div>
              <Button
                size="sm"
                variant="outline"
                disabled={!accessToken || saving}
                icon={copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
                onClick={() => void copyToken()}
              >
                {t(copied ? "preferences.accessTokenCopied" : "preferences.accessTokenCopy")}
              </Button>
            </div>
            <Input
              aria-label={t("preferences.accessTokenLabel")}
              readOnly
              value={accessToken ?? t("preferences.accessTokenUnavailable")}
              className="font-mono text-xs"
            />
            <Button
              size="sm"
              variant="ghost"
              disabled={saving}
              onClick={() => void save(true)}
            >
              {t("preferences.accessTokenRegenerate")}
            </Button>
          </div>
        ) : (
          <Footnote icon={<Lock className="h-3.5 w-3.5" />}>
            {t("preferences.accessLocalNoToken")}
          </Footnote>
        )}
        <Footnote icon={<ShieldCheck className="h-3.5 w-3.5" />}>
          {t("preferences.accessConnectionKeyNote")}
        </Footnote>
      </div>
    </Panel>
  );
}

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
  const [tlsSaving, setTlsSaving] = useState(false);
  const [logsSaving, setLogsSaving] = useState(false);
  const [networkChatSaving, setNetworkChatSaving] = useState(false);

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

  async function toggleTls(next: boolean) {
    setTlsSaving(true);
    try {
      await api.post("/api/ui/network/tls", { tls_required: next });
      toast.success(
        t(next ? "network.tlsEnabled" : "network.tlsDisabled"),
        t("network.tlsPropagates"),
      );
      invalidate();
    } catch (error) {
      toast.fromError(t("network.tlsFailed"), error);
    } finally {
      setTlsSaving(false);
    }
  }

  async function toggleLogs(next: boolean) {
    setLogsSaving(true);
    try {
      await api.post("/api/ui/network/logs", { logs_enabled: next });
      toast.success(
        t(next ? "network.logsEnabled" : "network.logsDisabled"),
        t("network.logsPropagates"),
      );
      invalidate();
    } catch (error) {
      toast.fromError(t("network.logsFailed"), error);
    } finally {
      setLogsSaving(false);
    }
  }

  async function toggleNetworkChat(next: boolean) {
    setNetworkChatSaving(true);
    try {
      await api.post("/api/ui/network/chat", { network_chat_enabled: next });
      toast.success(
        t(next ? "network.networkChatEnabled" : "network.networkChatDisabled"),
        t("network.networkChatPropagates"),
      );
      invalidate();
    } catch (error) {
      toast.fromError(t("network.networkChatFailed"), error);
    } finally {
      setNetworkChatSaving(false);
    }
  }

  return (
    <>
      <PageHeader
        title={t("preferences.title")}
        subtitle={t("preferences.subtitle")}
      />
      <div className="space-y-5">
        <AdminAccessPanel />
        <TlsPanel
          tlsRequired={node.data?.tls_required ?? false}
          authority={node.data?.tls_authority ?? false}
          busy={tlsSaving}
          onToggle={(next) => void toggleTls(next)}
        />
        <ClusterLogsPanel
          enabled={node.data?.cluster_logs_enabled ?? false}
          authority={node.data?.cluster_logs_authority ?? false}
          busy={logsSaving}
          onToggle={(next) => void toggleLogs(next)}
        />
        <NetworkChatPanel
          enabled={node.data?.network_chat_enabled ?? false}
          authority={node.data?.network_chat_authority ?? false}
          busy={networkChatSaving}
          onToggle={(next) => void toggleNetworkChat(next)}
        />
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
