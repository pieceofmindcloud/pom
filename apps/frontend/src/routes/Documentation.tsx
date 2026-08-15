/**
 * The Documentation screen: the repository's living documentation rendered as
 * navigable markdown.
 *
 * Content never leaves the browser because it never had a network path to take:
 * `scripts/build-docs.mjs` copies the folder AGENTS.md files, the subsystem
 * READMEs and the root README into `src/docs/` (gitignored) before every build,
 * and this module reads them through two build-time imports:
 *
 * - `manifest.json` (generated) - order, section and title per document;
 * - `import.meta.glob` over the copied `.md` files as raw strings.
 *
 * Embedding is the point: the panel is served from `dist/` on any machine, and
 * only what the bundle carries survives there. No node route, no disk reads at
 * runtime, no missing files on a machine that never had the repository.
 *
 * The tree on the left mirrors the repository layout (root, docs/, crates/,
 * apps/frontend/, scripts/); the crates section is grouped by crate so the
 * tree stays two levels deep. The search filters title and content across the
 * whole embedded set. The selection is a `?doc=` search param, so a document
 * is a shareable address.
 */
import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import { BookOpen, ChevronDown, FileText, Search, X } from "lucide-react";
import { useMemo, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { Markdown } from "../components/Markdown";
import { Empty, Input, PageHeader, Panel } from "../components/ui";
import { useI18n } from "../i18n/provider";
import manifest from "../docs/manifest.json";

const CONTENT = import.meta.glob("../docs/**/*.md", { as: "raw", eager: true });

type DocEntry = (typeof manifest)["entries"][number];

const SECTIONS: { id: string; labelKey: string }[] = [
  { id: "root", labelKey: "docs.section.root" },
  { id: "docs", labelKey: "docs.section.docs" },
  { id: "subsystems", labelKey: "docs.section.subsystems" },
  { id: "crates", labelKey: "docs.section.crates" },
  { id: "frontend", labelKey: "docs.section.frontend" },
  { id: "scripts", labelKey: "docs.section.scripts" },
];

/** First path segment after `crates/`: the crate the entry belongs to. */
function crateGroupOf(id: string): string {
  const rest = id.replace(/^crates\//, "");
  const slash = rest.indexOf("/");
  return slash === -1 ? rest : rest.slice(0, slash);
}

function Leaf({
  entry,
  active,
  onSelect,
}: {
  entry: DocEntry;
  active: boolean;
  onSelect: () => void;
}) {
  const { t } = useI18n();
  return (
    <button
      type="button"
      onClick={onSelect}
      aria-pressed={active}
      aria-label={t("docs.selectAria", { title: entry.title })}
      className={clsx(
        "flex min-w-0 flex-col items-start gap-0.5 rounded-lg px-2 py-1.5 text-left transition-colors duration-150",
        active
          ? "bg-iris/12 text-fg ring-1 ring-inset ring-iris/25"
          : "text-muted hover:bg-line/40 hover:text-fg",
      )}
    >
      <span className="w-full truncate text-[0.8rem] font-medium leading-snug">{entry.title}</span>
      <span className="w-full truncate font-mono text-[0.62rem] leading-tight text-faint">
        {entry.path}
      </span>
    </button>
  );
}

function Section({
  section,
  entries,
  selectedId,
  onSelect,
  defaultOpen,
}: {
  section: { id: string; labelKey: string };
  entries: DocEntry[];
  selectedId: string;
  onSelect: (id: string) => void;
  defaultOpen: boolean;
}) {
  const { t } = useI18n();
  const holdsSelection = entries.some((e) => e.id === selectedId);
  const [open, setOpen] = useState(defaultOpen);
  const expanded = open || holdsSelection;

  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={expanded}
        className="flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left text-[0.8rem] font-semibold text-fg transition-colors hover:bg-line/40"
      >
        <ChevronDown
          className={clsx(
            "h-3.5 w-3.5 shrink-0 text-faint transition-transform duration-200 ease-swift",
            expanded && "rotate-180",
          )}
        />
        <span className="flex-1 truncate">{t(section.labelKey)}</span>
        <span className="rounded-md border border-line bg-raised px-1.5 py-0.5 font-mono text-[0.62rem] text-faint">
          {entries.length}
        </span>
      </button>
      <AnimatePresence initial={false}>
        {expanded && (
          <motion.div
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: "auto", opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={{ duration: 0.18, ease: [0.22, 1, 0.36, 1] }}
            className="overflow-hidden"
          >
            <div className="ml-2.5 mt-0.5 flex flex-col gap-0.5 border-l border-line pb-1 pl-2">
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
            {crate}/
          </div>
          {group.map((e) => (
            <Leaf
              key={e.id}
              entry={e}
              active={e.id === selectedId}
              onSelect={() => onSelect(e.id)}
            />
          ))}
        </div>
      ))}
    </>
  );
}

