import { ArrowUpRight, ChevronLeft, ChevronRight, Download, LockKeyhole, Search, SlidersHorizontal } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useSearchParams } from "react-router-dom";
import { DownloadModelDialog } from "../components/DownloadModelDialog";
import { ModelDownloadsPanel } from "../components/ModelDownloadsPanel";
import { RemoteModelDetails } from "../components/RemoteModelDetails";
import { RemoteModelProviderStatus } from "../components/RemoteModelProviderStatus";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Field,
  FullBleedHeader,
  FullBleedToolbar,
  Input,
  ScrollX,
  Select,
  Skeleton,
} from "../components/ui";
import { useI18n } from "../i18n/provider";
import { num, since } from "../lib/format";
import { keys, useRemoteModelProviderStatus, useRemoteModelSearch } from "../lib/queries";
import type { RemoteGgufFile, RemoteModelDetails as RemoteModelDetailsData, RemoteModelSummary } from "../lib/types";

const PAGE_SIZE = 20;
type Sort = "relevance" | "downloads" | "recent";

function sortedModels(models: RemoteModelSummary[], sort: Sort) {
  return [...models].sort((a, b) => {
    if (sort === "downloads") return (b.downloads ?? -1) - (a.downloads ?? -1);
    if (sort === "recent") return (Date.parse(b.last_modified ?? "") || 0) - (Date.parse(a.last_modified ?? "") || 0);
    return 0;
  });
}

function ResultRow({ model, onOpen }: { model: RemoteModelSummary; onOpen: (model: RemoteModelSummary) => void }) {
  const { t } = useI18n();
  return (
    <tr className="border-t border-line align-top transition-colors duration-150 hover:bg-raised">
      <td className="min-w-[16rem] px-5 py-3.5">
        <button
          type="button"
          className="min-w-0 text-left focus:outline-none focus-visible:ring-2 focus-visible:ring-iris/50"
          onClick={() => onOpen(model)}
        >
          <span className="break-path block font-medium text-fg hover:text-iris">{model.repo_id}</span>
          {model.author && <span className="mt-0.5 block text-xs text-muted">{model.author}</span>}
        </button>
        <p className="mt-1.5 line-clamp-2 max-w-[32rem] text-xs leading-relaxed text-muted">
          {model.description || t("remoteModels.searchHint")}
        </p>
      </td>
      <td className="min-w-[13rem] px-5 py-3.5">
        <div className="flex flex-wrap items-center gap-1.5">
          {model.has_gguf && (
            <Badge tone="aqua" mono>
              <Download className="h-3 w-3" />
              {t("remoteModels.gguf")}
            </Badge>
          )}
          {model.gated && (
            <Badge tone="amber">
              <LockKeyhole className="h-3 w-3" />
              {t("remoteModels.gated")}
            </Badge>
          )}
          {model.private && <Badge tone="amber">{t("remoteModels.private")}</Badge>}
          {model.installed && <Badge tone="aqua">{t("remoteModels.downloaded")}</Badge>}
          {model.downloading && <Badge tone="amber">{t("remoteModels.downloading")}</Badge>}
          {model.tags.slice(0, 3).map((tag) => (
            <Badge key={tag} tone="neutral">
              {tag}
            </Badge>
          ))}
        </div>
      </td>
      <td className="whitespace-nowrap px-5 py-3.5 font-mono text-[0.8rem] tabular-nums text-fg">
        {num(model.downloads)}
      </td>
      <td className="whitespace-nowrap px-5 py-3.5 font-mono text-[0.8rem] tabular-nums text-fg">
        {num(model.likes)}
      </td>
      <td className="whitespace-nowrap px-5 py-3.5 text-[0.8rem] text-fg" title={model.last_modified ?? undefined}>
        {since(model.last_modified)}
      </td>
      <td className="whitespace-nowrap px-5 py-3.5">
        <Button
          variant="ghost"
          size="sm"
          icon={<ArrowUpRight className="h-3.5 w-3.5" />}
          onClick={() => onOpen(model)}
          aria-label={t("remoteModels.openDetails", { name: model.repo_id })}
        >
          {t("remoteModels.openDetails", { name: "" }).replace(/ $/, "")}
        </Button>
      </td>
    </tr>
  );
}

