import { Download, ExternalLink, KeyRound, Package, RefreshCw, ShieldCheck, XCircle } from "lucide-react";
import { useState } from "react";
import type { Tone } from "../components/ui";
import { Badge, Button, Empty, ErrorState, Input, PageHeader, Panel, PanelHeader, Skeleton } from "../components/ui";
import { api } from "../lib/api";
import { usePlugins, usePublicPluginCatalog } from "../lib/queries";
import type { PluginStatus } from "../lib/types";
import type { PublicPluginCatalogEntry, PublicPluginPrice } from "../lib/pluginCatalog";
import { useI18n } from "../i18n/provider";

function stateTone(state: PluginStatus["state"] | null): Tone {
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

function purchaseUrl(code: string): string {
  return `https://pieceofmind.cloud/plugins/index.html?plugin=${encodeURIComponent(code)}`;
}

function safeExternalUrl(value: string | null | undefined): string | null {
  if (!value) return null;
  try {
    const url = new URL(value);
    return url.protocol === "https:" || url.protocol === "http:" ? url.toString() : null;
  } catch {
    return null;
  }
}

function formatPrice(price: PublicPluginPrice | null, locale: string): string | null {
  if (price?.amount === null || price?.amount === undefined || !price.currency) return null;
  try {
    return new Intl.NumberFormat(locale, {
      style: "currency",
      currency: price.currency,
    }).format(price.amount / 100);
  } catch {
    return null;
  }
}

type PluginCardProps = {
  entry: PublicPluginCatalogEntry | null;
  status: PluginStatus | null;
  licenseStatusAvailable: boolean;
  busyCode: string | null;
  updateFailedCode: string | null;
  locale: string;
  onRefresh: (code: string) => void;
  onDeactivate: (code: string) => void;
  onApplyUpdate: (code: string, releaseId: string) => void;
};

function PluginCard({
  entry,
  status,
  licenseStatusAvailable,
  busyCode,
  updateFailedCode,
  locale,
  onRefresh,
  onDeactivate,
  onApplyUpdate,
}: PluginCardProps) {
  const { t } = useI18n();
  const code = entry?.code ?? status?.code ?? "";
  const name = entry?.display_name ?? code;
  const active = Boolean(status?.active);
  const release = entry?.releases[0] ?? null;
  const hasRequiredLicense = entry?.requires_license === true;
  const licenseLabel = !licenseStatusAvailable
    ? t("plugins.licenseUnknown")
    : status
      ? t(`plugins.state.${status.state}`)
      : entry?.requires_license === false
        ? t("plugins.noLicenseRequired")
        : entry?.requires_license === true
          ? t("plugins.state.needs_activation")
          : t("plugins.licenseUnknown");
  const badgeTone = status?.state
    ? stateTone(status.state)
    : entry?.requires_license === false && licenseStatusAvailable
      ? "aqua"
      : "neutral";
  const manageUrl = active ? safeExternalUrl(status?.manage_url) : null;
  const monthlyPrice = formatPrice(entry?.pricing.monthly ?? null, locale);
  const annualPrice = formatPrice(entry?.pricing.annual ?? null, locale);
  const hasMissingMetadata = entry !== null && (
    !entry.display_name
    || !entry.description
    || (!entry.icon_url && entry.screenshot_urls.length === 0)
  );

  return (
    <Panel className={active ? "border-aqua/35 bg-aqua/5" : "border-line"}>
      <PanelHeader
        eyebrow={code}
        title={name}
        subtitle={entry?.description ?? t("plugins.metadataUnavailable")}
        actions={(
          <div className="flex items-center gap-2">
            {entry?.icon_url ? (
              <img
                src={entry.icon_url}
                alt={name}
                loading="lazy"
                referrerPolicy="no-referrer"
                className="h-10 w-10 rounded-xl border border-line object-cover"
              />
            ) : (
              <span aria-hidden="true" className="flex h-10 w-10 items-center justify-center rounded-xl border border-line bg-raised text-muted">
                <Package className="h-5 w-5" />
              </span>
            )}
            <Badge tone={badgeTone}>{licenseLabel}</Badge>
          </div>
        )}
      />

      <div className="space-y-4 px-5 py-5">
        {hasMissingMetadata && (
          <p role="note" className="text-xs text-faint">{t("plugins.metadataIncomplete")}</p>
        )}

        {entry && entry.screenshot_urls.length > 0 && (
          <div className="grid grid-cols-2 gap-2 sm:grid-cols-4" aria-label={t("plugins.screenshots")}>
            {entry.screenshot_urls.slice(0, 4).map((url, index) => (
              <img
                key={`${url}:${index}`}
                src={url}
                alt={t("plugins.screenshotAlt", { name, number: index + 1 })}
                loading="lazy"
                referrerPolicy="no-referrer"
                className="aspect-video w-full rounded-lg border border-line bg-raised object-cover"
              />
            ))}
          </div>
        )}

        <dl className="grid gap-4 sm:grid-cols-2">
          <div>
            <dt className="eyebrow mb-1">{t("plugins.version")}</dt>
            <dd className="font-mono text-sm text-fg">{status?.plugin_version ?? "-"}</dd>
          </div>
          <div>
            <dt className="eyebrow mb-1">{t("plugins.availableRelease")}</dt>
            <dd className="font-mono text-sm text-fg">
              {release
                ? `${release.version} · ${release.channel} · ${release.os} / ${release.arch}`
                : t("plugins.releaseUnavailable")}
            </dd>
          </div>
          {active && status && (
            <div className="sm:col-span-2">
              <dt className="eyebrow mb-1">{t("plugins.features")}</dt>
              <dd className="text-sm text-fg">{status.features.length > 0 ? status.features.join(", ") : "-"}</dd>
            </div>
          )}
        </dl>

        {(monthlyPrice || annualPrice) && (
          <div className="flex flex-wrap gap-2" aria-label={t("plugins.pricing")}>
            {monthlyPrice && <Badge tone="neutral">{t("plugins.monthlyPrice", { price: monthlyPrice })}</Badge>}
            {annualPrice && <Badge tone="neutral">{t("plugins.annualPrice", { price: annualPrice })}</Badge>}
          </div>
        )}

        <div className="flex flex-wrap gap-2">
          {active && (
            <>
              <Button
                variant="outline"
                loading={busyCode === code}
                icon={<RefreshCw className="h-3.5 w-3.5" />}
                onClick={() => onRefresh(code)}
              >
                {t("plugins.refresh")}
              </Button>
              <Button
                variant="outline"
                loading={busyCode === code}
                icon={<XCircle className="h-3.5 w-3.5" />}
                onClick={() => onDeactivate(code)}
              >
                {t("plugins.deactivate")}
              </Button>
            </>
          )}
          {active && manageUrl && (
            <a
              href={manageUrl}
              target="_blank"
              rel="noreferrer"
              className="inline-flex h-9 items-center gap-2 rounded-lg px-3.5 text-sm font-medium text-muted hover:bg-line/40 hover:text-fg"
            >
              {t("plugins.manage")} <ExternalLink className="h-3.5 w-3.5" />
            </a>
          )}
          {!active && hasRequiredLicense && licenseStatusAvailable && entry && (
            <a
              href={purchaseUrl(code)}
              target="_blank"
              rel="noreferrer"
              className="inline-flex h-9 items-center gap-2 rounded-lg px-3.5 text-sm font-medium text-muted hover:bg-line/40 hover:text-fg"
            >
              {t("plugins.buy")} <ExternalLink className="h-3.5 w-3.5" />
            </a>
          )}
        </div>

        {active && status?.available_update && (
          <div className="space-y-3 rounded-xl border border-iris/35 bg-iris/8 px-4 py-3" role="status">
            <div>
              <p className="text-sm font-medium text-fg">
                {t("plugins.updateAvailable", { version: status.available_update.version })}
              </p>
              <p className="mt-1 font-mono text-xs text-muted">
                {t("plugins.updateRelease", { release_id: status.available_update.release_id })}
              </p>
              {status.available_update.changelog && (
                <p className="mt-2 text-xs leading-relaxed text-muted">{status.available_update.changelog}</p>
              )}
            </div>
            <Button
              variant="primary"
              loading={busyCode === code}
              icon={<Download className="h-3.5 w-3.5" />}
              onClick={() => onApplyUpdate(code, status.available_update!.release_id)}
            >
              {t("plugins.updateApply")}
            </Button>
            {updateFailedCode === code && (
              <p role="alert" className="text-xs text-rose">{t("plugins.updateApplyFailed")}</p>
            )}
          </div>
        )}
      </div>
    </Panel>
  );
}

export function Plugins() {
  const { t, locale } = useI18n();
  const plugins = usePlugins();
  const catalog = usePublicPluginCatalog();
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
      await api.post(`/api/ui/plugins/${encodeURIComponent(code)}/refresh`);
    } finally {
      await plugins.refetch();
      setBusyCode(null);
    }
  }

  async function deactivate(code: string) {
    setBusyCode(code);
    try {
      await api.post(`/api/ui/plugins/${encodeURIComponent(code)}/deactivate`);
    } finally {
      await plugins.refetch();
      setBusyCode(null);
    }
  }

  async function applyUpdate(code: string, releaseId: string) {
    setBusyCode(code);
    setUpdateFailedCode(null);
    try {
      await api.post(`/api/ui/plugins/${encodeURIComponent(code)}/update/apply`, { release_id: releaseId });
    } catch {
      setUpdateFailedCode(code);
    } finally {
      await plugins.refetch();
      setBusyCode(null);
    }
  }

  const publicEntries = catalog.data?.plugins ?? [];
  const localStatuses = plugins.data ?? [];
  const localByCode = new Map(localStatuses.map((plugin) => [plugin.code, plugin]));
  const publicCodes = new Set(publicEntries.map((plugin) => plugin.code));
  const cards = [
    ...publicEntries.map((entry) => ({ entry, status: localByCode.get(entry.code) ?? null })),
    ...localStatuses
      .filter((status) => status.active && !publicCodes.has(status.code))
      .map((status) => ({ entry: null, status })),
  ];
  const licenseStatusAvailable = !plugins.isLoading && !plugins.isError && plugins.data !== undefined;

  return (
    <>
      <PageHeader title={t("plugins.title")} subtitle={t("plugins.subtitle")} />

      <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]">
        <div className="space-y-4">
          {catalog.isLoading && (
            <div role="status" aria-label={t("plugins.catalogLoading")} className="space-y-4">
              <Skeleton className="h-52 w-full" />
              <Skeleton className="h-52 w-full" />
            </div>
          )}
          {catalog.isError && (
            <ErrorState
              title={t("plugins.catalogError")}
              error={catalog.error}
              onRetry={() => void catalog.refetch()}
            />
          )}
          {plugins.isError && (
            <ErrorState
              title={t("plugins.licenseStatusError")}
              error={plugins.error}
              onRetry={() => void plugins.refetch()}
            />
          )}

          {cards.map(({ entry, status }) => (
            <PluginCard
              key={entry?.code ?? status?.code}
              entry={entry}
              status={status}
              licenseStatusAvailable={licenseStatusAvailable}
              busyCode={busyCode}
              updateFailedCode={updateFailedCode}
              locale={locale}
              onRefresh={(code) => void refresh(code)}
              onDeactivate={(code) => void deactivate(code)}
              onApplyUpdate={(code, releaseId) => void applyUpdate(code, releaseId)}
            />
          ))}

          {!catalog.isLoading && !catalog.isError && licenseStatusAvailable && cards.length === 0 && (
            <Panel>
              <Empty icon={<Package className="h-5 w-5" />} title={t("plugins.catalogEmpty")} />
            </Panel>
          )}
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
                  plugins: activatedCodes.map((code) => {
                    const plugin = publicEntries.find((entry) => entry.code === code);
                    return plugin?.display_name ?? code;
                  }).join(", "),
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
