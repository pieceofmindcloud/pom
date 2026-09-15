import { ExternalLink, Info, PackageOpen, Scale, Search } from "lucide-react";
import { useMemo, useState } from "react";
import { Badge, Input, PageHeader, Panel, PanelHeader } from "../components/ui";
import { useI18n } from "../i18n/provider";
import { POM_LICENSE, THIRD_PARTY_LICENSES, type LicenseEntry } from "../lib/licenses.generated";

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
  const [query, setQuery] = useState("");
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const filteredEntries = useMemo(
    () => THIRD_PARTY_LICENSES.filter((entry) => {
      if (!normalizedQuery) return true;
      return [entry.name, entry.license, entry.version, entry.ecosystem]
        .some((value) => value.toLocaleLowerCase().includes(normalizedQuery));
    }),
    [normalizedQuery],
  );
  const groups = useMemo(() => licenseGroups(filteredEntries), [filteredEntries]);

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
