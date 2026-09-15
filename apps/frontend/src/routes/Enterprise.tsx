import { CheckCircle2, ExternalLink, KeyRound, RefreshCw, ShieldCheck } from "lucide-react";
import { useState } from "react";
import { Button, ErrorState, Input, PageHeader, Panel, PanelHeader, Skeleton } from "../components/ui";
import { api } from "../lib/api";
import { useEnterpriseStatus } from "../lib/queries";
import { useI18n } from "../i18n/provider";

export function Enterprise() {
  const { t } = useI18n();
  const status = useEnterpriseStatus();
  const [licenseKey, setLicenseKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(false);
  const active = Boolean(status.data?.active);
  const manageUrl = status.data?.manage_url ?? "https://pieceofmind.cloud/enterprise";

  async function activate(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const key = licenseKey.trim();
    if (!key) {
      setError(true);
      return;
    }
    setBusy(true);
    setError(false);
    try {
      await api.post("/api/ui/enterprise/activate", { license_key: key });
      setLicenseKey("");
      await status.refetch();
    } catch {
      setError(true);
    } finally {
      setBusy(false);
    }
  }

  async function refresh() {
    setBusy(true);
    setError(false);
    try {
      await api.post("/api/ui/enterprise/refresh");
      await status.refetch();
    } catch {
      setError(true);
    } finally {
      setBusy(false);
    }
  }

  if (status.isLoading) {
    return <Skeleton className="h-64 w-full" />;
  }

  if (status.isError && !status.data) {
    return <ErrorState error={status.error} onRetry={() => void status.refetch()} />;
  }

  return (
    <>
      <PageHeader title={t("enterprise.activationTitle")} subtitle={t("enterprise.activationSubtitle")} />

      <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]">
        <Panel className={active ? "border-aqua/35 bg-aqua/5" : "border-iris/30"}>
          <PanelHeader
            eyebrow={active ? t("enterprise.activeEyebrow") : t("enterprise.activationEyebrow")}
            title={active ? t("enterprise.activeTitle") : t("enterprise.formTitle")}
            subtitle={active ? t("enterprise.activeSubtitle") : t("enterprise.formSubtitle")}
            actions={active ? <CheckCircle2 className="h-5 w-5 text-aqua" /> : <KeyRound className="h-5 w-5 text-iris" />}
          />

          {active ? (
            <div className="space-y-4 px-5 py-5">
              <dl className="grid gap-4 sm:grid-cols-2">
                <div>
                  <dt className="eyebrow mb-1">{t("enterprise.pluginVersion")}</dt>
                  <dd className="font-mono text-sm text-fg">{status.data?.plugin_version ?? "-"}</dd>
                </div>
                <div>
                  <dt className="eyebrow mb-1">{t("enterprise.licenseState")}</dt>
                  <dd className="text-sm text-fg">{t("enterprise.activeState")}</dd>
                </div>
              </dl>
              <div className="flex flex-wrap gap-2">
                <Button variant="outline" loading={busy} icon={<RefreshCw className="h-3.5 w-3.5" />} onClick={() => void refresh()}>
                  {t("enterprise.refresh")}
                </Button>
                <a href={manageUrl} target="_blank" rel="noreferrer" className="inline-flex h-9 items-center gap-2 rounded-lg px-3.5 text-sm font-medium text-muted hover:bg-line/40 hover:text-fg">
                  {t("enterprise.manageLicense")} <ExternalLink className="h-3.5 w-3.5" />
                </a>
              </div>
            </div>
          ) : (
            <form className="space-y-4 px-5 py-5" onSubmit={(event) => void activate(event)}>
              <label className="block">
                <span className="mb-1.5 block text-[0.8rem] font-medium text-muted">{t("enterprise.licenseKeyLabel")}</span>
                <Input
                  type="password"
                  name="enterprise-license-key"
                  autoComplete="off"
                  aria-label={t("enterprise.licenseKeyLabel")}
                  placeholder={t("enterprise.licenseKeyPlaceholder")}
                  value={licenseKey}
                  onChange={(event) => setLicenseKey(event.target.value)}
                />
              </label>
              {error && <p role="alert" className="text-xs text-rose">{t("enterprise.activationFailed")}</p>}
              <Button variant="primary" type="submit" loading={busy} icon={<ShieldCheck className="h-3.5 w-3.5" />}>
                {t("enterprise.activate")}
              </Button>
              <p className="text-xs leading-relaxed text-faint">{t("enterprise.keyPrivacy")}</p>
            </form>
          )}
        </Panel>

        <Panel className="self-start">
          <PanelHeader title={t("enterprise.includedTitle")} subtitle={t("enterprise.includedSubtitle")} />
          <ul className="space-y-3 px-5 py-5 text-sm leading-relaxed text-muted">
            <li className="flex gap-2.5"><CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0 text-aqua" />{t("enterprise.requestLogs")}</li>
            <li className="flex gap-2.5"><CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0 text-aqua" />{t("enterprise.statistics")}</li>
            <li className="pt-1"><a href={manageUrl} target="_blank" rel="noreferrer" className="text-iris hover:text-iris/80">{t("enterprise.purchaseLink")}</a></li>
          </ul>
        </Panel>
      </div>
    </>
  );
}
