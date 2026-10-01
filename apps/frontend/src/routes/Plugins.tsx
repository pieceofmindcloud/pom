import { ChevronLeft, ChevronRight, Download, ExternalLink, GitBranch, Hammer, KeyRound, Package, RefreshCw, Save, Settings, ShieldCheck, Trash2, X, XCircle } from "lucide-react";
import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import type { Tone } from "../components/ui";
import { Badge, Button, Empty, ErrorState, FullBleedHeader, Input, Panel, PanelHeader, Select, Skeleton } from "../components/ui";
import { Confirm, Modal } from "../components/Confirm";
import { ApiError, api } from "../lib/api";
import { keys, useDevelopmentPluginSources, useGithubRepositories, useNode, usePlugins, usePublicPluginCatalog } from "../lib/queries";
import type { DevelopmentPluginError, DevelopmentPluginRebuildResult, DevelopmentPluginSource, GithubRepository, GithubRepositoryRelease, PluginPreferences, PluginPreferencesSchema, PluginStatus } from "../lib/types";
import { releaseForTarget, type PublicPluginCatalogEntry, type PublicPluginPrice } from "../lib/pluginCatalog";
import { navigateToPluginPurchase } from "../lib/pluginPurchase";
import { pluginAssetUrl, pluginUiSnapshot } from "../lib/pluginUi";
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

function formatDateTime(value: string | null | undefined, locale: string, time = true): string | null {
  if (!value) return null;
  const date = new Date(value);
  if (!Number.isFinite(date.getTime())) return null;
  try {
    return new Intl.DateTimeFormat(locale, time ? { dateStyle: "medium", timeStyle: "short" } : { dateStyle: "medium" }).format(date);
  } catch {
    return null;
  }
}

function developmentError(error: unknown): DevelopmentPluginError {
  if (!(error instanceof ApiError) || !error.body || typeof error.body !== "object") {
    return { code: "unknown" };
  }
  const body = error.body as { error?: unknown; code?: unknown; stage?: unknown; exit_code?: unknown };
  const detail = body.error && typeof body.error === "object"
    ? body.error as { code?: unknown; stage?: unknown; exit_code?: unknown }
    : body;
  return {
    code: typeof detail.code === "string" ? detail.code : "unknown",
    ...(typeof detail.stage === "string" ? { stage: detail.stage as DevelopmentPluginError["stage"] } : {}),
    ...(typeof detail.exit_code === "number" ? { exit_code: detail.exit_code } : {}),
  };
}

function localizedText(texts: Record<string, string> | undefined, locale: string): string | null {
  if (!texts) return null;
  return texts[locale] ?? texts[locale.split("-")[0]] ?? texts.en ?? Object.values(texts)[0] ?? null;
}

/** `v1.2.0` e `1.2.0` são a mesma versão para decidir entre instalar e atualizar. */
function sameVersion(tag: string, version: string | null | undefined): boolean {
  return Boolean(version) && tag.replace(/^v/i, "") === version!.replace(/^v/i, "");
}

/** Releases que este nó consegue instalar, da mais nova para a mais antiga. */
export function compatibleReleases(repository: GithubRepository): GithubRepositoryRelease[] {
  return repository.releases
    .filter((release) => release.available_for_node)
    .map((release, index) => ({ release, index }))
    .sort((left, right) => {
      const a = left.release.published_at ? Date.parse(left.release.published_at) : Number.NaN;
      const b = right.release.published_at ? Date.parse(right.release.published_at) : Number.NaN;
      if (Number.isFinite(a) && Number.isFinite(b) && a !== b) return b - a;
      return left.index - right.index;
    })
    .map(({ release }) => release);
}

function ScreenshotStrip({ name, screenshots }: { name: string; screenshots: string[] }) {
  const { t } = useI18n();
  const dialogTitleId = useId();
  const dialogRef = useRef<HTMLDialogElement>(null);
  const closeButtonRef = useRef<HTMLButtonElement>(null);
  const returnFocusRef = useRef<HTMLButtonElement | null>(null);
  const [active, setActive] = useState<number | null>(null);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (active !== null) {
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
  }, [active]);

  useEffect(() => {
    if (active === null) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setActive(null);
      } else if (event.key === "ArrowLeft") {
        event.preventDefault();
        move(-1);
      } else if (event.key === "ArrowRight") {
        event.preventDefault();
        move(1);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [active, screenshots.length]);

  function move(delta: number) {
    if (active === null || screenshots.length === 0) return;
    setActive((active + delta + screenshots.length) % screenshots.length);
  }

  if (screenshots.length === 0) return null;
  const arrow = "absolute top-1/2 inline-flex h-10 w-10 -translate-y-1/2 items-center justify-center rounded-full border border-white/20 bg-black/70 text-white hover:bg-black/90 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white";
  return (
    <>
      <div className="grid grid-cols-4 gap-1.5" role="group" aria-label={t("plugins.screenshots")}>
        {screenshots.map((url, index) => (
          <button
            key={`${url}:${index}`}
            type="button"
            aria-label={t("plugins.screenshotOpen", { name, number: index + 1 })}
            aria-haspopup="dialog"
            onClick={(event) => {
              returnFocusRef.current = event.currentTarget;
              setActive(index);
            }}
            className="group overflow-hidden rounded-md border border-line bg-raised transition-colors hover:border-iris focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris"
          >
            <img
              src={url}
              alt={t("plugins.screenshotAlt", { name, number: index + 1 })}
              loading="lazy"
              referrerPolicy="no-referrer"
              className="aspect-video w-full object-cover transition-transform duration-200 group-hover:scale-[1.03] motion-reduce:transition-none"
            />
          </button>
        ))}
      </div>
      <dialog
        ref={dialogRef}
        aria-labelledby={dialogTitleId}
        aria-hidden={active === null}
        onCancel={(event) => {
          event.preventDefault();
          setActive(null);
        }}
        className="fixed inset-0 m-0 h-screen max-h-none w-screen max-w-none border-0 bg-transparent p-0 text-fg backdrop:bg-ink/90"
      >
        <div className="relative flex h-full w-full items-center justify-center p-4 sm:p-8">
          <h2 id={dialogTitleId} className="sr-only">{t("plugins.screenshotsTitle", { name })}</h2>
          {active !== null && screenshots[active] && (
            <img
              src={screenshots[active]}
              alt={t("plugins.screenshotAlt", { name, number: active + 1 })}
              referrerPolicy="no-referrer"
              className="max-h-full max-w-full rounded-lg object-contain shadow-2xl"
            />
          )}
          <button
            ref={closeButtonRef}
            type="button"
            aria-label={t("plugins.screenshotClose")}
            onClick={() => setActive(null)}
            className="absolute right-4 top-4 inline-flex h-10 w-10 items-center justify-center rounded-full border border-white/20 bg-black/70 text-white hover:bg-black/90 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white"
          >
            <X className="h-5 w-5" aria-hidden="true" />
          </button>
          {screenshots.length > 1 && (
            <>
              <button type="button" aria-label={t("plugins.screenshotPrevious")} onClick={() => move(-1)} className={`${arrow} left-4`}>
                <ChevronLeft className="h-5 w-5" aria-hidden="true" />
              </button>
              <button type="button" aria-label={t("plugins.screenshotNext")} onClick={() => move(1)} className={`${arrow} right-4`}>
                <ChevronRight className="h-5 w-5" aria-hidden="true" />
              </button>
            </>
          )}
          {active !== null && (
            <p className="absolute bottom-4 left-1/2 -translate-x-1/2 rounded-full bg-black/70 px-3 py-1 text-xs text-white">
              {active + 1} / {screenshots.length}
            </p>
          )}
        </div>
      </dialog>
    </>
  );
}

