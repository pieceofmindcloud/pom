/**
 * Local preferences of this node: administrative boundary, TLS between nodes,
 * thermal protection and optional credential for the Hugging Face provider.
 */
import { Ban, KeyRound, Lock, MessagesSquare, RadioTower, ScrollText, ShieldCheck, Thermometer, Zap } from "lucide-react";
import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { HttpsCertificatePanel } from "../components/HttpsCertificatePanel";
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
import { api, clearAdminAccessToken } from "../lib/api";
import { useAdminAccess, useAdminAccessInfo, useEnterpriseStatus, useInvalidate, useNode } from "../lib/queries";
import { useToast } from "../lib/toast";
import { useProfile } from "../lib/profile";
import type { AdminAccessInfo, AdminExposure, ThermalPolicy } from "../lib/types";
import { useI18n } from "../i18n/provider";

const DEFAULT_THERMAL_POLICY: ThermalPolicy = {
  limit_c: 85,
  hysteresis_c: 8,
  cooldown_secs: 30,
};

export function QuicPanel({
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
        title={t("network.quicTitle")}
        subtitle={t("network.quicSubtitle")}
        actions={
          <Button
            variant={enabled ? "danger" : "primary"}
            role="switch"
            aria-checked={enabled}
            disabled={!authority || busy}
            loading={busy}
            icon={<Zap className="h-3.5 w-3.5" />}
            onClick={() => onToggle?.(!enabled)}
          >
            {t(enabled ? "network.quicDisable" : "network.quicEnable")}
          </Button>
        }
      />
      <div className="space-y-3 px-5 py-4">
        <KeyValue label={t("network.quicState")}>
          <Badge tone={enabled ? "aqua" : "amber"}>
            <Zap className="h-3.5 w-3.5" />
            {t(enabled ? "network.quicOn" : "network.quicOff")}
          </Badge>
        </KeyValue>
        <Footnote icon={<Zap className="h-3.5 w-3.5" />}>
          {t("network.quicHowItWorks")}
        </Footnote>
        {!authority && (
          <Footnote icon={<Ban className="h-3.5 w-3.5" />}>
            {t("network.quicNotAuthority")}
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

type AutoDiscoveryStatus = {
  enabled: boolean;
  active: boolean;
  searching: boolean;
  found: boolean;
  port: number;
};

export function AutoDiscoveryPanel({
  status,
  authority,
  busy = false,
  onToggle,
}: {
  status: AutoDiscoveryStatus;
  authority: boolean;
  busy?: boolean;
  onToggle?: (next: boolean) => void;
}) {
  const { t } = useI18n();
  const enabled = status.enabled;
  return (
    <Panel>
      <PanelHeader
        title={t("network.autoDiscoveryTitle")}
        subtitle={t("network.autoDiscoverySubtitle")}
        actions={
          <Button
            variant={enabled ? "danger" : "primary"}
            role="switch"
            aria-label={t("network.autoDiscoveryTitle")}
            aria-checked={enabled}
            disabled={!authority || busy}
            loading={busy}
            icon={<RadioTower className="h-3.5 w-3.5" />}
            onClick={() => onToggle?.(!enabled)}
          >
            {t(enabled ? "network.autoDiscoveryDisable" : "network.autoDiscoveryEnable")}
          </Button>
        }
      />
      <div className="space-y-3 px-5 py-4">
        <KeyValue label={t("network.autoDiscoveryState")}>
          <Badge tone={enabled ? "aqua" : "amber"}>
            <RadioTower className="h-3.5 w-3.5" />
            {t(enabled ? "network.autoDiscoveryOn" : "network.autoDiscoveryOff")}
          </Badge>
        </KeyValue>
        <Footnote icon={<RadioTower className="h-3.5 w-3.5" />}>
          {t("network.autoDiscoveryHowItWorks")}
        </Footnote>
        {!authority && (
          <Footnote icon={<Ban className="h-3.5 w-3.5" />}>
            {t("network.autoDiscoveryNotAuthority")}
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
  const [passwordConfigured, setPasswordConfigured] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (loaded || !access.data) return;
    setExposure(access.data.admin_exposure);
    setPort(String(access.data.gateway_port));
    setPasswordConfigured(access.data.password_configured);
    setLoaded(true);
  }, [access.data, loaded]);

  async function save() {
    const gatewayPort = Number(port);
    if (!Number.isInteger(gatewayPort) || gatewayPort < 1 || gatewayPort > 65535) {
      toast.fromError(t("preferences.accessSaveFailed"), new Error(t("preferences.portInvalid")));
      return;
    }
    if (exposure === "network" && !passwordConfigured) {
      toast.fromError(t("preferences.accessSaveFailed"), new Error(t("preferences.accessPasswordRequired")));
      return;
    }
    setSaving(true);
    try {
      const saved = await api.post<AdminAccessInfo & { restart: boolean }>(
        "/api/ui/access",
        {
          admin_exposure: exposure,
          gateway_port: gatewayPort,
        },
      );
      setPasswordConfigured(saved.password_configured);
      if (exposure === "localhost") clearAdminAccessToken();
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

  const networkExposed = exposure === "network";
  // The exposure is the principal's setting: elsewhere the node refuses it.
  if (access.isError) return null;
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
          <Field label={t("preferences.accessModeLabel")} hint={t(networkExposed ? "preferences.accessModePasswordHint" : "preferences.accessModeLocalHint")}>
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
          <Footnote icon={<Lock className="h-3.5 w-3.5" />}>
            {passwordConfigured ? t("preferences.accessPasswordSet") : (
              <>
                {t("preferences.accessPasswordRequired")}{" "}
                <Link to="/config/password" className="text-iris hover:underline">{t("password.change")}</Link>
              </>
            )}
          </Footnote>
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

/**
 * The panel password, for every profile: a link to its own screen
 * (`/config/password`), which asks the current password, the new one and its
 * confirmation. Separate from the exposure above.
 */
function PasswordPanel() {
  const { t } = useI18n();
  const access = useAdminAccess();
  return (
    <Panel>
      <PanelHeader
        eyebrow={t("preferences.accessEyebrow")}
        title={t("preferences.accessPasswordTitle")}
        subtitle={t(access.data?.default_password ? "preferences.passwordDefault" : "preferences.passwordSubtitle")}
        actions={
          <Link to="/config/password" data-testid="preferences-change-password">
            <Button variant={access.data?.default_password ? "primary" : "outline"} icon={<KeyRound className="h-3.5 w-3.5" />}>
              {t("password.change")}
            </Button>
          </Link>
        }
      />
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
  const [validationError, setValidationError] = useState<string | null>(null);

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

  function saveThermal() {
    const next = nextPolicy();
    if (!Number.isFinite(next.limit_c) || next.limit_c < 70) {
      setValidationError(t("network.thermalLimitInvalid"));
      return;
    }
    if (!Number.isInteger(next.cooldown_secs) || next.cooldown_secs < 1 || next.cooldown_secs > 60) {
      setValidationError(t("network.thermalCooldownInvalid"));
      return;
    }
    setValidationError(null);
    onSave(next);
  }

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
            onClick={saveThermal}
          >
            {t("network.thermalSave")}
          </Button>
        }
      />
      <div className="grid gap-4 px-5 py-4 sm:grid-cols-3">
        <Field label={t("network.thermalLimitLabel")}>
          <Input type="number" min="70" max="150" step="0.5" value={limit} onChange={(e) => { setLimit(e.target.value); setValidationError(null); }} />
        </Field>
        <Field label={t("network.thermalHysteresisLabel")}>
          <Input type="number" min="0.1" max="149" step="0.5" value={hysteresis} onChange={(e) => setHysteresis(e.target.value)} />
        </Field>
        <Field label={t("network.thermalCooldownLabel")}>
          <Input type="number" min="1" max="60" step="1" value={cooldown} onChange={(e) => { setCooldown(e.target.value); setValidationError(null); }} />
        </Field>
        {validationError && <p className="text-xs text-rose sm:col-span-3" role="alert">{validationError}</p>}
        <Footnote className="sm:col-span-3" icon={<Thermometer className="h-3.5 w-3.5" />}>
          {t("network.thermalHint")} {Number.isFinite(resumeAt) && ` ${t("network.thermalResumeAt", { value: resumeAt })}`}
        </Footnote>
      </div>
    </Panel>
  );
}

export function Preferences({ administrative = false }: { administrative?: boolean }) {
  const { t } = useI18n();
  const node = useNode();
  // Process-log collection is kept by the logs plugin: the switch exists only
  // on an admin whose machine runs it.
  const logsPlugin = useEnterpriseStatus();
  // An inference node only talks to the models: GPU thermal protection and
  // the Hugging Face token are for machines that serve or download them.
  const { profile } = useProfile();
  const inferOnly = profile === "visitor";
  const toast = useToast();
  const invalidate = useInvalidate();
  const [thermalSaving, setThermalSaving] = useState(false);
  const [quicSaving, setQuicSaving] = useState(false);
  const [logsSaving, setLogsSaving] = useState(false);
  const [networkChatSaving, setNetworkChatSaving] = useState(false);
  const [autoDiscovery, setAutoDiscovery] = useState<AutoDiscoveryStatus>({
    enabled: false,
    active: false,
    searching: false,
    found: false,
    port: 0,
  });
  const [autoDiscoveryLoaded, setAutoDiscoveryLoaded] = useState(!administrative);
  const [autoDiscoverySaving, setAutoDiscoverySaving] = useState(false);
  const quicEnabled = node.data?.quic_enabled ?? false;

  useEffect(() => {
    if (!administrative || autoDiscoveryLoaded) return;
    let cancelled = false;
    void api.get<AutoDiscoveryStatus>("/api/ui/network/auto-discovery")
      .then((status) => {
        if (cancelled) return;
        setAutoDiscovery(status);
        setAutoDiscoveryLoaded(true);
      })
      .catch((error) => {
        if (cancelled) return;
        setAutoDiscoveryLoaded(true);
        toast.fromError(t("network.autoDiscoveryFailed"), error);
      });
    return () => {
      cancelled = true;
    };
  }, [administrative, autoDiscoveryLoaded, t, toast]);

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

  async function toggleQuic(next: boolean) {
    setQuicSaving(true);
    try {
      await api.post("/api/ui/network/quic", { quic_enabled: next });
      toast.success(
        t(next ? "network.quicEnabled" : "network.quicDisabled"),
        t("network.quicPropagates"),
      );
      invalidate();
    } catch (error) {
      toast.fromError(t("network.quicFailed"), error);
    } finally {
      setQuicSaving(false);
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

  async function toggleAutoDiscovery(next: boolean) {
    setAutoDiscoverySaving(true);
    try {
      const status = await api.post<AutoDiscoveryStatus>("/api/ui/network/auto-discovery", {
        enabled: next,
      });
      setAutoDiscovery(status);
      toast.success(
        t(next ? "network.autoDiscoveryEnabled" : "network.autoDiscoveryDisabled"),
        t("network.autoDiscoveryPropagates"),
      );
      invalidate();
    } catch (error) {
      toast.fromError(t("network.autoDiscoveryFailed"), error);
    } finally {
      setAutoDiscoverySaving(false);
    }
  }

  return (
    <>
      <PageHeader
        title={t(administrative ? "preferences.adminTitle" : "preferences.title")}
        subtitle={t(administrative ? "preferences.adminSubtitle" : "preferences.subtitle")}
      />
      <div className="space-y-5">
        {administrative ? <>
          <QuicPanel enabled={quicEnabled} authority={node.data?.quic_authority ?? false} busy={quicSaving} onToggle={(next) => void toggleQuic(next)} />
          {logsPlugin.data.active && <ClusterLogsPanel enabled={node.data?.cluster_logs_enabled ?? false} authority={node.data?.cluster_logs_authority ?? false} busy={logsSaving} onToggle={(next) => void toggleLogs(next)} />}
          <NetworkChatPanel enabled={node.data?.network_chat_enabled ?? false} authority={node.data?.network_chat_authority ?? false} busy={networkChatSaving} onToggle={(next) => void toggleNetworkChat(next)} />
          {autoDiscoveryLoaded && <AutoDiscoveryPanel status={autoDiscovery} authority={node.data?.quic_authority ?? false} busy={autoDiscoverySaving} onToggle={(next) => void toggleAutoDiscovery(next)} />}
        </> : <>
          <PasswordPanel />
          <AdminAccessPanel />
          <HttpsCertificatePanel />
          {!inferOnly && <ThermalPanel policy={node.data?.thermal} busy={thermalSaving} onSave={(next) => void saveThermal(next)} />}
        </>}
      </div>
    </>
  );
}