export function RemoteModels() {
  const { t } = useI18n();
  const queryClient = useQueryClient();
  const [params, setParams] = useSearchParams();
  const urlQuery = params.get("q") ?? "";
  const urlGguf = params.get("format") !== "all";
  const urlPage = Math.max(1, Number(params.get("page") || "1") || 1);
  const sortParam = params.get("sort");
  const sort: Sort = sortParam === "downloads" || sortParam === "recent" ? sortParam : "relevance";
  const [input, setInput] = useState(urlQuery);
  const [query, setQuery] = useState(urlQuery);
  const [ggufOnly, setGgufOnly] = useState(urlGguf);
  const [page, setPage] = useState(urlPage);
  const [detailsRepo, setDetailsRepo] = useState<string | null>(null);
  const [download, setDownload] = useState<{ file: RemoteGgufFile; details: RemoteModelDetailsData } | null>(null);
  const providerStatus = useRemoteModelProviderStatus();
  const search = useRemoteModelSearch({
    query,
    ggufOnly,
    page,
    pageSize: PAGE_SIZE,
    sort,
    enabled: providerStatus.data?.cli.installed === true,
  });

  useEffect(() => setInput(urlQuery), [urlQuery]);

  useEffect(() => {
    setQuery(urlQuery);
    setGgufOnly(urlGguf);
    setPage(urlPage);
  }, [urlGguf, urlPage, urlQuery]);

  useEffect(() => {
    if (input.trim() === query) return;
    const timer = window.setTimeout(() => submit(input), 400);
    return () => window.clearTimeout(timer);
  }, [input, query, ggufOnly, params, sort]);

  function submit(value: string) {
    const nextQuery = value.trim();
    setQuery(nextQuery);
    setPage(1);
    const next = new URLSearchParams(params);
    if (nextQuery) next.set("q", nextQuery);
    else next.delete("q");
    next.set("page", "1");
    next.set("format", ggufOnly ? "gguf" : "all");
    if (sort !== "relevance") next.set("sort", sort);
    else next.delete("sort");
    setParams(next, { replace: true });
  }

  function toggleGguf(nextValue: boolean) {
    setGgufOnly(nextValue);
    setPage(1);
    const next = new URLSearchParams(params);
    next.set("format", nextValue ? "gguf" : "all");
    next.set("page", "1");
    setParams(next, { replace: true });
  }

  function changeSort(nextSort: Sort) {
    const next = new URLSearchParams(params);
    if (nextSort === "relevance") next.delete("sort");
    else next.set("sort", nextSort);
    setParams(next, { replace: true });
  }

  function changePage(nextPage: number) {
    const next = new URLSearchParams(params);
    next.set("page", String(nextPage));
    setPage(nextPage);
    setParams(next, { replace: true });
  }

  const models = useMemo(() => sortedModels(search.data?.models ?? [], sort), [search.data?.models, sort]);
  const data = search.data;
  const hasQuery = query.length > 0;

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <FullBleedHeader title={t("remoteModels.title")} subtitle={t("remoteModels.subtitle")} />

      <div className="min-h-0 min-w-0 flex-1 overflow-auto">
        <div className="space-y-4 p-4 sm:p-5">
          {providerStatus.isLoading && <Skeleton className="h-9 w-full" />}
          {providerStatus.isError && (
            <ErrorState error={providerStatus.error} onRetry={() => void providerStatus.refetch()} />
          )}
          {providerStatus.data && <RemoteModelProviderStatus status={providerStatus.data} />}

          <ModelDownloadsPanel />
        </div>

        <FullBleedToolbar>
          <Field label={t("remoteModels.searchLabel")} className="min-w-[16rem] flex-1">
            <div className="relative">
              <Search className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-faint" />
              <Input
                value={input}
                onChange={(event) => setInput(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    submit(input);
                  }
                }}
                className="pl-9"
                placeholder={t("remoteModels.searchPlaceholder")}
                aria-label={t("remoteModels.searchAria")}
              />
            </div>
          </Field>
          <label className="flex h-9 items-center gap-2 rounded-lg border border-line bg-raised px-3 text-sm text-fg">
            <input
              type="checkbox"
              checked={ggufOnly}
              onChange={(event) => toggleGguf(event.target.checked)}
              className="h-4 w-4 accent-iris"
            />
            <span>{t("remoteModels.ggufOnly")}</span>
          </label>
          <Field label={t("remoteModels.sort")} className="min-w-[11rem]">
            <Select value={sort} onChange={(event) => changeSort(event.target.value as Sort)}>
              <option value="relevance">{t("remoteModels.relevance")}</option>
              <option value="downloads">{t("remoteModels.downloads")}</option>
              <option value="recent">{t("remoteModels.recent")}</option>
            </Select>
          </Field>
        </FullBleedToolbar>

        {!hasQuery && (
          <Empty icon={<Search className="h-5 w-5" />} title={t("remoteModels.title")}>
            {t("remoteModels.searchHint")}
          </Empty>
        )}
        {search.isError && (
          <div className="p-5">
            <ErrorState error={search.error} onRetry={() => void search.refetch()} />
          </div>
        )}
        {hasQuery && search.isLoading && (
          <div className="space-y-2 p-5">
            {[0, 1, 2, 3, 4].map((index) => (
              <Skeleton key={index} className="h-12 w-full" />
            ))}
          </div>
        )}
        {hasQuery && data && models.length === 0 && (
          <Empty icon={<Search className="h-5 w-5" />} title={t("remoteModels.noResults")}>
            {t("remoteModels.noResultsDetail")}
          </Empty>
        )}
        {hasQuery && data && models.length > 0 && (
          <>
            <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-2.5 text-xs text-faint">
              <span className="flex items-center gap-2">
                <SlidersHorizontal className="h-3.5 w-3.5" />
                {t("remoteModels.results", { count: data.total })}
              </span>
              {search.isFetching && <span>{t("remoteModels.searchHint")}</span>}
            </div>
            <ScrollX>
              <table className="w-full min-w-[60rem] text-sm">
                <thead className="sticky top-0 z-10 bg-surface/95 backdrop-blur-sm">
                  <tr className="text-left text-xs text-faint">
                    <th className="px-5 py-2">{t("models.columnName")}</th>
                    <th className="px-5 py-2">{t("transfers.columnState")}</th>
                    <th className="px-5 py-2">{t("remoteModels.downloads")}</th>
                    <th className="px-5 py-2">{t("remoteModels.likes")}</th>
                    <th className="px-5 py-2">{t("remoteModels.updated")}</th>
                    <th className="px-5 py-2">{t("models.columnActions")}</th>
                  </tr>
                </thead>
                <tbody>
                  {models.map((model) => (
                    <ResultRow
                      key={model.repo_id}
                      model={model}
                      onOpen={(selected) => setDetailsRepo(selected.repo_id)}
                    />
                  ))}
                </tbody>
              </table>
            </ScrollX>
            {(page > 1 || data.has_next) && (
              <div className="flex items-center justify-between gap-3 border-t border-line px-5 py-3">
                <Button size="sm" variant="ghost" disabled={page <= 1} icon={<ChevronLeft className="h-3.5 w-3.5" />} onClick={() => changePage(page - 1)}>
                  {t("common.previous")}
                </Button>
                <span className="font-mono text-xs text-faint">{page}</span>
                <Button size="sm" variant="ghost" disabled={!data.has_next} icon={<ChevronRight className="h-3.5 w-3.5" />} onClick={() => changePage(page + 1)}>
                  {t("common.next")}
                </Button>
              </div>
            )}
          </>
        )}
      </div>

      <RemoteModelDetails
        repoId={detailsRepo}
        open={detailsRepo != null}
        onClose={() => setDetailsRepo(null)}
        onChoose={(file, details) => setDownload({ file, details })}
      />
      <DownloadModelDialog
        open={download != null}
        file={download?.file ?? null}
        details={download?.details ?? null}
        onClose={() => setDownload(null)}
        onStarted={() => {
          void queryClient.invalidateQueries({ queryKey: keys.jobs });
        }}
      />
    </div>
  );
}
