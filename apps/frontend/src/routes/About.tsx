import { ExternalLink, Info, PackageOpen, RefreshCw, Scale, Search } from "lucide-react";
import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { Badge, Button, Input, PageHeader, Panel, PanelHeader } from "../components/ui";
import { useI18n } from "../i18n/provider";
import { api } from "../lib/api";
import { POM_LICENSE, THIRD_PARTY_LICENSES, type LicenseEntry } from "../lib/licenses.generated";
import { useEnterpriseStatus } from "../lib/queries";

function licenseGroups(entries: readonly LicenseEntry[]) {
  const groups = new Map<string, LicenseEntry[]>();
  for (const entry of entries) {
    const group = groups.get(entry.license) ?? [];
    group.push(entry);
    groups.set(entry.license, group);
  }
  return [...groups.entries()].sort(([a], [b]) => a.localeCompare(b));
}

export function About() {
  const { t } = useI18n();
  const enterpriseStatus = useEnterpriseStatus();
  const [query, setQuery] = useState("");
  const [checkingUpdate, setCheckingUpdate] = useState(false);
  const [updateCheckFailed, setUpdateCheckFailed] = useState(false);
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const enterpriseActive = Boolean(enterpriseStatus.data?.active);
  const enterpriseState = enterpriseStatus.data?.state ?? "unavailable";
  const manageUrl = enterpriseStatus.data?.manage_url ?? "https://pieceofmind.cloud/enterprise";
  const filteredEntries = useMemo(
    () => THIRD_PARTY_LICENSES.filter((entry) => {
      if (!normalizedQuery) return true;
      return [entry.name, entry.license, entry.version, entry.ecosystem]
        .some((value) => value.toLocaleLowerCase().includes(normalizedQuery));
    }),
    [normalizedQuery],
  );
  const groups = useMemo(() => licenseGroups(filteredEntries), [filteredEntries]);

  async function checkForUpdates() {
    setCheckingUpdate(true);
    setUpdateCheckFailed(false);
    try {
      await api.post("/api/ui/enterprise/pom-update/check");
      await enterpriseStatus.refetch();
    } catch {
      setUpdateCheckFailed(true);
    } finally {
      setCheckingUpdate(false);
    }
  }

  return (
    <>
      <PageHeader title={t("about.title")} subtitle={t("about.subtitle")} />

      <div className="space-y-5">
        <Panel data-license-status={POM_LICENSE.status} className="overflow-hidden border-amber/35 bg-amber/5">
          <PanelHeader
            eyebrow="POM"
            title={t("about.placeholderTitle")}
            subtitle={t("about.placeholderBody")}
            actions={<Badge tone="amber"><Info className="h-3.5 w-3.5" />{t("about.placeholderLicense")}</Badge>}
          />
          <div className="flex flex-wrap items-center gap-x-4 gap-y-2 border-l-2 border-amber px-5 py-4">
            <span className="font-mono text-sm font-medium text-amber">{t("about.placeholderLicenseValue")}</span>
            <span className="text-sm text-muted">{t("about.placeholderLicenseText")}</span>
          </div>
        </Panel>

        <Panel data-enterprise-active={enterpriseActive} className={enterpriseActive ? "border-aqua/35 bg-aqua/5" : "border-iris/30"}>
          <PanelHeader
            eyebrow="Enterprise"
            title={t("about.enterpriseTitle")}
            subtitle={t("about.enterpriseSubtitle")}
            actions={<Badge tone={enterpriseActive ? "aqua" : "amber"}>{enterpriseActive ? t("about.enterpriseActive") : t("about.enterpriseInactive")}</Badge>}
          />
          <div className="space-y-4 px-5 py-5">
            <dl className="grid gap-4 sm:grid-cols-2">
              <div>
                <dt className="eyebrow mb-1">{t("enterprise.licenseState")}</dt>
                <dd className="text-sm text-fg">{enterpriseActive ? t("about.enterpriseActive") : t("about.enterpriseInactive")}</dd>
              </div>
              <div>
                <dt className="eyebrow mb-1">{t("about.enterpriseState")}</dt>
                <dd className="font-mono text-sm text-fg">{enterpriseState}</dd>
              </div>
            </dl>
            <div className="flex flex-wrap items-center gap-3">
              <Button
                variant="outline"
                loading={checkingUpdate}
                icon={<RefreshCw className="h-3.5 w-3.5" />}
                onClick={() => void checkForUpdates()}
              >
                {t("about.enterpriseCheckUpdates")}
              </Button>
              {updateCheckFailed && <p role="alert" className="text-xs text-rose">{t("about.enterpriseCheckFailed")}</p>}
            </div>
            {enterpriseStatus.data?.core_update && (
              <div className="space-y-3 rounded-xl border border-iris/35 bg-iris/8 px-4 py-3" role="status">
                <div>
                  <p className="text-sm font-medium text-fg">
                    {t("enterprise.updateAvailable", { version: enterpriseStatus.data.core_update.version })}
                  </p>
                  <p className="mt-1 font-mono text-xs text-muted">
                    {t("enterprise.updateRelease", { release_id: enterpriseStatus.data.core_update.release_id })}
                  </p>
                  <p className="mt-2 text-xs leading-relaxed text-muted">{t("about.enterpriseUpdateHint")}</p>
                </div>
                <Link
                  to="/enterprise"
                  className="inline-flex h-9 items-center rounded-lg bg-iris px-3.5 text-sm font-medium text-white hover:bg-iris/85"
                >
                  {t("about.enterpriseOpen")}
                </Link>
              </div>
            )}
            {!enterpriseActive && (
              <a href={manageUrl} target="_blank" rel="noreferrer" className="text-sm text-iris hover:text-iris/80">
                {t("enterprise.purchaseLink")}
              </a>
            )}
          </div>
        </Panel>

        <Panel>
          <PanelHeader
            title={t("about.dependenciesTitle")}
            subtitle={t("about.dependenciesSubtitle")}
            actions={
              <div className="w-full sm:w-72">
                <label className="sr-only" htmlFor="about-dependency-search">{t("about.searchLabel")}</label>
                <div className="relative">
                  <Search className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-faint" />
                  <Input
                    id="about-dependency-search"
                    type="search"
                    role="searchbox"
                    aria-label={t("about.searchLabel")}
                    placeholder={t("about.searchPlaceholder")}
                    value={query}
                    onChange={(event) => setQuery(event.target.value)}
                    className="pl-9"
                  />
                </div>
              </div>
            }
          />
          <div className="flex items-center gap-2 border-b border-line px-5 py-3 text-xs text-muted">
            <PackageOpen className="h-3.5 w-3.5 text-iris" />
            {t("about.dependenciesCount", { visible: filteredEntries.length, total: THIRD_PARTY_LICENSES.length })}
          </div>
        </Panel>

        {groups.length === 0 ? (
          <Panel>
            <div className="flex min-h-28 items-center justify-center px-5 py-8 text-sm text-muted">
              {t("about.empty")}
            </div>
          </Panel>
        ) : (
          groups.map(([license, entries]) => (
            <Panel key={license}>
              <PanelHeader
                title={<span className="flex items-center gap-2"><Scale className="h-4 w-4 text-iris" />{license}</span>}
                subtitle={t("about.licenseCount", { count: entries.length })}
              />
              <ul className="divide-y divide-line">
                {entries.map((entry) => (
                  <li key={`${entry.ecosystem}:${entry.name}:${entry.version}`} className="flex min-w-0 flex-wrap items-center gap-3 px-5 py-3.5">
                    <div className="min-w-0 flex-1">
                      <a
                        href={entry.repository}
                        target="_blank"
                        rel="noreferrer"
                        className="group inline-flex max-w-full items-center gap-1.5 text-sm font-medium text-fg hover:text-iris"
                        aria-label={`${entry.name} - ${t("about.repository")}`}
                      >
                        <span className="break-path">{entry.name}</span>
                        <ExternalLink className="h-3.5 w-3.5 shrink-0 text-faint transition-colors group-hover:text-iris" />
                      </a>
                      <div className="mt-1 flex flex-wrap items-center gap-2 text-xs text-faint">
                        <Badge tone={entry.ecosystem === "frontend" ? "sky" : "aqua"}>
                          {t(`about.ecosystem.${entry.ecosystem}`)}
                        </Badge>
                        <span>{t("about.version")} {entry.version}</span>
                      </div>
                    </div>
                  </li>
                ))}
              </ul>
            </Panel>
          ))
        )}
      </div>
    </>
  );
}
