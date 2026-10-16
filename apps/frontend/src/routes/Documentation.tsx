/**
 * The Documentation screen: two audiences, one address.
 *
 * **Guides** (`src/guides/`) are written for people - ordered pages with
 * diagrams that explain how the platform works. **Reference** is the
 * repository's living markdown, written for agents: exhaustive, file-level, and
 * true to the code. Mixing them was the old mistake; a person landing here got
 * `AGENTS.md - crates/routing-base/` as a first impression.
 *
 * Content never leaves the browser because it never had a network path to take:
 * `scripts/build-docs.mjs` copies the folder AGENTS.md files, the subsystem
 * READMEs and the root README into `src/docs/` (gitignored) before every build,
 * and this module reads them through two build-time imports:
 *
 * - `manifest.json` (generated) - order, section, human label and role;
 * - `import.meta.glob` over the copied `.md` files as raw strings.
 *
 * Two things the manifest carries so this screen never parses paths: `kind`
 * ("overview" for a folder's AGENTS.md, "doc" otherwise) and `label`, the short
 * human name. A folder's AGENTS.md is not a file called AGENTS.md - it is that
 * section's own page, which is why clicking a section header opens it instead
 * of listing it as a leaf.
 *
 * The screen is full-bleed (see `App.tsx`) and owns its scrolling: the
 * navigation column and the reading column scroll independently, so a long
 * document never drags the tree out of reach.
 *
 * Address: `?tab=`, `?guide=` and `?doc=` - every page is a shareable link.
 */