export function Documentation() {
  const { t } = useI18n();
  const [params, setParams] = useSearchParams();
  const [query, setQuery] = useState("");
  const viewerRef = useRef<HTMLDivElement>(null);

  const entries = manifest.entries;
  const byId = useMemo(() => new Map(entries.map((e) => [e.id, e])), [entries]);

  const selectedId = byId.has(params.get("doc") ?? "") ? (params.get("doc") as string) : entries[0]?.id ?? "";
  const selected = selectedId ? byId.get(selectedId) : undefined;
  const selectedContent = selected ? CONTENT[`../docs/${selected.id}`] : undefined;

  const select = (id: string) => {
    setParams({ doc: id }, { replace: true });
    // The viewer scrolls with the page; a new document must not open halfway
    // down an old one. jsdom does not implement scrollIntoView, hence the guard.
    requestAnimationFrame(() => {
      try {
        viewerRef.current?.scrollIntoView({ block: "start" });
      } catch {
        // test environment
      }
    });
  };

  const q = query.trim().toLowerCase();
  const results = useMemo(() => {
    if (!q) return null;
    return entries.filter(
      (e) =>
        e.title.toLowerCase().includes(q) ||
        (CONTENT[`../docs/${e.id}`] ?? "").toLowerCase().includes(q),
    );
  }, [q, entries]);

  if (entries.length === 0) {
    return (
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
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <PageHeader title={t("docs.title")} subtitle={t("docs.subtitle")} />
      <div className="grid gap-4 lg:grid-cols-[minmax(0,290px)_minmax(0,1fr)] lg:items-start">
        <Panel className="overflow-hidden lg:sticky lg:top-0 lg:max-h-[calc(100vh-8.5rem)]">
          <div className="border-b border-line p-3">
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
          <div className="max-h-[calc(100vh-13rem)] min-h-0 overflow-y-auto p-2 lg:max-h-none lg:min-h-[20rem]">
            {results ? (
              results.length === 0 ? (
                <div className="px-2 py-4">
                  <p className="text-sm font-medium text-fg">{t("docs.noResults")}</p>
                  <p className="mt-1 text-xs leading-relaxed text-muted">
                    {t("docs.noResultsDetail")}
                  </p>
                </div>
              ) : (
                <div>
                  <p className="px-2 pb-1 text-[0.68rem] font-medium text-faint">
                    {t("docs.resultCount", { count: results.length })}
                  </p>
                  <div className="flex flex-col gap-0.5">
                    {results.map((e) => (
                      <Leaf
                        key={e.id}
                        entry={e}
                        active={e.id === selectedId}
                        onSelect={() => select(e.id)}
                      />
                    ))}
                  </div>
                </div>
              )
            ) : (
              <div className="flex flex-col gap-0.5">
                {SECTIONS.map((section) => {
                  const inSection = entries.filter((e) => e.section === section.id);
                  if (inSection.length === 0) return null;
                  return (
                    <Section
                      key={section.id}
                      section={section}
                      entries={inSection}
                      selectedId={selectedId}
                      onSelect={select}
                      defaultOpen
                    />
                  );
                })}
              </div>
            )}
          </div>
        </Panel>

        <Panel className="min-w-0 overflow-hidden">
          {selected && selectedContent !== undefined ? (
            <div ref={viewerRef}>
              <div className="border-b border-line px-5 py-4">
                <div className="eyebrow mb-1.5 flex items-center gap-1.5">
                  <FileText className="h-3 w-3" />
                  <span className="truncate font-mono">{selected.path}</span>
                </div>
                <h2 className="font-display text-[0.98rem] font-semibold tracking-tight text-fg">
                  {selected.title}
                </h2>
              </div>
              <div className="min-w-0 px-5 py-4">
                <Markdown>{selectedContent}</Markdown>
              </div>
            </div>
          ) : (
            <Empty icon={<FileText className="h-5 w-5" />} title={t("docs.noDoc")}>
              {t("docs.noDocDetail")}
            </Empty>
          )}
        </Panel>
      </div>
    </div>
  );
}
