import { Download, ExternalLink, Info, PackageOpen, RefreshCw, Scale, Search } from "lucide-react";
import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { Badge, Button, Input, PageHeader, Panel, PanelHeader } from "../components/ui";
import { useI18n } from "../i18n/provider";
import { api } from "../lib/api";
import { THIRD_PARTY_LICENSES, type LicenseEntry } from "../lib/licenses.generated";
import { useEnterpriseStatus, usePomCoreUpdateStatus } from "../lib/queries";

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
  const pomUpdate = usePomCoreUpdateStatus();
  const [query, setQuery] = useState("");
  const [checkingUpdate, setCheckingUpdate] = useState(false);
  const [updateCheckFailed, setUpdateCheckFailed] = useState(false);
  const [applyingUpdate, setApplyingUpdate] = useState(false);
  const [applyUpdateFailed, setApplyUpdateFailed] = useState(false);
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const enterpriseActive = Boolean(enterpriseStatus.data?.active);
  const enterpriseState = enterpriseStatus.data?.state ?? "unavailable";
  const manageUrl = enterpriseStatus.data?.manage_url ?? "https://pieceofmind.cloud/enterprise";
  const coreUpdate = pomUpdate.data?.core_update ?? null;
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
      await api.post("/api/ui/pom-update/check");
      await pomUpdate.refetch();
    } catch {
      setUpdateCheckFailed(true);
    } finally {
      setCheckingUpdate(false);
    }
  }

  async function applyUpdate() {
    if (!coreUpdate) return;
    setApplyingUpdate(true);
    setApplyUpdateFailed(false);
    try {
      await api.post("/api/ui/pom-update/apply", { release_id: coreUpdate.release_id });
    } catch {
      setApplyUpdateFailed(true);
    } finally {
      setApplyingUpdate(false);
    }
  }

  return (
    <>
      <PageHeader title={t("about.title")} subtitle={t("about.subtitle")} />

      <div className="space-y-5">
        <Panel className="overflow-hidden border-line">
          <PanelHeader
            eyebrow="POM"
            title={t("about.placeholderTitle")}
            subtitle={t("about.placeholderBody")}
            actions={<Badge tone="amber"><Info className="h-3.5 w-3.5" />AGPL-3.0</Badge>}
          />
          <div className="flex flex-wrap items-center gap-x-4 gap-y-2 border-l-2 border-aqua px-5 py-4">
            <a href="https://www.gnu.org/licenses/agpl-3.0.html" target="_blank" rel="noreferrer" className="inline-flex items-center gap-1.5 font-mono text-sm font-medium text-aqua hover:text-aqua/80">
              {t("about.placeholderLicenseValue")} <ExternalLink className="h-3.5 w-3.5" />
            </a>
            <a href="https://github.com/pereirajair/rdma-p2p/blob/feat/enterprise/LICENSE" target="_blank" rel="noreferrer" className="inline-flex items-center gap-1.5 text-sm text-iris hover:text-iris/80">
              {t("about.repositoryLicenseLink")} <ExternalLink className="h-3.5 w-3.5" />
            </a>
            <span className="text-sm text-muted">{t("about.placeholderLicenseText")}</span>
          </div>
        </Panel>

        <Panel>
          <PanelHeader eyebrow="LGPD" title={t("about.safetyTitle")} subtitle={t("about.safetyBody")} />
          <div className="space-y-3 px-5 py-4 text-sm leading-relaxed text-muted">
            <p>{t("about.safetyRisks")}</p>
            <p className="font-medium text-fg">{t("about.safetyWarning")}</p>
            <div className="flex flex-wrap gap-x-5 gap-y-2 border-t border-line pt-3">
              <a href="https://pieceofmind.cloud" target="_blank" rel="noreferrer" className="text-iris hover:text-iris/80">{t("about.websiteLink")}</a>
              <a href="https://pieceofmind.cloud/contact.html" target="_blank" rel="noreferrer" className="text-iris hover:text-iris/80">{t("about.contactLink")}</a>
              <a href="https://github.com/pereirajair/rdma-p2p" target="_blank" rel="noreferrer" className="text-iris hover:text-iris/80">{t("about.repositoryLink")}</a>
              <a href="https://github.com/pereirajair/pom-plugin-base" target="_blank" rel="noreferrer" className="text-iris hover:text-iris/80">{t("about.pluginBaseLink")}</a>
            </div>
            <p>{t("about.pluginBaseDescription")}</p>
          </div>
        </Panel>

        <Panel data-core-update-available={Boolean(coreUpdate)} className={coreUpdate ? "border-iris/35 bg-iris/5" : "border-line"}>
          <PanelHeader
            eyebrow="POM"
            title={t("about.pomUpdateTitle")}
            subtitle={t("about.pomUpdateSubtitle")}
          />
          <div className="space-y-4 px-5 py-5">
            <dl className="grid gap-4 sm:grid-cols-2">
              <div>
                <dt className="eyebrow mb-1">{t("about.enterpriseState")}</dt>
                <dd className="text-sm text-fg">
                  <Badge tone={enterpriseActive ? "aqua" : "amber"}>{enterpriseActive ? t("about.enterpriseActive") : t("about.enterpriseInactive")}</Badge>
                </dd>
              </div>
              <div>
                <dt className="eyebrow mb-1">{t("enterprise.licenseState")}</dt>
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
            {coreUpdate && (
              <div className="space-y-3 rounded-xl border border-iris/35 bg-iris/8 px-4 py-3" role="status">
                <div>
                  <p className="text-sm font-medium text-fg">
                    {t("about.pomUpdateAvailable", { version: coreUpdate.version })}
                  </p>
                  <p className="mt-1 font-mono text-xs text-muted">
                    {t("about.pomUpdateRelease", { release_id: coreUpdate.release_id })}
                  </p>
                  {coreUpdate.changelog && <p className="mt-2 text-xs leading-relaxed text-muted">{coreUpdate.changelog}</p>}
                </div>
                <Button
                  variant="primary"
                  loading={applyingUpdate}
                  icon={<Download className="h-3.5 w-3.5" />}
                  onClick={() => void applyUpdate()}
                >
                  {t("about.pomUpdateApply")}
                </Button>
                {applyUpdateFailed && <p role="alert" className="text-xs text-rose">{t("about.pomUpdateApplyFailed")}</p>}
              </div>
            )}
            {!enterpriseActive && (
              <p className="text-xs leading-relaxed text-muted">
                {t("about.pomUpdateNeedsPlugin")}{" "}
                <Link to="/plugins" className="text-iris hover:text-iris/80">{t("about.pomUpdateOpenPlugins")}</Link>
                {" "}
                <a href={manageUrl} target="_blank" rel="noreferrer" className="text-iris hover:text-iris/80">
                  {t("enterprise.purchaseLink")}
                </a>
              </p>
            )}
          </div>
        </Panel>

        <Panel>
          <PanelHeader title={t("about.dataTitle")} subtitle={t("about.dataSubtitle")} />
          <ul className="space-y-2 px-5 py-4 text-sm text-muted" data-testid="about-data">
            <li>
              <a className="text-iris hover:underline" href="https://github.com/dr5hn/countries-states-cities-database" target="_blank" rel="noreferrer">Countries States Cities Database</a>
              {" - "}{t("about.dataCities")}
            </li>
            <li>
              <a className="text-iris hover:underline" href="https://www.openstreetmap.org/copyright" target="_blank" rel="noreferrer">OpenStreetMap</a>
              {" - "}{t("about.dataMap")}
            </li>
          </ul>
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
