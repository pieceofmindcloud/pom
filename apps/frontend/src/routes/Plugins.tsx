import { Download, ExternalLink, KeyRound, RefreshCw, ShieldCheck, XCircle } from "lucide-react";
import { useState } from "react";
import type { Tone } from "../components/ui";
import { Badge, Button, ErrorState, Input, PageHeader, Panel, PanelHeader, Skeleton } from "../components/ui";
import { api } from "../lib/api";
import { usePlugins } from "../lib/queries";
import type { PluginStatus } from "../lib/types";
import { useI18n } from "../i18n/provider";

/**
 * Presentation-only lookup for plugins the catalog already knows about. Not
 * a second catalog: it just names/describes the codes `PluginStatus.code`
 * can carry, and falls back to the raw code for anything it does not
 * recognize yet, so a plugin added on the backend without a frontend update
 * still renders (as its bare code) instead of disappearing.
 */
const CATALOG_COPY: Record<string, { nameKey: string; descriptionKey: string }> = {
  enterprise: {
    nameKey: "plugins.catalog.enterprise.name",
    descriptionKey: "plugins.catalog.enterprise.description",
  },
};

function pluginName(t: (key: string) => string, code: string): string {
  const copy = CATALOG_COPY[code];
  return copy ? t(copy.nameKey) : code;
}

function pluginDescription(t: (key: string) => string, code: string): string {
  const copy = CATALOG_COPY[code];
  return copy ? t(copy.descriptionKey) : "";
}

function stateTone(state: PluginStatus["state"]): Tone {
  switch (state) {
    case "active":
      return "aqua";
    case "degraded":
      return "amber";
    case "expired":
    case "revoked":
      return "rose";
    default:
      return "neutral";
  }
}