function PreferencesDialog({
  open,
  name,
  schema,
  saved,
  busy,
  error,
  onSave,
  onClose,
}: {
  open: boolean;
  name: string;
  schema: PluginPreferencesSchema;
  saved: PluginPreferences | null;
  busy: boolean;
  error: string | null;
  onSave: (preferences: PluginPreferences) => void;
  onClose: () => void;
}) {
  const { t } = useI18n();
  const defaults = useMemo<PluginPreferences>(() => ({
    features: {
      ...Object.fromEntries(schema.features.map((preference) => [preference.key, preference.default])),
      ...(saved?.features ?? {}),
    },
    storage_directory: saved ? saved.storage_directory ?? null : schema.storage_directory?.default ?? null,
  }), [schema, saved]);
  const [draft, setDraft] = useState<PluginPreferences>(defaults);
  useEffect(() => {
    if (open) setDraft(defaults);
  }, [open, defaults]);

  return (
    <Modal
      open={open}
      title={t("plugins.preferencesDialogTitle", { name })}
      onClose={onClose}
      footer={(
        <>
          <Button variant="ghost" onClick={onClose}>{t("common.cancel")}</Button>
          <Button variant="primary" loading={busy} icon={<Save className="h-3.5 w-3.5" />} onClick={() => onSave(draft)}>
            {t("plugins.preferencesSave")}
          </Button>
        </>
      )}
    >
      <div className="space-y-4">
        <p className="text-xs text-muted">{t("plugins.preferencesLocalOnly")}</p>
        {schema.features.length > 0 && (
          <fieldset className="space-y-3">
            <legend className="sr-only">{t("plugins.preferencesTitle")}</legend>
            {schema.features.map((preference) => (
              <label key={preference.key} className="flex items-start gap-3 text-sm text-fg">
                <input
                  type="checkbox"
                  checked={draft.features[preference.key] ?? preference.default}
                  onChange={(event) => setDraft({
                    ...draft,
                    features: { ...draft.features, [preference.key]: event.target.checked },
                  })}
                  className="mt-0.5 h-4 w-4 accent-iris focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris"
                />
                <span>
                  <span className="block font-medium">{preference.label}</span>
                  {preference.description && <span className="mt-0.5 block text-xs text-muted">{preference.description}</span>}
                </span>
              </label>
            ))}
          </fieldset>
        )}
        {schema.storage_directory && (
          <label className="block">
            <span className="mb-1.5 block text-xs font-medium text-muted">{schema.storage_directory.label}</span>
            <Input
              type="text"
              value={draft.storage_directory ?? ""}
              onChange={(event) => setDraft({ ...draft, storage_directory: event.target.value.trim() ? event.target.value : null })}
              aria-label={schema.storage_directory.label}
              placeholder="/data/workspace/.plugin"
              autoComplete="off"
            />
            <span className="mt-1 block text-[0.7rem] text-faint">{t("plugins.preferencesDirectoryHint")}</span>
          </label>
        )}
        {error !== null && (
          <p role="alert" className="break-words text-xs text-rose">
            {t("plugins.preferencesSaveFailed")}
            {error && <span className="mt-1 block">{error}</span>}
          </p>
        )}
      </div>
    </Modal>
  );
}

type PluginCardProps = {
  code: string;
  entry: PublicPluginCatalogEntry | null;
  status: PluginStatus | null;
  repository: GithubRepository | null;
  licenseStatus: PluginStatus | null;
  licenseStatusAvailable: boolean;
  busyCode: string | null;
  githubBusyId: string | null;
  githubFailedId: string | null;
  githubFailureMessage: string | null;
  updateFailedCode: string | null;
  deactivateFailedCode: string | null;
  locale: string;
  targetOs: string | null;
  targetArch: string | null;
  onRefresh: (code: string) => void;
  onDeactivate: (code: string) => void;
  onApplyUpdate: (code: string, releaseId: string) => void;
  onOpenPreferences: (code: string) => void;
  onBuy: (entry: PublicPluginCatalogEntry) => void;
  onGithubRefresh: (id: string) => void;
  onGithubRemove: (id: string) => void;
  onGithubInstall: (repository: GithubRepository, release: GithubRepositoryRelease, update: boolean) => void;
};