import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import { ArrowLeft, BookOpen, ChevronRight, FileText, Layers, Search, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { Markdown } from "../components/Markdown";
import { Empty, Input, Panel, Tabs } from "../components/ui";
import { GuideView } from "../guides/Guide";
import { guidesFor } from "../guides/content";
import { useI18n } from "../i18n/provider";
import manifest from "../docs/manifest.json";
import { api } from "../lib/api";
import { pluginAssetUrl, pluginUiSnapshot, type PluginDocumentationEntry } from "../lib/pluginUi";

const CONTENT = import.meta.glob<string>("../docs/**/*.md", {
  query: "?raw",
  import: "default",
  eager: true,
});

type DocEntry = (typeof manifest)["entries"][number];

type PluginDocEntry = {
  key: string;
  code: string;
  kind: "guide" | "reference";
  title: string;
  asset: string;
};

function localizedPluginTitle(entry: PluginDocumentationEntry, locale: string): string {
  return entry.title[locale] ?? entry.title[locale.split("-")[0]] ?? entry.title.en;
}

function pluginDocsFor(locale: string): PluginDocEntry[] {
  return pluginUiSnapshot().flatMap((plugin) => {
    const documentation = plugin.manifest.documentation;
    if (!documentation) return [];
    const entries = [
      ...documentation.guides.map((entry) => ({ entry, kind: "guide" as const })),
      ...documentation.reference.map((entry) => ({ entry, kind: "reference" as const })),
    ];
    return entries.map(({ entry, kind }) => ({
      key: "plugin:" + plugin.code + ":" + kind + ":" + entry.id,
      code: plugin.code,
      kind,
      title: localizedPluginTitle(entry, locale),
      asset: entry.asset,
    }));
  });
}

function PluginDocument({
  document,
  content,
  loading,
  error,
  loadingText,
  noDocTitle,
  noDocDetail,
}: {
  document: PluginDocEntry;
  content?: string;
  loading: boolean;
  error: boolean;
  loadingText: string;
  noDocTitle: string;
  noDocDetail: string;
}) {
  return (
    <article className="min-w-0">
      <header className="mb-5 border-b border-line pb-4">
        <h2 className="font-display text-[1.5rem] font-bold tracking-tight text-fg">
          {document.title}
        </h2>
        <p className="mt-1.5 flex items-center gap-1.5 font-mono text-[0.7rem] text-faint">
          <FileText className="h-3 w-3 shrink-0" />
          <span className="truncate">{document.code}/{document.asset}</span>
        </p>
      </header>
      {loading ? (
        <p className="text-sm text-muted">{loadingText}</p>
      ) : error || content === undefined ? (
        <Empty icon={<FileText className="h-5 w-5" />} title={noDocTitle}>
          {noDocDetail}
        </Empty>
      ) : (
        <Markdown>{withoutLeadingH1(content)}</Markdown>
      )}
    </article>
  );
}

const SECTIONS: { id: string; labelKey: string }[] = [
  { id: "root", labelKey: "docs.section.root" },
  { id: "docs", labelKey: "docs.section.docs" },
  { id: "subsystems", labelKey: "docs.section.subsystems" },
  { id: "crates", labelKey: "docs.section.crates" },
  { id: "frontend", labelKey: "docs.section.frontend" },
  { id: "scripts", labelKey: "docs.section.scripts" },
];

/** Localized name of the section an entry belongs to. */
function sectionLabelKey(section: string): string {
  return SECTIONS.find((s) => s.id === section)?.labelKey ?? "docs.section.root";
}

/** First path segment after `crates/`: the crate the entry belongs to. */
function crateGroupOf(id: string): string {
  const rest = id.replace(/^crates\//, "");
  const slash = rest.indexOf("/");
  return slash === -1 ? rest : rest.slice(0, slash);
}

/** The entry a section header opens: that section's own overview page. */
function overviewOf(entries: DocEntry[], section: string): DocEntry | undefined {
  const inSection = entries.filter((e) => e.section === section && e.kind === "overview");
  // The shallowest overview is the section's own: `crates/AGENTS.md` before
  // `crates/node/src/backend/AGENTS.md`.
  return inSection.sort((a, b) => a.id.split("/").length - b.id.split("/").length)[0];
}

/**
 * What the reader is called on screen. Never the file name, and never the
 * heading an agent doc gives itself ("AGENTS.md - crates/routing-base/"): a
 * folder overview is named after its folder, and the repository root is named
 * after the section it heads.
 */
function nameOf(entry: DocEntry, t: (key: string) => string): string {
  if (entry.kind === "overview") return entry.label || t("docs.section.root");
  return entry.label || entry.title;
}

/**
 * The reader prints the subject in its own header, so the document's first `# `
 * heading would say it a second time, in a different wording. Only a leading
 * H1 is dropped - a `#` further down is a real section of the text.
 */
function withoutLeadingH1(markdown: string): string {
  return markdown.replace(/^\s*#\s+[^\n]*\n/, "");
}

function Leaf({
  entry,
  active,
  onSelect,
  hint,
}: {
  entry: DocEntry;
  active: boolean;
  onSelect: () => void;
  hint?: string;
}) {
  const { t } = useI18n();
  const name = nameOf(entry, t);
  // The hint says *where* the page lives, never the file name: a search result
  // needs the section to be told apart, not `p2p-network.md`.
  return (
    <button
      type="button"
      onClick={onSelect}
      aria-pressed={active}
      aria-label={t("docs.selectAria", { title: name })}
      className={clsx(
        "flex min-w-0 flex-col items-start rounded-md px-2 py-1.5 text-left transition-colors duration-150",
        active
          ? "bg-iris/12 text-fg ring-1 ring-inset ring-iris/25"
          : "text-muted hover:bg-line/40 hover:text-fg",
      )}
    >
      <span className="w-full truncate text-[0.8rem] leading-snug">{name}</span>
      {hint && (
        <span className="w-full truncate text-[0.62rem] leading-tight text-faint">{hint}</span>
      )}
    </button>
  );
}

/**
 * A collapsible section.
 *
 * The header does two things on purpose, and they are the same intent: it opens
 * the section and shows the section's own overview page. Collapsing is the
 * chevron, which is a separate control so that closing a section never changes
 * what is being read.
 */
function Section({
  section,
  entries,
  overview,
  selectedId,
  onSelect,
  open,
  onToggle,
}: {
  section: { id: string; labelKey: string };
  entries: DocEntry[];
  overview?: DocEntry;
  selectedId: string;
  onSelect: (id: string) => void;
  open: boolean;
  onToggle: () => void;
}) {
  const { t } = useI18n();
  const label = t(section.labelKey);
  const headerActive = overview ? overview.id === selectedId : false;
  // A section whose only page is its own overview (docs/, scripts/) has
  // nothing to expand; the chevron would be a control that does nothing.
  const collapsible = entries.length > 0;

  return (
    <div>
      <div
        className={clsx(
          "flex items-center rounded-md pr-1 transition-colors",
          headerActive ? "bg-iris/12 ring-1 ring-inset ring-iris/25" : "hover:bg-line/40",
        )}
      >
        {collapsible ? (
          <button
            type="button"
            onClick={onToggle}
            aria-expanded={open}
            aria-label={t(open ? "docs.collapse" : "docs.expand", { section: label })}
            className="flex h-7 w-7 shrink-0 items-center justify-center rounded-md text-faint hover:text-fg"
          >
            <ChevronRight
              className={clsx(
                "h-3.5 w-3.5 transition-transform duration-200 ease-swift",
                open && "rotate-90",
              )}
            />
          </button>
        ) : (
          <span className="h-7 w-7 shrink-0" aria-hidden="true" />
        )}
        <button
          type="button"
          onClick={() => {
            if (collapsible && !open) onToggle();
            if (overview) onSelect(overview.id);
          }}
          className="min-w-0 flex-1 py-1.5 text-left text-[0.8rem] font-semibold tracking-tight text-fg"
        >
          <span className="block truncate">{label}</span>
        </button>
        {collapsible && (
          <span className="shrink-0 font-mono text-[0.62rem] text-faint">{entries.length}</span>
        )}
      </div>
      <AnimatePresence initial={false}>
        {open && entries.length > 0 && (
          <motion.div
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: "auto", opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={{ duration: 0.18, ease: [0.22, 1, 0.36, 1] }}
            className="overflow-hidden"
          >
            <div className="ml-3.5 mt-0.5 flex flex-col gap-0.5 border-l border-line pb-1 pl-2">
              {section.id === "crates" ? (
                <CrateGroups entries={entries} selectedId={selectedId} onSelect={onSelect} />
              ) : (
                entries.map((e) => (
                  <Leaf
                    key={e.id}
                    entry={e}
                    active={e.id === selectedId}
                    onSelect={() => onSelect(e.id)}
                  />
                ))
              )}
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}

function CrateGroups({
  entries,
  selectedId,
  onSelect,
}: {
  entries: DocEntry[];
  selectedId: string;
  onSelect: (id: string) => void;
}) {
  const groups = useMemo(() => {
    const map = new Map<string, DocEntry[]>();
    for (const e of entries) {
      const g = crateGroupOf(e.id);
      map.set(g, [...(map.get(g) ?? []), e]);
    }
    return [...map.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  }, [entries]);

  return (
    <>
      {groups.map(([crate, group]) => (
        <div key={crate}>
          <div className="px-2 pb-0.5 pt-2 font-mono text-[0.62rem] font-semibold uppercase tracking-wider text-faint">
            {crate}
          </div>
          {group.map((e) => (
            <Leaf key={e.id} entry={e} active={e.id === selectedId} onSelect={() => onSelect(e.id)} />
          ))}
        </div>
      ))}
    </>
  );
}

/** The two audiences, switched at the top of the navigation column. */
function TabSwitch({ tab, onChange }: { tab: Tab; onChange: (t: Tab) => void }) {
  const { t } = useI18n();
  return (
    <Tabs
      fill
      value={tab}
      onChange={onChange}
      items={[
        { value: "guides", label: t("docs.tab.guides"), icon: <BookOpen /> },
        { value: "reference", label: t("docs.tab.reference"), icon: <Layers /> },
      ]}
    />
  );
}

type Tab = "guides" | "reference";

export function Documentation() {
  const { t, locale } = useI18n();
  const [params, setParams] = useSearchParams();
  const [query, setQuery] = useState("");
  const readerRef = useRef<HTMLDivElement>(null);
  // On a phone the sidebar takes the whole screen and the reader stays hidden
  // until an explicit selection; a deep link (`?doc=`/`?guide=` already in the
  // URL on mount) counts as that selection and opens straight on the content.
  // `Voltar` at the top of the reader only closes that view - the
  // `?doc=`/`?guide=` still holds, so reloading the page does not lose the place.
  const [mobileReaderOpen, setMobileReaderOpen] = useState(
    () => Boolean(params.get("doc") || params.get("guide")),
  );

  const entries = manifest.entries;
  const byId = useMemo(() => new Map(entries.map((e) => [e.id, e])), [entries]);
  const guides = useMemo(() => guidesFor(locale), [locale]);
  const pluginDocs = useMemo(() => pluginDocsFor(locale), [locale]);
  const pluginGuides = pluginDocs.filter((document) => document.kind === "guide");
  const pluginReference = pluginDocs.filter((document) => document.kind === "reference");

  const tab: Tab = params.get("tab") === "reference" ? "reference" : "guides";
  const selectedPluginReference = pluginReference.find((document) => document.key === params.get("doc"));
  const selectedId = byId.has(params.get("doc") ?? "")
    ? (params.get("doc") as string)
    : entries[0]?.id ?? "";
  const selected = selectedId ? byId.get(selectedId) : undefined;
  const selectedContent = selected ? CONTENT[`../docs/${selected.id}`] : undefined;
  const selectedPluginGuide = pluginGuides.find((document) => document.key === params.get("guide"));
  const guideId = guides.some((g) => g.id === params.get("guide"))
    ? (params.get("guide") as string)
    : guides[0]?.id ?? "";
  const guide = guides.find((g) => g.id === guideId);
  const activePluginDocument = tab === "guides" ? selectedPluginGuide : selectedPluginReference;
  const [pluginContent, setPluginContent] = useState<Record<string, string>>({});
  const [pluginContentError, setPluginContentError] = useState<Record<string, boolean>>({});
  const [pluginContentLoading, setPluginContentLoading] = useState<string | null>(null);

  useEffect(() => {
    if (!activePluginDocument || pluginContent[activePluginDocument.key] !== undefined) return;
    let cancelled = false;
    setPluginContentLoading(activePluginDocument.key);
    void api
      .getText(pluginAssetUrl(activePluginDocument.code, activePluginDocument.asset), { auth: false })
      .then((content) => {
        if (cancelled) return;
        setPluginContent((current) => ({ ...current, [activePluginDocument.key]: content }));
      })
      .catch(() => {
        if (cancelled) return;
        setPluginContentError((current) => ({ ...current, [activePluginDocument.key]: true }));
      })
      .finally(() => {
        if (!cancelled) setPluginContentLoading(null);
      });
    return () => {
      cancelled = true;
    };
  }, [activePluginDocument]);

  // A new page must not open halfway down the previous one. jsdom has no
  // layout, hence the guard.
  const toTop = useCallback(() => {
    requestAnimationFrame(() => {
      try {
        readerRef.current?.scrollTo({ top: 0 });
      } catch {
        // test environment
      }
    });
  }, []);

  const openDoc = useCallback(
    (id: string) => {
      setParams({ tab: "reference", doc: id }, { replace: true });
      setMobileReaderOpen(true);
      toTop();
    },
    [setParams, toTop],
  );

  const openGuide = useCallback(
    (id: string) => {
      setParams({ tab: "guides", guide: id }, { replace: true });
      setMobileReaderOpen(true);
      toTop();
    },
    [setParams, toTop],
  );

  const switchTab = (next: Tab) => {
    if (next === tab) return;
    setParams(next === "guides" ? { tab: "guides", guide: guideId } : { tab: "reference", doc: selectedId }, {
      replace: true,
    });
    toTop();
  };

  // Sections start open, but the operator's collapse wins: an explicit `false`
  // is remembered, and the selection no longer forces a section back open (that
  // was the bug - a section holding the open document could not be closed).
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const toggle = (id: string) => setCollapsed((c) => ({ ...c, [id]: !c[id] }));

  const q = query.trim().toLowerCase();
  const results = useMemo(() => {
    if (!q) return null;
    return entries.filter(
      (e) =>
        (e.label || e.title).toLowerCase().includes(q) ||
        e.path.toLowerCase().includes(q) ||
        (CONTENT[`../docs/${e.id}`] ?? "").toLowerCase().includes(q),
    );
  }, [q, entries]);
  const pluginResults = useMemo(
    () => (q ? pluginDocs.filter((document) => document.title.toLowerCase().includes(q) || document.asset.toLowerCase().includes(q)) : []),
    [q, pluginDocs],
  );

  if (entries.length === 0) {
    return (
      <div className="p-6">
        <Panel>
          <Empty
            icon={<BookOpen className="h-5 w-5" />}
            title={t("docs.empty")}
            action={
              <button
                type="button"
                className="rounded-lg border border-line bg-raised px-3 py-2 text-sm font-medium text-fg hover:border-line-strong hover:bg-line/40"
                onClick={() => window.location.reload()}
              >
                {t("common.retry")}
              </button>
            }
          >
            {t("docs.emptyDetail")}
          </Empty>
        </Panel>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 w-full">
      {/*
        On a phone this bar takes the whole screen and the reader stays hidden
        until a selection; from `sm:` up the two columns live side by side again,
        regardless of `mobileReaderOpen` - that state only exists for the
        phone.
      */}
      <nav
        className={clsx(
          "w-full shrink-0 flex-col border-r border-line bg-surface sm:flex sm:w-[16.5rem]",
          mobileReaderOpen ? "hidden" : "flex",
        )}
      >
        <div className="shrink-0 border-b border-line p-3">
          <TabSwitch tab={tab} onChange={switchTab} />
        </div>

        {tab === "reference" && (
          <div className="shrink-0 border-b border-line p-3">
            <label className="relative block">
              <Search className="pointer-events-none absolute left-2.5 top-1/2 h-4 w-4 -translate-y-1/2 text-faint" />
              <Input
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                placeholder={t("docs.search")}
                aria-label={t("docs.searchAria")}
                className="pl-8 pr-8"
              />
              {query && (
                <button
                  type="button"
                  onClick={() => setQuery("")}
                  aria-label={t("docs.clearSearch")}
                  className="absolute right-2 top-1/2 -translate-y-1/2 rounded p-0.5 text-faint hover:text-fg"
                >
                  <X className="h-3.5 w-3.5" />
                </button>
              )}
            </label>
          </div>
        )}

        {/*
          `min-h-0` is what makes this scroll: without it the flex child grows
          to its content and the overflow never engages - which is exactly how
          the search results used to run off the bottom of the screen with no
          scrollbar.
        */}
        <div className="min-h-0 flex-1 overflow-y-auto p-2">
          {tab === "guides" ? (
            <div className="flex flex-col gap-0.5">
              {guides.map((g) => {
                const active = g.id === guideId;
                return (
                  <button
                    key={g.id}
                    type="button"
                    onClick={() => openGuide(g.id)}
                    aria-pressed={active}
                    className={clsx(
                      "flex min-w-0 flex-col items-start rounded-md px-2.5 py-2 text-left transition-colors duration-150",
                      active
                        ? "bg-iris/12 text-fg ring-1 ring-inset ring-iris/25"
                        : "text-muted hover:bg-line/40 hover:text-fg",
                    )}
                  >
                    <span className="w-full truncate text-[0.82rem] font-medium leading-snug">
                      {g.label}
                    </span>
                    <span className="w-full text-[0.68rem] leading-snug text-faint">{g.summary}</span>
                  </button>
                );
              })}
              {pluginGuides.length > 0 && (
                <div className="mt-3 border-t border-line pt-3">
                  <p className="px-2.5 pb-1 text-[0.68rem] font-medium uppercase tracking-wide text-faint">
                    {t("docs.pluginGuides")}
                  </p>
                  {pluginGuides.map((document) => {
                    const active = document.key === params.get("guide");
                    return (
                      <button
                        key={document.key}
                        type="button"
                        onClick={() => {
                          setParams({ tab: "guides", guide: document.key }, { replace: true });
                          setMobileReaderOpen(true);
                          toTop();
                        }}
                        aria-pressed={active}
                        className={clsx(
                          "flex min-w-0 flex-col items-start rounded-md px-2.5 py-2 text-left transition-colors duration-150",
                          active
                            ? "bg-iris/12 text-fg ring-1 ring-inset ring-iris/25"
                            : "text-muted hover:bg-line/40 hover:text-fg",
                        )}
                      >
                        <span className="w-full truncate text-[0.82rem] font-medium leading-snug">
                          {document.title}
                        </span>
                        <span className="w-full truncate text-[0.68rem] leading-snug text-faint">
                          {document.code}
                        </span>
                      </button>
                    );
                  })}
                </div>
              )}
            </div>
          ) : results ? (
            results.length === 0 && pluginResults.length === 0 ? (
              <div className="px-2 py-4">
                <p className="text-sm font-medium text-fg">{t("docs.noResults")}</p>
                <p className="mt-1 text-xs leading-relaxed text-muted">{t("docs.noResultsDetail")}</p>
              </div>
            ) : (
              <div>
                <p className="px-2 pb-1 text-[0.68rem] font-medium text-faint">
                  {t("docs.resultCount", { count: results.length + pluginResults.length })}
                </p>
                <div className="flex flex-col gap-0.5">
                  {results.map((e) => (
                    <Leaf
                      key={e.id}
                      entry={e}
                      active={e.id === selectedId}
                      hint={t(sectionLabelKey(e.section))}
                      onSelect={() => openDoc(e.id)}
                    />
                  ))}
                  {pluginResults.map((document) => (
                    <button
                      key={document.key}
                      type="button"
                      onClick={() => {
                        const parameter: Record<string, string> = document.kind === "guide"
                          ? { tab: "guides", guide: document.key }
                          : { tab: "reference", doc: document.key };
                        setParams(parameter, { replace: true });
                        setMobileReaderOpen(true);
                        toTop();
                      }}
                      aria-pressed={document.key === params.get(document.kind === "guide" ? "guide" : "doc")}
                      className="flex min-w-0 flex-col items-start rounded-md px-2 py-1.5 text-left text-muted transition-colors hover:bg-line/40 hover:text-fg"
                    >
                      <span className="w-full truncate text-[0.8rem] leading-snug">{document.title}</span>
                      <span className="w-full truncate text-[0.62rem] leading-tight text-faint">
                        {document.code}
                      </span>
                    </button>
                  ))}
                </div>
              </div>
            )
          ) : (
            <div className="flex flex-col gap-0.5">
              {pluginReference.length > 0 && (
                <div className="mb-2 border-b border-line pb-2">
                  <p className="px-2 pb-1 text-[0.68rem] font-medium uppercase tracking-wide text-faint">
                    {t("docs.plugins")}
                  </p>
                  {pluginReference.map((document) => (
                    <button
                      key={document.key}
                      type="button"
                      onClick={() => {
                        setParams({ tab: "reference", doc: document.key }, { replace: true });
                        setMobileReaderOpen(true);
                        toTop();
                      }}
                      aria-pressed={document.key === params.get("doc")}
                      className={clsx(
                        "flex min-w-0 flex-col items-start rounded-md px-2 py-1.5 text-left transition-colors",
                        document.key === params.get("doc")
                          ? "bg-iris/12 text-fg ring-1 ring-inset ring-iris/25"
                          : "text-muted hover:bg-line/40 hover:text-fg",
                      )}
                    >
                      <span className="w-full truncate text-[0.8rem] leading-snug">{document.title}</span>
                      <span className="w-full truncate text-[0.62rem] leading-tight text-faint">{document.code}</span>
                    </button>
                  ))}
                </div>
              )}
              {SECTIONS.map((section) => {
                const overview = overviewOf(entries, section.id);
                const inSection = entries.filter(
                  (e) => e.section === section.id && e.id !== overview?.id,
                );
                if (inSection.length === 0 && !overview) return null;
                return (
                  <Section
                    key={section.id}
                    section={section}
                    entries={inSection}
                    overview={overview}
                    selectedId={selectedId}
                    onSelect={openDoc}
                    open={collapsed[section.id] !== true}
                    onToggle={() => toggle(section.id)}
                  />
                );
              })}
            </div>
          )}
        </div>
      </nav>

      <div
        ref={readerRef}
        data-testid="docs-reader"
        className={clsx(
          "min-w-0 flex-1 overflow-y-auto sm:block",
          mobileReaderOpen ? "block" : "hidden",
        )}
      >
        {/* It only makes sense below `sm:` - from there up the two columns live
            side by side and going back is just picking another item in the bar, already
            visible. Mounted only when the mobile view is open, so the state
            stays inspectable without depending on a real breakpoint. */}
        {mobileReaderOpen && (
          <div className="sticky top-0 z-10 flex items-center border-b border-line bg-surface/95 px-2 py-1.5 backdrop-blur-sm sm:hidden">
            <button
              type="button"
              onClick={() => setMobileReaderOpen(false)}
              className="inline-flex items-center gap-1.5 rounded-md px-2 py-1.5 text-[0.82rem] font-medium text-muted transition-colors hover:text-fg"
            >
              <ArrowLeft className="h-4 w-4" aria-hidden="true" />
              {t("docs.back")}
            </button>
          </div>
        )}
        <div className="mx-auto w-full max-w-[62rem] px-6 py-7 sm:px-10">
          {tab === "guides" ? (
            selectedPluginGuide ? (
              <PluginDocument
                document={selectedPluginGuide}
                content={pluginContent[selectedPluginGuide.key]}
                loading={pluginContentLoading === selectedPluginGuide.key}
                error={pluginContentError[selectedPluginGuide.key] === true}
                loadingText={t("docs.pluginLoading")}
                noDocTitle={t("docs.noDoc")}
                noDocDetail={t("docs.noDocDetail")}
              />
            ) : guide ? (
              <GuideView guide={guide} onOpenDoc={openDoc} />
            ) : (
              <Empty icon={<BookOpen className="h-5 w-5" />} title={t("docs.noDoc")}>
                {t("docs.noDocDetail")}
              </Empty>
            )
          ) : selectedPluginReference ? (
            <PluginDocument
              document={selectedPluginReference}
              content={pluginContent[selectedPluginReference.key]}
              loading={pluginContentLoading === selectedPluginReference.key}
              error={pluginContentError[selectedPluginReference.key] === true}
              loadingText={t("docs.pluginLoading")}
              noDocTitle={t("docs.noDoc")}
              noDocDetail={t("docs.noDocDetail")}
            />
          ) : selected && selectedContent !== undefined ? (
            <article className="min-w-0">
              <header className="mb-5 border-b border-line pb-4">
                <h2 className="font-display text-[1.5rem] font-bold tracking-tight text-fg">
                  {nameOf(selected, t)}
                </h2>
                <p className="mt-1.5 flex items-center gap-1.5 font-mono text-[0.7rem] text-faint">
                  <FileText className="h-3 w-3 shrink-0" />
                  <span className="truncate">{selected.path}</span>
                </p>
              </header>
              <Markdown>{withoutLeadingH1(selectedContent)}</Markdown>
            </article>
          ) : (
            <Empty icon={<FileText className="h-5 w-5" />} title={t("docs.noDoc")}>
              {t("docs.noDocDetail")}
            </Empty>
          )}
        </div>
      </div>
    </div>
  );
}