export function Plugins() {
  const { t } = useI18n();
  const plugins = usePlugins();
  const [licenseKey, setLicenseKey] = useState("");
  const [activateBusy, setActivateBusy] = useState(false);
  const [activateError, setActivateError] = useState(false);
  const [activatedCodes, setActivatedCodes] = useState<string[] | null>(null);
  const [busyCode, setBusyCode] = useState<string | null>(null);
  const [updateFailedCode, setUpdateFailedCode] = useState<string | null>(null);

  async function activate(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const key = licenseKey.trim();
    if (!key) {
      setActivateError(true);
      return;
    }
    setActivateBusy(true);
    setActivateError(false);
    setActivatedCodes(null);
    try {
      const response = await api.post<{ activated: string[] }>("/api/ui/plugins/activate", {
        license_key: key,
      });
      setLicenseKey("");
      setActivatedCodes(response.activated);
      await plugins.refetch();
    } catch {
      setActivateError(true);
    } finally {
      setActivateBusy(false);
    }
  }

  async function refresh(code: string) {
    setBusyCode(code);
    try {
      await api.post(`/api/ui/plugins/${code}/refresh`);
    } finally {
      await plugins.refetch();
      setBusyCode(null);
    }
  }

  async function deactivate(code: string) {
    setBusyCode(code);
    try {
      await api.post(`/api/ui/plugins/${code}/deactivate`);
    } finally {
      await plugins.refetch();
      setBusyCode(null);
    }
  }

  async function applyUpdate(code: string, releaseId: string) {
    setBusyCode(code);
    setUpdateFailedCode(null);
    try {
      await api.post(`/api/ui/plugins/${code}/update/apply`, { release_id: releaseId });
    } catch {
      setUpdateFailedCode(code);
    } finally {
      await plugins.refetch();
      setBusyCode(null);
    }
  }

  if (plugins.isLoading) {
    return <Skeleton className="h-64 w-full" />;
  }

  if (plugins.isError && !plugins.data) {
    return <ErrorState error={plugins.error} onRetry={() => void plugins.refetch()} />;
  }

  const catalog = plugins.data ?? [];

  return (
    <>
      <PageHeader title={t("plugins.title")} subtitle={t("plugins.subtitle")} />

      <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]">
        <div className="space-y-4">
          {catalog.map((plugin) => (
            <Panel key={plugin.code} className={plugin.active ? "border-aqua/35 bg-aqua/5" : "border-line"}>
              <PanelHeader
                eyebrow={pluginName(t, plugin.code)}
                title={plugin.active ? t("plugins.activeTitle") : t("plugins.inactiveTitle")}
                subtitle={pluginDescription(t, plugin.code)}
                actions={<Badge tone={stateTone(plugin.state)}>{t(`plugins.state.${plugin.state}`)}</Badge>}
              />
              <div className="space-y-4 px-5 py-5">
                {plugin.active && (
                  <dl className="grid gap-4 sm:grid-cols-2">
                    <div>
                      <dt className="eyebrow mb-1">{t("plugins.version")}</dt>
                      <dd className="font-mono text-sm text-fg">{plugin.plugin_version ?? "-"}</dd>
                    </div>
                    <div>
                      <dt className="eyebrow mb-1">{t("plugins.features")}</dt>
                      <dd className="text-sm text-fg">{plugin.features.length > 0 ? plugin.features.join(", ") : "-"}</dd>
                    </div>
                  </dl>
                )}
                <div className="flex flex-wrap gap-2">
                  {plugin.active && (
                    <>
                      <Button
                        variant="outline"
                        loading={busyCode === plugin.code}
                        icon={<RefreshCw className="h-3.5 w-3.5" />}
                        onClick={() => void refresh(plugin.code)}
                      >
                        {t("plugins.refresh")}
                      </Button>
                      <Button
                        variant="outline"
                        loading={busyCode === plugin.code}
                        icon={<XCircle className="h-3.5 w-3.5" />}
                        onClick={() => void deactivate(plugin.code)}
                      >
                        {t("plugins.deactivate")}
                      </Button>
                    </>
                  )}
                  <a
                    href={plugin.manage_url}
                    target="_blank"
                    rel="noreferrer"
                    className="inline-flex h-9 items-center gap-2 rounded-lg px-3.5 text-sm font-medium text-muted hover:bg-line/40 hover:text-fg"
                  >
                    {plugin.active ? t("plugins.manage") : t("plugins.buy")} <ExternalLink className="h-3.5 w-3.5" />
                  </a>
                </div>
                {plugin.available_update && (
                  <div className="space-y-3 rounded-xl border border-iris/35 bg-iris/8 px-4 py-3" role="status">
                    <div>
                      <p className="text-sm font-medium text-fg">
                        {t("plugins.updateAvailable", { version: plugin.available_update.version })}
                      </p>
                      <p className="mt-1 font-mono text-xs text-muted">
                        {t("plugins.updateRelease", { release_id: plugin.available_update.release_id })}
                      </p>
                      {plugin.available_update.changelog && (
                        <p className="mt-2 text-xs leading-relaxed text-muted">{plugin.available_update.changelog}</p>
                      )}
                    </div>
                    <Button
                      variant="primary"
                      loading={busyCode === plugin.code}
                      icon={<Download className="h-3.5 w-3.5" />}
                      onClick={() => void applyUpdate(plugin.code, plugin.available_update!.release_id)}
                    >
                      {t("plugins.updateApply")}
                    </Button>
                    {updateFailedCode === plugin.code && (
                      <p role="alert" className="text-xs text-rose">{t("plugins.updateApplyFailed")}</p>
                    )}
                  </div>
                )}
              </div>
            </Panel>
          ))}
        </div>

        <Panel className="self-start">
          <PanelHeader
            eyebrow={t("plugins.activationEyebrow")}
            title={t("plugins.formTitle")}
            subtitle={t("plugins.formSubtitle")}
            actions={<KeyRound className="h-5 w-5 text-iris" />}
          />
          <form className="space-y-4 px-5 py-5" onSubmit={(event) => void activate(event)}>
            <label className="block">
              <span className="mb-1.5 block text-[0.8rem] font-medium text-muted">{t("plugins.licenseKeyLabel")}</span>
              <Input
                type="password"
                name="plugin-license-key"
                autoComplete="off"
                aria-label={t("plugins.licenseKeyLabel")}
                placeholder={t("plugins.licenseKeyPlaceholder")}
                value={licenseKey}
                onChange={(event) => setLicenseKey(event.target.value)}
              />
            </label>
            {activateError && <p role="alert" className="text-xs text-rose">{t("plugins.activationFailed")}</p>}
            {activatedCodes && activatedCodes.length > 0 && (
              <p role="status" className="text-xs text-aqua">
                {t("plugins.activationSucceeded", {
                  plugins: activatedCodes.map((code) => pluginName(t, code)).join(", "),
                })}
              </p>
            )}
            <Button variant="primary" type="submit" loading={activateBusy} icon={<ShieldCheck className="h-3.5 w-3.5" />}>
              {t("plugins.activate")}
            </Button>
            <p className="text-xs leading-relaxed text-faint">{t("plugins.keyPrivacy")}</p>
          </form>
        </Panel>
      </div>
    </>
  );
}
