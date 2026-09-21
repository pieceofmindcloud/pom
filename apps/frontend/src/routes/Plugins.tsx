import { ChevronLeft, ChevronRight, Download, ExternalLink, KeyRound, Package, RefreshCw, ShieldCheck, X, XCircle } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import type { Tone } from "../components/ui";
import { Badge, Button, Empty, ErrorState, FullBleedHeader, Input, Panel, PanelHeader, Skeleton } from "../components/ui";
import { Confirm } from "../components/Confirm";
import { api } from "../lib/api";
import { usePlugins, usePublicPluginCatalog } from "../lib/queries";
import type { PluginStatus } from "../lib/types";
import type { PublicPluginCatalogEntry, PublicPluginPrice } from "../lib/pluginCatalog";
import { createPluginCheckout, navigateToCheckout } from "../lib/pluginCheckout";
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

function formatDateTime(value: string | null | undefined, locale: string): string | null {
  if (!value) return null;
  const date = new Date(value);
  if (!Number.isFinite(date.getTime())) return null;
  try {
    return new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "short" }).format(date);
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
  checkoutBusyCode: string | null;
  checkoutFailedCode: string | null;
  deactivateFailedCode: string | null;
  locale: string;
  onRefresh: (code: string) => void;
  onDeactivate: (code: string) => void;
  onApplyUpdate: (code: string, releaseId: string) => void;
  onBuy: (entry: PublicPluginCatalogEntry) => void;
};