function PluginCard({
  code,
  entry,
  status,
  repository,
  licenseStatus,
  licenseStatusAvailable,
  busyCode,
  githubBusyId,
  githubFailedId,
  githubFailureMessage,
  updateFailedCode,
  deactivateFailedCode,
  locale,
  targetOs,
  targetArch,
  onRefresh,
  onDeactivate,
  onApplyUpdate,
  onOpenPreferences,
  onBuy,
  onGithubRefresh,
  onGithubRemove,
  onGithubInstall,
}: PluginCardProps) {
  const { t } = useI18n();
  const uiPlugin = pluginUiSnapshot().find((plugin) => plugin.code === code);
  const name = entry?.display_name ?? repository?.manifest.display_name ?? code;
  const active = Boolean(status?.active);
  const description = entry
    ? locale === "pt-BR" ? entry.description_pt_br ?? entry.description : entry.description
    : localizedText(repository?.description, locale)
      ?? localizedText(uiPlugin?.manifest.description, locale)
      ?? repository?.manifest.description
      ?? null;
  const iconUrl = entry?.icon_url
    ?? (uiPlugin?.manifest.icon_image ? pluginAssetUrl(code, uiPlugin.manifest.icon_image) : null);
  const release = entry ? releaseForTarget(entry.releases, targetOs, targetArch) : null;
  const hasRequiredLicense = entry?.requires_license === true;
  const fromGithub = Boolean(repository) || status?.public_install === true;
  const licenseLabel = entry?.requires_license === false || status?.public_install || (!entry && repository)
    ? t("plugins.noLicenseRequired")
    : !licenseStatusAvailable
      ? t("plugins.licenseUnknown")
      : status
        ? t(`plugins.state.${status.state}`)
        : entry?.requires_license === true
          ? t("plugins.state.needs_activation")
          : t("plugins.licenseUnknown");
  const badgeTone = entry?.requires_license === false || status?.public_install || (!entry && repository)
    ? "aqua"
    : status?.state
      ? stateTone(status.state)
      : "neutral";
  const installedAt = formatDateTime(status?.installed_at, locale);
  // A validade da licença, não o `lease_expires_at` técnico que o heartbeat
  // empurra a cada poucos dias.
  const licenseExpiresAt = formatDateTime(
    licenseStatus?.license_expires_at ?? status?.license_expires_at,
    locale,
  );
  const monthlyPrice = formatPrice(entry?.pricing.monthly ?? null, locale);
  const annualPrice = formatPrice(entry?.pricing.annual ?? null, locale);
  const screenshots = (entry?.screenshot_urls.length ? entry.screenshot_urls : repository?.screenshot_urls ?? []).slice(0, 4);
  const isLicenseEnded = status?.state === "expired" || status?.state === "revoked";
  const hasLicense = entry?.requires_license !== false
    && status?.public_install !== true
    && (status?.active === true
      || (!isLicenseEnded && licenseStatus?.licensed_plugins.includes(code) === true));
  const directCount = status?.activation_count;
  const directMaximum = status?.max_activations;
  const directUsage = directCount !== null && directCount !== undefined
    && directMaximum !== null && directMaximum !== undefined && directMaximum > 0
    ? { active: directCount, max: directMaximum }
    : null;
  const sharedUsage = licenseStatus?.plugin_usage.find((usage) => usage.plugin_code === code);
  const activationUsage = hasLicense
    ? directUsage
      ?? (sharedUsage && sharedUsage.max_activations > 0
        ? { active: sharedUsage.activation_count, max: sharedUsage.max_activations }
        : null)
    : null;
  const machineLimitReached = activationUsage !== null && activationUsage.active >= activationUsage.max;
  const machineUsageState = activationUsage === null
    ? null
    : machineLimitReached ? "limitReached" : activationUsage.active > 0 ? "partial" : "unused";
  const machineUsageTone: Tone = machineLimitReached ? "rose" : activationUsage?.active ? "amber" : "aqua";
  const machineUsageCount = activationUsage
    ? t("plugins.activationUsage.count", { active: activationUsage.active, max: activationUsage.max })
    : null;
  const preferenceSchema = status?.preferences_schema ?? null;

  // GitHub: só releases instaláveis neste nó; a mais nova vem marcada.
  const releases = repository ? compatibleReleases(repository) : [];
  const [selectedRelease, setSelectedRelease] = useState<string>(releases[0]?.release_id ?? "");
  const releaseKey = releases.map((item) => item.release_id).join(",");
  useEffect(() => {
    if (!releases.some((item) => item.release_id === selectedRelease)) {
      setSelectedRelease(releases[0]?.release_id ?? "");
    }
  }, [releaseKey]);
  const chosen = releases.find((item) => item.release_id === selectedRelease) ?? null;
  const installedFromThisRepository = Boolean(
    repository && status?.public_install && status.github_repository === repository.repository_url,
  );
  const sourceConflict = Boolean(repository && status?.public_install && status.github_repository !== repository.repository_url);
  const licensedConflict = Boolean(repository && status && status.public_install !== true && (
    status.active || status.licensed_plugins.includes(code)
  ));
  const chosenIsInstalled = Boolean(chosen && installedFromThisRepository && sameVersion(chosen.tag_name, status?.plugin_version));
  const githubBusy = repository !== null && githubBusyId === repository.id;

  return (
    <Panel className={`flex h-full flex-col ${active ? "border-aqua/35 bg-aqua/5" : "border-line"}`}>
      <div className="flex items-start gap-3 px-4 pt-4">
        {iconUrl ? (
          <img
            src={iconUrl}
            alt={name}
            loading="lazy"
            referrerPolicy="no-referrer"
            className="h-10 w-10 shrink-0 rounded-xl border border-line object-cover"
          />
        ) : (
          <span aria-hidden="true" className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl border border-line bg-raised text-muted">
            <Package className="h-5 w-5" />
          </span>
        )}
        <div className="min-w-0 flex-1">
          <h2 className="truncate font-display text-sm font-semibold text-fg">{name}</h2>
          <p className="mt-0.5 truncate font-mono text-[0.68rem] text-faint">{code}</p>
          <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
            <Badge tone={badgeTone}>{licenseLabel}</Badge>
            {fromGithub && <Badge tone="neutral">GitHub</Badge>}
          </div>
        </div>
        <div className="flex shrink-0 items-center gap-0.5">
          {repository && (
            <>
              <button
                type="button"
                aria-label={t("plugins.githubCheck")}
                title={t("plugins.githubCheck")}
                disabled={githubBusy}
                onClick={() => onGithubRefresh(repository.id)}
                className="inline-flex h-8 w-8 items-center justify-center rounded-lg text-muted hover:bg-line/40 hover:text-fg disabled:opacity-50"
              >
                <RefreshCw className={`h-3.5 w-3.5 ${githubBusy ? "animate-spin" : ""}`} aria-hidden="true" />
              </button>
              <button
                type="button"
                aria-label={t("plugins.githubRemove")}
                title={t("plugins.githubRemove")}
                disabled={githubBusy}
                onClick={() => onGithubRemove(repository.id)}
                className="inline-flex h-8 w-8 items-center justify-center rounded-lg text-muted hover:bg-line/40 hover:text-rose disabled:opacity-50"
              >
                <Trash2 className="h-3.5 w-3.5" aria-hidden="true" />
              </button>
            </>
          )}
          {active && preferenceSchema && (
            <button
              type="button"
              aria-label={t("plugins.preferencesOpen", { name })}
              title={t("plugins.preferencesTitle")}
              onClick={() => onOpenPreferences(code)}
              className="inline-flex h-8 w-8 items-center justify-center rounded-lg text-muted hover:bg-line/40 hover:text-fg"
            >
              <Settings className="h-4 w-4" aria-hidden="true" />
            </button>
          )}
        </div>
      </div>

      <div className="flex flex-1 flex-col gap-3 px-4 pb-4 pt-3">
        {description ? (
          <p className="line-clamp-3 text-xs leading-relaxed text-muted" title={description}>{description}</p>
        ) : (
          <p className="text-xs text-faint">{t("plugins.metadataUnavailable")}</p>
        )}
        {status?.installation_error && (
          <div role="alert" className="space-y-1 rounded-lg border border-rose/35 bg-rose/5 px-3 py-2 text-xs text-rose">
            <p className="font-medium">{t("plugins.installationFailed")}</p>
            <p className="break-words">{status.installation_error}</p>
          </div>
        )}
        {repository && (
          <a className="block truncate text-[0.7rem] text-iris underline" href={repository.repository_url} target="_blank" rel="noreferrer">
            {repository.repository_url.replace(/^https:\/\//, "")}
          </a>
        )}

        <ScreenshotStrip name={name} screenshots={screenshots} />

        {hasRequiredLicense && activationUsage && machineUsageState && machineUsageCount && (
          <div className="space-y-1.5 rounded-lg border border-line bg-raised/45 px-3 py-2" role="group" aria-label={t("plugins.activationUsage.label")}>
            <div className="flex flex-wrap items-center justify-between gap-2">
              <span className="text-xs font-medium text-fg">{machineUsageCount}</span>
              <Badge tone={machineUsageTone}>{t(`plugins.activationUsage.${machineUsageState}`)}</Badge>
            </div>
            <div
              role="progressbar"
              aria-label={t("plugins.activationUsage.label")}
              aria-valuemin={0}
              aria-valuemax={activationUsage.max}
              aria-valuenow={Math.min(activationUsage.active, activationUsage.max)}
              aria-valuetext={machineUsageCount}
              className="h-1 overflow-hidden rounded-full bg-line"
            >
              <div
                className={machineLimitReached ? "h-full rounded-full bg-rose" : "h-full rounded-full bg-aqua"}
                style={{ width: `${Math.min(100, activationUsage.active / activationUsage.max * 100)}%` }}
              />
            </div>
          </div>
        )}

        <dl className="grid grid-cols-2 gap-x-3 gap-y-2 text-xs">
          <div>
            <dt className="eyebrow mb-0.5 text-[0.6rem]">{t("plugins.version")}</dt>
            <dd className="font-mono text-fg">{status?.plugin_version ?? "-"}</dd>
          </div>
          {release && (
            <div>
              <dt className="eyebrow mb-0.5 text-[0.6rem]">{t("plugins.availableRelease")}</dt>
              <dd className="font-mono text-fg">{`${release.version} · ${release.channel} · ${release.os} / ${release.arch}`}</dd>
            </div>
          )}
          {active && installedAt && (
            <div>
              <dt className="eyebrow mb-0.5 text-[0.6rem]">{t("plugins.installedAt")}</dt>
              <dd className="text-fg">{installedAt}</dd>
            </div>
          )}
          {hasLicense && licenseExpiresAt && (
            <div>
              <dt className="eyebrow mb-0.5 text-[0.6rem]">{t("plugins.licenseExpires")}</dt>
              <dd className="text-fg">{licenseExpiresAt}</dd>
            </div>
          )}
        </dl>

        {(monthlyPrice || annualPrice) && (
          <div className="flex flex-wrap gap-1.5" aria-label={t("plugins.pricing")}>
            {monthlyPrice && <Badge tone="neutral">{t("plugins.monthlyPrice", { price: monthlyPrice })}</Badge>}
            {annualPrice && <Badge tone="neutral">{t("plugins.annualPrice", { price: annualPrice })}</Badge>}
          </div>
        )}

        {repository && (
          <div className="space-y-2 rounded-lg border border-line bg-raised/30 p-2.5">
            {releases.length === 0 ? (
              <p className="text-xs text-muted">{t("plugins.githubNoCompatibleReleases")}</p>
            ) : (
              <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
                <label className="min-w-0 flex-1">
                  <span className="sr-only">{t("plugins.githubReleaseSelect")}</span>
                  <Select
                    compact
                    aria-label={t("plugins.githubReleaseSelect")}
                    value={selectedRelease}
                    onChange={(event) => setSelectedRelease(event.target.value)}
                  >
                    {releases.map((item, index) => (
                      <option key={item.release_id} value={item.release_id}>
                        {[
                          item.tag_name,
                          item.prerelease ? t("plugins.githubPrerelease") : null,
                          index === 0 ? t("plugins.githubLatest") : null,
                          formatDateTime(item.published_at, locale, false),
                        ].filter(Boolean).join(" · ")}
                      </option>
                    ))}
                  </Select>
                </label>
                <Button
                  variant="primary"
                  className="shrink-0 whitespace-nowrap"
                  disabled={!chosen || chosenIsInstalled || sourceConflict || licensedConflict}
                  loading={githubBusy}
                  icon={<Download className="h-3.5 w-3.5" />}
                  onClick={() => chosen && onGithubInstall(repository, chosen, installedFromThisRepository)}
                >
                  {chosenIsInstalled
                    ? t("plugins.githubInstalled")
                    : installedFromThisRepository ? t("plugins.githubUpdate") : t("plugins.githubInstall")}
                </Button>
              </div>
            )}
            {chosen?.changelog && (
              <p className="line-clamp-3 whitespace-pre-wrap break-words text-[0.7rem] leading-relaxed text-faint">{chosen.changelog}</p>
            )}
            {repository.check_error && <p role="alert" className="break-words text-xs text-rose">{t("plugins.githubCheckFailed")} {repository.check_error}</p>}
            {githubFailedId === repository.id && <p role="alert" className="break-words text-xs text-rose">{t("plugins.githubInstallFailed")} {githubFailureMessage}</p>}
          </div>
        )}

        <div className="mt-auto flex flex-wrap gap-2">
          {active && status?.public_install !== true && (
            <>
              <Button variant="outline" loading={busyCode === code} icon={<RefreshCw className="h-3.5 w-3.5" />} onClick={() => onRefresh(code)}>
                {t("plugins.refresh")}
              </Button>
              <Button variant="outline" loading={busyCode === code} icon={<XCircle className="h-3.5 w-3.5" />} onClick={() => onDeactivate(code)}>
                {t("plugins.deactivate")}
              </Button>
            </>
          )}
          {active && status?.public_install === true && (
            <Button variant="outline" loading={busyCode === code} icon={<XCircle className="h-3.5 w-3.5" />} onClick={() => onDeactivate(code)}>
              {t("plugins.githubUninstall")}
            </Button>
          )}
          {!active && hasRequiredLicense && licenseStatusAvailable && entry && !hasLicense && (
            <Button
              variant="ghost"
              type="button"
              icon={<ExternalLink className="h-4 w-4" />}
              className="plugin-buy-button focus-visible:ring-2 focus-visible:ring-iris focus-visible:ring-offset-2 focus-visible:ring-offset-surface"
              onClick={() => onBuy(entry)}
            >
              {t("plugins.buy")}
            </Button>
          )}
        </div>

        {deactivateFailedCode === code && (
          <p role="alert" className="text-xs text-rose">{t("plugins.deactivateFailed")}</p>
        )}

        {active && status?.available_update && (
          <div className="space-y-2 rounded-lg border border-iris/35 bg-iris/8 px-3 py-2.5" role="status">
            <p className="text-xs font-medium text-fg">
              {t("plugins.updateAvailable", { version: status.available_update.version })}
            </p>
            <p className="font-mono text-[0.68rem] text-muted">
              {t("plugins.updateRelease", { release_id: status.available_update.release_id })}
            </p>
            {status.available_update.changelog && (
              <p className="text-[0.7rem] leading-relaxed text-muted">{status.available_update.changelog}</p>
            )}
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

function GithubSourceCard({
  busy,
  failed,
  failureMessage,
  listError,
  onAdd,
}: {
  busy: boolean;
  failed: boolean;
  failureMessage: string | null;
  listError: Error | null;
  onAdd: (repositoryUrl: string) => void;
}) {
  const { t } = useI18n();
  const [repositoryUrl, setRepositoryUrl] = useState("");
  return (
    <Panel>
      <PanelHeader
        eyebrow="GitHub"
        title={t("plugins.githubSourceTitle")}
        subtitle={t("plugins.githubSourceDescription")}
        actions={<GitBranch className="h-5 w-5 text-iris" />}
      />
      <form
        className="space-y-3 px-5 py-4"
        onSubmit={(event) => {
          event.preventDefault();
          onAdd(repositoryUrl.trim());
          setRepositoryUrl("");
        }}
      >
        <label className="block">
          <span className="sr-only">{t("plugins.githubRepositoryLabel")}</span>
          <Input
            type="url"
            value={repositoryUrl}
            onChange={(event) => setRepositoryUrl(event.target.value)}
            placeholder="https://github.com/owner/repository"
            aria-label={t("plugins.githubRepositoryLabel")}
            autoComplete="url"
            required
          />
        </label>
        <Button variant="outline" type="submit" loading={busy}>
          {t("plugins.githubSourceRegister")}
        </Button>
        {listError && (
          <p role="alert" className="break-words text-xs text-rose">{t("plugins.githubListFailed")} {listError.message}</p>
        )}
        {failed && (
          <p role="alert" className="text-xs text-rose">
            {t("plugins.githubSourceFailed")}
            {failureMessage && <span className="mt-1 block break-words">{failureMessage}</span>}
          </p>
        )}
      </form>
    </Panel>
  );
}

export function Plugins() {
  const { t, locale } = useI18n();
  const queryClient = useQueryClient();
  const plugins = usePlugins();
  const catalog = usePublicPluginCatalog();
  const githubRepositories = useGithubRepositories();
  const node = useNode();
  const developmentEnabled = node.data?.profile === "admin" && node.data?.pom_desenv === true;
  const developmentSources = useDevelopmentPluginSources(developmentEnabled);
  const [licenseKey, setLicenseKey] = useState("");
  const [activateBusy, setActivateBusy] = useState(false);
  const [activateError, setActivateError] = useState(false);
  const [activatedCodes, setActivatedCodes] = useState<string[] | null>(null);
  const [busyCode, setBusyCode] = useState<string | null>(null);
  const [updateFailedCode, setUpdateFailedCode] = useState<string | null>(null);
  const [githubBusyId, setGithubBusyId] = useState<string | null>(null);
  const [githubFailedId, setGithubFailedId] = useState<string | null>(null);
  const [githubFailureMessage, setGithubFailureMessage] = useState<string | null>(null);
  const [developmentBusyRepository, setDevelopmentBusyRepository] = useState<string | null>(null);
  const [developmentFailure, setDevelopmentFailure] = useState<{ repository: string; error: DevelopmentPluginError } | null>(null);
  const [developmentResult, setDevelopmentResult] = useState<DevelopmentPluginRebuildResult | null>(null);
  const [installTarget, setInstallTarget] = useState<{ repository: GithubRepository; release: GithubRepositoryRelease; update: boolean } | null>(null);
  const [preferencesCode, setPreferencesCode] = useState<string | null>(null);
  const [preferencesError, setPreferencesError] = useState<string | null>(null);
  const [deactivateTarget, setDeactivateTarget] = useState<string | null>(null);
  const [deactivateFailedCode, setDeactivateFailedCode] = useState<string | null>(null);

  async function refreshPluginData() {
    await Promise.all([
      plugins.refetch(),
      queryClient.invalidateQueries({ queryKey: keys.pluginUi }),
    ]);
  }

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
      await refreshPluginData();
    } catch {
      setActivateError(true);
      await refreshPluginData();
    } finally {
      setActivateBusy(false);
    }
  }

  async function refresh(code: string) {
    setBusyCode(code);
    try {
      await api.post(`/api/ui/plugins/${encodeURIComponent(code)}/refresh`);
    } finally {
      await refreshPluginData();
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
      await refreshPluginData();
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
      await refreshPluginData();
      setBusyCode(null);
    }
  }

  async function runGithubAction(id: string, action: () => Promise<unknown>) {
    setGithubBusyId(id);
    setGithubFailedId(null);
    setGithubFailureMessage(null);
    try {
      await action();
    } catch (error) {
      setGithubFailedId(id);
      setGithubFailureMessage(error instanceof Error ? error.message : null);
    } finally {
      await Promise.all([githubRepositories.refetch(), refreshPluginData()]);
      setGithubBusyId(null);
    }
  }

  async function rebuildDevelopmentPlugin(source: DevelopmentPluginSource) {
    setDevelopmentBusyRepository(source.repository);
    setDevelopmentFailure(null);
    setDevelopmentResult(null);
    try {
      const result = await api.post<DevelopmentPluginRebuildResult>(
        `/api/ui/plugins/dev-sources/${encodeURIComponent(source.repository)}/rebuild-install`,
      );
      setDevelopmentResult(result);
      await Promise.all([developmentSources.refetch(), refreshPluginData()]);
    } catch (error) {
      setDevelopmentFailure({ repository: source.repository, error: developmentError(error) });
    } finally {
      setDevelopmentBusyRepository(null);
    }
  }

  function developmentErrorText(error: DevelopmentPluginError): string {
    let message: string;
    switch (error.code) {
      case "development_disabled": message = t("plugins.devDisabled"); break;
      case "unknown_repository": message = t("plugins.devUnknownRepository"); break;
      case "mount_missing": message = t("plugins.devMountMissing"); break;
      case "invalid_source": message = t("plugins.devInvalidSource"); break;
      case "plugin_not_active": message = t("plugins.devPluginInactive"); break;
      case "build_failed": message = t("plugins.devBuildFailed", { stage: error.stage ?? "build" }); break;
      case "artifact_missing": message = t("plugins.devArtifactMissing"); break;
      case "install_failed": message = t("plugins.devInstallFailed"); break;
      default: message = t("plugins.devUnknownFailure");
    }
    return typeof error.exit_code === "number"
      ? `${message} ${t("plugins.devExitCode", { code: error.exit_code })}`
      : message;
  }

  async function savePreferences(code: string, preferences: PluginPreferences) {
    setBusyCode(code);
    setPreferencesError(null);
    try {
      await api.post(`/api/ui/plugins/${encodeURIComponent(code)}/preferences`, { preferences });
      setPreferencesCode(null);
    } catch (error) {
      // A mensagem do nó diz o que recusou (diretório inexistente, chave
      // desconhecida...); o texto genérico sozinho não deixava corrigir nada.
      setPreferencesError(error instanceof Error ? error.message : "");
    } finally {
      await refreshPluginData();
      setBusyCode(null);
    }
  }

  const publicEntries = catalog.data?.plugins ?? [];
  const localStatuses = plugins.data ?? [];
  const repositories = githubRepositories.data ?? [];
  const localByCode = new Map(localStatuses.map((plugin) => [plugin.code, plugin]));
  const repositoryByCode = new Map(repositories.map((repository) => [repository.manifest.plugin_code, repository]));
  const licenseStatusByCode = new Map<string, PluginStatus>();
  for (const source of localStatuses) {
    if (source.licensed_plugins.includes(source.code)) licenseStatusByCode.set(source.code, source);
  }
  for (const source of [...localStatuses].sort((left, right) => Number(right.active) - Number(left.active) || left.code.localeCompare(right.code))) {
    for (const code of new Set(source.licensed_plugins)) {
      if (!licenseStatusByCode.has(code)) licenseStatusByCode.set(code, source);
    }
  }
  const shouldShowActivation = publicEntries.some((entry) => {
    if (entry.requires_license !== true) return false;
    const status = localByCode.get(entry.code);
    return status?.active !== true || status.state === "expired" || status.state === "revoked";
  });
  // Um card por `plugin_code`: catálogo comercial, repositório GitHub e estado
  // local ativo se somam no mesmo card quando falam do mesmo plugin.
  const codes: string[] = [];
  const addCode = (code: string) => {
    if (!codes.includes(code)) codes.push(code);
  };
  publicEntries.forEach((entry) => addCode(entry.code));
  repositories.forEach((repository) => addCode(repository.manifest.plugin_code));
  localStatuses.filter((status) => status.active).forEach((status) => addCode(status.code));
  const cards = codes.map((code) => ({
    code,
    entry: publicEntries.find((entry) => entry.code === code) ?? null,
    status: localByCode.get(code) ?? null,
    repository: repositoryByCode.get(code) ?? null,
    licenseStatus: licenseStatusByCode.get(code) ?? localByCode.get(code) ?? null,
  }));
  const licenseStatusAvailable = !plugins.isLoading && !plugins.isError && plugins.data !== undefined;
  const targetOs = node.data?.system.os ?? null;
  const targetArch = node.data?.system.arch ?? null;
  const deactivateCard = cards.find(({ status }) => status?.code === deactivateTarget);
  const deactivateName = deactivateCard?.entry?.display_name ?? deactivateCard?.repository?.manifest.display_name ?? deactivateTarget ?? "";
  const uninstallingPublicPlugin = deactivateCard?.status?.public_install === true;
  const preferencesCard = cards.find(({ code }) => code === preferencesCode);
  const preferencesSchema = preferencesCard?.status?.preferences_schema ?? null;

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <FullBleedHeader title={t("plugins.title")} subtitle={t("plugins.subtitle")} />

      <div className="min-h-0 min-w-0 flex-1 overflow-auto px-5 py-5">
        <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,21rem)]">
          <div className="order-2 min-w-0 space-y-4 lg:order-1">
            {catalog.isLoading && (
              <div role="status" aria-label={t("plugins.catalogLoading")} className="grid gap-4 xl:grid-cols-2">
                <Skeleton className="h-44 w-full" />
                <Skeleton className="h-44 w-full" />
              </div>
            )}
            {catalog.isError && (
              <ErrorState title={t("plugins.catalogError")} error={catalog.error} onRetry={() => void catalog.refetch()} />
            )}
            {plugins.isError && (
              <ErrorState title={t("plugins.licenseStatusError")} error={plugins.error} onRetry={() => void refreshPluginData()} />
            )}
            {githubRepositories.isLoading && repositories.length === 0 && (
              <p role="status" className="text-sm text-muted">{t("plugins.githubRepositoriesLoading")}</p>
            )}

            {cards.length > 0 && (
              <div className="grid gap-4 xl:grid-cols-2">
                {cards.map(({ code, entry, status, repository, licenseStatus }) => (
                  <PluginCard
                    key={code}
                    code={code}
                    entry={entry}
                    status={status}
                    repository={repository}
                    licenseStatus={licenseStatus}
                    licenseStatusAvailable={licenseStatusAvailable}
                    busyCode={busyCode}
                    githubBusyId={githubBusyId}
                    githubFailedId={githubFailedId}
                    githubFailureMessage={githubFailureMessage}
                    updateFailedCode={updateFailedCode}
                    deactivateFailedCode={deactivateFailedCode}
                    locale={locale}
                    targetOs={targetOs}
                    targetArch={targetArch}
                    onRefresh={(target) => void refresh(target)}
                    onDeactivate={(target) => {
                      setDeactivateFailedCode(null);
                      setDeactivateTarget(target);
                    }}
                    onApplyUpdate={(target, releaseId) => void applyUpdate(target, releaseId)}
                    onOpenPreferences={(target) => {
                      setPreferencesError(null);
                      setPreferencesCode(target);
                    }}
                    onBuy={(target) => navigateToPluginPurchase(target.code)}
                    onGithubRefresh={(id) => void runGithubAction(id, () => api.post(`/api/ui/plugins/github-repositories/${encodeURIComponent(id)}/refresh`))}
                    onGithubRemove={(id) => void runGithubAction(id, () => api.del(`/api/ui/plugins/github-repositories/${encodeURIComponent(id)}`))}
                    onGithubInstall={(repository, release, update) => setInstallTarget({ repository, release, update })}
                  />
                ))}
              </div>
            )}

            {!catalog.isLoading && !catalog.isError && licenseStatusAvailable && !githubRepositories.isLoading
              && repositories.length === 0 && cards.length === 0 && (
              <Panel>
                <Empty icon={<Package className="h-5 w-5" />} title={t("plugins.catalogEmpty")} />
              </Panel>
            )}
          </div>

          <div className="order-1 min-w-0 space-y-4 self-start lg:order-2">
            {developmentEnabled && (
              <Panel>
                <PanelHeader
                  eyebrow="Development"
                  title={t("plugins.devTitle")}
                  subtitle={t("plugins.devDescription")}
                  actions={<Hammer className="h-5 w-5 text-iris" aria-hidden="true" />}
                />
                <div className="space-y-3 px-5 py-4">
                  {developmentSources.isLoading && (
                    <p role="status" className="text-xs text-muted">{t("plugins.devLoading")}</p>
                  )}
                  {developmentSources.isError && (
                    <p role="alert" className="text-xs text-rose">{t("plugins.devListFailed")}</p>
                  )}
                  {developmentSources.data && !developmentSources.data.mount_available && (
                    <p role="status" className="text-xs text-muted">{t("plugins.devMountMissing")}</p>
                  )}
                  {developmentSources.data?.mount_available && developmentSources.data.sources.length === 0 && (
                    <p role="status" className="text-xs text-muted">{t("plugins.devNoSources")}</p>
                  )}
                  {developmentSources.data?.sources.map((source) => (
                    <div key={source.repository} className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-line bg-raised/30 p-3">
                      <div className="min-w-0">
                        <p className="truncate text-sm font-medium text-fg">{source.display_name}</p>
                        <p className="truncate font-mono text-[0.68rem] text-faint">{source.repository} · {source.plugin_code}</p>
                        {!source.active && <p className="mt-1 text-[0.68rem] text-muted">{t("plugins.devPluginInactive")}</p>}
                      </div>
                      <Button
                        variant="outline"
                        disabled={!source.active || developmentBusyRepository !== null}
                        loading={developmentBusyRepository === source.repository}
                        icon={<Hammer className="h-3.5 w-3.5" aria-hidden="true" />}
                        onClick={() => void rebuildDevelopmentPlugin(source)}
                      >
                        {developmentBusyRepository === source.repository ? t("plugins.devBuilding") : t("plugins.devRebuildInstall")}
                      </Button>
                    </div>
                  ))}
                  {developmentResult && (
                    <p role="status" className="text-xs text-aqua">
                      {t("plugins.devSucceeded", {
                        name: developmentSources.data?.sources.find((source) => source.repository === developmentResult.repository)?.display_name ?? developmentResult.plugin_code,
                        duration: developmentResult.build_ms,
                      })}
                    </p>
                  )}
                  {developmentFailure && (
                    <p role="alert" className="break-words text-xs text-rose">
                      {developmentSources.data?.sources.find((source) => source.repository === developmentFailure.repository)?.display_name ?? developmentFailure.repository}: {developmentErrorText(developmentFailure.error)}
                    </p>
                  )}
                </div>
              </Panel>
            )}
            <GithubSourceCard
              busy={githubBusyId === "add"}
              failed={githubFailedId === "add"}
              failureMessage={githubFailureMessage}
              listError={githubRepositories.error}
              onAdd={(repositoryUrl) => void runGithubAction("add", () => api.post("/api/ui/plugins/github-repositories", {
                repository_url: repositoryUrl,
              }))}
            />
            {shouldShowActivation && (
              <Panel>
                <PanelHeader
                  eyebrow={t("plugins.activationEyebrow")}
                  title={t("plugins.formTitle")}
                  subtitle={t("plugins.formSubtitle")}
                  actions={<KeyRound className="h-5 w-5 text-iris" />}
                />
                <form className="space-y-3 px-5 py-4" onSubmit={(event) => void activate(event)}>
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
                        plugins: activatedCodes.map((code) => publicEntries.find((entry) => entry.code === code)?.display_name ?? code).join(", "),
                      })}
                    </p>
                  )}
                  <Button variant="primary" type="submit" loading={activateBusy} icon={<ShieldCheck className="h-3.5 w-3.5" />}>
                    {t("plugins.activate")}
                  </Button>
                  <p className="text-xs leading-relaxed text-faint">{t("plugins.keyPrivacy")}</p>
                </form>
              </Panel>
            )}
          </div>
        </div>
      </div>

      {preferencesCard && preferencesSchema && (
        <PreferencesDialog
          open
          name={preferencesCard.entry?.display_name ?? preferencesCard.repository?.manifest.display_name ?? preferencesCard.code}
          schema={preferencesSchema}
          saved={preferencesCard.status?.preferences ?? null}
          busy={busyCode === preferencesCard.code}
          error={preferencesError}
          onSave={(preferences) => void savePreferences(preferencesCard.code, preferences)}
          onClose={() => setPreferencesCode(null)}
        />
      )}
      <Confirm
        open={installTarget !== null}
        title={installTarget
          ? t(installTarget.update ? "plugins.githubUpdateConfirmTitle" : "plugins.githubInstallConfirmTitle", { name: installTarget.repository.manifest.display_name })
          : ""}
        description={installTarget ? t("plugins.githubInstallConfirmDescription", { version: installTarget.release.tag_name }) : ""}
        confirmLabel={installTarget?.update ? t("plugins.githubUpdate") : t("plugins.githubInstall")}
        busy={installTarget !== null && githubBusyId === installTarget.repository.id}
        onConfirm={() => {
          if (!installTarget) return;
          const { repository, release } = installTarget;
          setInstallTarget(null);
          void runGithubAction(repository.id, () => api.post(`/api/ui/plugins/github-repositories/${encodeURIComponent(repository.id)}/install`, {
            release_id: release.release_id,
          }));
        }}
        onCancel={() => setInstallTarget(null)}
      />
      <Confirm
        open={deactivateTarget !== null}
        title={uninstallingPublicPlugin
          ? t("plugins.githubUninstallConfirmTitle", { name: deactivateName })
          : t("plugins.deactivateConfirmTitle", { name: deactivateName })}
        description={uninstallingPublicPlugin
          ? t("plugins.githubUninstallConfirmDescription")
          : t("plugins.deactivateConfirmDescription")}
        confirmLabel={uninstallingPublicPlugin ? t("plugins.githubUninstall") : t("plugins.deactivate")}
        tone="danger"
        busy={deactivateTarget !== null && busyCode === deactivateTarget}
        onConfirm={() => deactivateTarget && void deactivate(deactivateTarget)}
        onCancel={() => setDeactivateTarget(null)}
      />
    </div>
  );
}