function PluginCard({
  entry,
  status,
  licenseStatusAvailable,
  busyCode,
  updateFailedCode,
  checkoutBusyCode,
  checkoutFailedCode,
  deactivateFailedCode,
  locale,
  onRefresh,
  onDeactivate,
  onApplyUpdate,
  onBuy,
}: PluginCardProps) {
  const { t } = useI18n();
  const dialogTitleId = useId();
  const dialogRef = useRef<HTMLDialogElement>(null);
  const closeButtonRef = useRef<HTMLButtonElement>(null);
  const returnFocusRef = useRef<HTMLButtonElement | null>(null);
  const [activeScreenshotIndex, setActiveScreenshotIndex] = useState<number | null>(null);
  const code = entry?.code ?? status?.code ?? "";
  const name = entry?.display_name ?? code;
  const active = Boolean(status?.active);
  const description = entry
    ? locale === "pt-BR" ? entry.description_pt_br ?? entry.description : entry.description
    : null;
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
  const installedAt = formatDateTime(status?.installed_at, locale);
  const updateCheckedAt = formatDateTime(status?.update_checked_at, locale);
  // The license's own validity, not `lease_expires_at` (a short technical
  // renewal deadline the heartbeat rolls forward every few days) - showing
  // the lease here would read as "your license expires tomorrow" while the
  // purchased license is still valid for months.
  const licenseExpiresAt = formatDateTime(status?.license_expires_at, locale);
  const monthlyPrice = formatPrice(entry?.pricing.monthly ?? null, locale);
  const annualPrice = formatPrice(entry?.pricing.annual ?? null, locale);
  const screenshots = entry?.screenshot_urls.slice(0, 4) ?? [];
  const activationUsage = entry?.activation_usage ?? null;
  const machineLimitReached = activationUsage !== null
    && activationUsage.active_machines >= activationUsage.max_machines;
  const machineUsageState = activationUsage === null
    ? null
    : machineLimitReached
      ? "limitReached"
      : activationUsage.active_machines > 0
        ? "partial"
        : "unused";
  const machineUsageTone: Tone = machineLimitReached ? "rose" : activationUsage?.active_machines ? "amber" : "aqua";
  const machineUsageCount = activationUsage
    ? t("plugins.machineUsage.count", {
      active: activationUsage.active_machines,
      max: activationUsage.max_machines,
    })
    : null;
  const hasMissingMetadata = entry !== null && (
    !entry.display_name
    || !description
    || (!entry.icon_url && screenshots.length === 0)
  );

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (activeScreenshotIndex !== null) {
      if (!dialog.open) {
        if (typeof dialog.showModal === "function") dialog.showModal();
        else dialog.setAttribute("open", "");
        closeButtonRef.current?.focus();
      }
      return;
    }
    if (dialog.open) {
      if (typeof dialog.close === "function") dialog.close();
      else dialog.removeAttribute("open");
      returnFocusRef.current?.focus();
    }
  }, [activeScreenshotIndex]);

  useEffect(() => {
    if (activeScreenshotIndex === null) return;
    const navigateScreenshot = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setActiveScreenshotIndex(null);
      } else if (event.key === "ArrowLeft") {
        event.preventDefault();
        moveScreenshot(-1);
      } else if (event.key === "ArrowRight") {
        event.preventDefault();
        moveScreenshot(1);
      }
    };
    document.addEventListener("keydown", navigateScreenshot);
    return () => document.removeEventListener("keydown", navigateScreenshot);
  }, [activeScreenshotIndex, screenshots.length]);

  function moveScreenshot(delta: number) {
    if (activeScreenshotIndex === null || screenshots.length === 0) return;
    setActiveScreenshotIndex((activeScreenshotIndex + delta + screenshots.length) % screenshots.length);
  }

  return (
    <Panel className={active ? "border-aqua/35 bg-aqua/5" : "border-line"}>
      <PanelHeader
        eyebrow={code}
        title={name}
        subtitle={description ?? t("plugins.metadataUnavailable")}
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

        {entry && screenshots.length > 0 && (
          <div className="grid grid-cols-2 gap-2 sm:grid-cols-4" role="group" aria-label={t("plugins.screenshots")}>
            {screenshots.map((url, index) => (
              <button
                key={`${url}:${index}`}
                type="button"
                aria-label={t("plugins.screenshotOpen", { name, number: index + 1 })}
                aria-haspopup="dialog"
                onClick={(event) => {
                  returnFocusRef.current = event.currentTarget;
                  setActiveScreenshotIndex(index);
                }}
                className="group overflow-hidden rounded-lg border border-line bg-raised text-left transition-colors hover:border-iris focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris focus-visible:ring-offset-2 focus-visible:ring-offset-surface"
              >
                <img
                  src={url}
                  alt={t("plugins.screenshotAlt", { name, number: index + 1 })}
                  loading="lazy"
                  referrerPolicy="no-referrer"
                  className="aspect-video w-full object-cover transition-transform duration-200 group-hover:scale-[1.025] motion-reduce:transition-none"
                />
              </button>
            ))}
          </div>
        )}

          {hasRequiredLicense && activationUsage && machineUsageState && machineUsageCount && (
          <div className="space-y-2 rounded-lg border border-line bg-raised/45 px-3.5 py-3" role="group" aria-label={t("plugins.machineUsage.label")}>
            <div className="flex flex-wrap items-center justify-between gap-2">
              <span className="text-sm font-medium text-fg">{machineUsageCount}</span>
              <Badge tone={machineUsageTone}>{t(`plugins.machineUsage.${machineUsageState}`)}</Badge>
            </div>
            <div
              role="progressbar"
              aria-label={t("plugins.machineUsage.label")}
              aria-valuemin={0}
              aria-valuemax={activationUsage.max_machines}
              aria-valuenow={Math.min(activationUsage.active_machines, activationUsage.max_machines)}
              aria-valuetext={machineUsageCount}
              className="h-1.5 overflow-hidden rounded-full bg-line"
            >
              <div
                className={machineLimitReached ? "h-full rounded-full bg-rose" : "h-full rounded-full bg-aqua"}
                style={{ width: `${Math.min(100, activationUsage.active_machines / activationUsage.max_machines * 100)}%` }}
              />
            </div>
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
          {active && installedAt && (
            <div>
              <dt className="eyebrow mb-1">{t("plugins.installedAt")}</dt>
              <dd className="text-sm text-fg">{installedAt}</dd>
            </div>
          )}
          {active && updateCheckedAt && (
            <div>
              <dt className="eyebrow mb-1">{t("plugins.lastUpdateCheck")}</dt>
              <dd className="text-sm text-fg">{updateCheckedAt}</dd>
            </div>
          )}
          {active && licenseExpiresAt && (
            <div>
              <dt className="eyebrow mb-1">{t("plugins.licenseExpires")}</dt>
              <dd className="text-sm text-fg">{licenseExpiresAt}</dd>
            </div>
          )}
        </dl>

        {entry && (
          <dialog
            ref={dialogRef}
            aria-labelledby={dialogTitleId}
            aria-hidden={activeScreenshotIndex === null}
            onCancel={(event) => {
              event.preventDefault();
              setActiveScreenshotIndex(null);
            }}
            className="fixed inset-0 m-0 h-screen max-h-none w-screen max-w-none border-0 bg-transparent p-0 text-fg backdrop:bg-ink/90"
          >
            <div className="relative flex h-full w-full items-center justify-center p-4 sm:p-8">
              <h2 id={dialogTitleId} className="sr-only">{t("plugins.screenshotsTitle", { name })}</h2>
              {activeScreenshotIndex !== null && screenshots[activeScreenshotIndex] && (
                <img
                  src={screenshots[activeScreenshotIndex]}
                  alt={t("plugins.screenshotAlt", { name, number: activeScreenshotIndex + 1 })}
                  className="max-h-full max-w-full rounded-lg object-contain shadow-2xl"
                />
              )}
              <button
                ref={closeButtonRef}
                type="button"
                aria-label={t("plugins.screenshotClose")}
                onClick={() => setActiveScreenshotIndex(null)}
                className="absolute right-4 top-4 inline-flex h-11 w-11 items-center justify-center rounded-full border border-white/20 bg-black/70 text-white hover:bg-black/90 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white"
              >
                <X className="h-5 w-5" aria-hidden="true" />
              </button>
              {screenshots.length > 1 && (
                <>
                  <button
                    type="button"
                    aria-label={t("plugins.screenshotPrevious")}
                    onClick={() => moveScreenshot(-1)}
                    className="absolute left-4 top-1/2 inline-flex h-11 w-11 -translate-y-1/2 items-center justify-center rounded-full border border-white/20 bg-black/70 text-white hover:bg-black/90 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white"
                  >
                    <ChevronLeft className="h-5 w-5" aria-hidden="true" />
                  </button>
                  <button
                    type="button"
                    aria-label={t("plugins.screenshotNext")}
                    onClick={() => moveScreenshot(1)}
                    className="absolute right-4 top-1/2 inline-flex h-11 w-11 -translate-y-1/2 items-center justify-center rounded-full border border-white/20 bg-black/70 text-white hover:bg-black/90 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white"
                  >
                    <ChevronRight className="h-5 w-5" aria-hidden="true" />
                  </button>
                </>
              )}
              {activeScreenshotIndex !== null && (
                <p className="absolute bottom-4 left-1/2 -translate-x-1/2 rounded-full bg-black/70 px-3 py-1 text-xs text-white">
                  {activeScreenshotIndex + 1} / {screenshots.length}
                </p>
              )}
            </div>
          </dialog>
        )}

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
          {!active && hasRequiredLicense && licenseStatusAvailable && entry && (
            <Button
              variant="ghost"
              type="button"
              loading={checkoutBusyCode === code}
              icon={<ExternalLink className="h-4 w-4" />}
              className="plugin-buy-button focus-visible:ring-2 focus-visible:ring-iris focus-visible:ring-offset-2 focus-visible:ring-offset-surface"
              onClick={() => onBuy(entry)}
            >
              {t("plugins.buy")}
            </Button>
          )}
        </div>

        {checkoutFailedCode === code && (
          <p role="alert" className="text-xs text-rose">{t("plugins.checkoutFailed")}</p>
        )}
        {deactivateFailedCode === code && (
          <p role="alert" className="text-xs text-rose">{t("plugins.deactivateFailed")}</p>
        )}

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
  const [deactivateTarget, setDeactivateTarget] = useState<string | null>(null);
  const [deactivateFailedCode, setDeactivateFailedCode] = useState<string | null>(null);
  const [checkoutBusyCode, setCheckoutBusyCode] = useState<string | null>(null);
  const [checkoutFailedCode, setCheckoutFailedCode] = useState<string | null>(null);

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
    setDeactivateFailedCode(null);
    try {
      await api.post(`/api/ui/plugins/${encodeURIComponent(code)}/deactivate`);
      setDeactivateTarget(null);
    } catch {
      setDeactivateFailedCode(code);
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

  async function buy(entry: PublicPluginCatalogEntry) {
    setCheckoutBusyCode(entry.code);
    setCheckoutFailedCode(null);
    try {
      const checkoutUrl = await createPluginCheckout(entry, locale);
      navigateToCheckout(checkoutUrl);
    } catch {
      setCheckoutFailedCode(entry.code);
    } finally {
      setCheckoutBusyCode(null);
    }
  }

  const publicEntries = catalog.data?.plugins ?? [];
  const localStatuses = plugins.data ?? [];
  const localByCode = new Map(localStatuses.map((plugin) => [plugin.code, plugin]));
  const publicCodes = new Set(publicEntries.map((plugin) => plugin.code));
  const activationEligible = publicEntries.some(
    (entry) => entry.requires_license === true && localByCode.get(entry.code)?.active !== true,
  );
  const cards = [
    ...publicEntries.map((entry) => ({ entry, status: localByCode.get(entry.code) ?? null })),
    ...localStatuses
      .filter((status) => status.active && !publicCodes.has(status.code))
      .map((status) => ({ entry: null, status })),
  ];
  const licenseStatusAvailable = !plugins.isLoading && !plugins.isError && plugins.data !== undefined;

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <FullBleedHeader title={t("plugins.title")} subtitle={t("plugins.subtitle")} />

      <div className="min-h-0 min-w-0 flex-1 overflow-auto px-5 py-5">
        <div className={`grid gap-5 ${activationEligible && licenseStatusAvailable ? "lg:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]" : "grid-cols-1"}`}>
          <div className="min-w-0 space-y-4">
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
              checkoutBusyCode={checkoutBusyCode}
              checkoutFailedCode={checkoutFailedCode}
              deactivateFailedCode={deactivateFailedCode}
              locale={locale}
              onRefresh={(code) => void refresh(code)}
              onDeactivate={(code) => {
                setDeactivateFailedCode(null);
                setDeactivateTarget(code);
              }}
              onApplyUpdate={(code, releaseId) => void applyUpdate(code, releaseId)}
              onBuy={(entry) => void buy(entry)}
            />
          ))}

          {!catalog.isLoading && !catalog.isError && licenseStatusAvailable && cards.length === 0 && (
            <Panel>
              <Empty icon={<Package className="h-5 w-5" />} title={t("plugins.catalogEmpty")} />
            </Panel>
          )}
          </div>

        {activationEligible && licenseStatusAvailable && <Panel className="self-start">
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
        </Panel>}
        </div>
      </div>
      <Confirm
        open={deactivateTarget !== null}
        title={t("plugins.deactivateConfirmTitle", {
          name: cards.find(({ status }) => status?.code === deactivateTarget)?.entry?.display_name ?? deactivateTarget ?? "",
        })}
        description={t("plugins.deactivateConfirmDescription")}
        confirmLabel={t("plugins.deactivate")}
        tone="danger"
        busy={deactivateTarget !== null && busyCode === deactivateTarget}
        onConfirm={() => deactivateTarget && void deactivate(deactivateTarget)}
        onCancel={() => setDeactivateTarget(null)}
      />
    </div>
  );
}
