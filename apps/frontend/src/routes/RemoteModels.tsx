import { ChevronLeft, ChevronRight, Search, SlidersHorizontal } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useSearchParams } from "react-router-dom";
import { DownloadModelDialog } from "../components/DownloadModelDialog";
import { ModelDownloadsPanel } from "../components/ModelDownloadsPanel";
import { RemoteModelCard } from "../components/RemoteModelCard";
import { RemoteModelDetails } from "../components/RemoteModelDetails";
import { RemoteModelProviderStatus } from "../components/RemoteModelProviderStatus";
import { Button, Empty, ErrorState, Field, Input, PageHeader, Panel, Select, Skeleton } from "../components/ui";
import { useI18n } from "../i18n/provider";
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
    <div className="space-y-5">
      <PageHeader title={t("remoteModels.title")} subtitle={t("remoteModels.subtitle")} />

      {providerStatus.isLoading && <Panel className="px-4 py-3.5 sm:px-5"><Skeleton className="h-9 w-full" /></Panel>}
      {providerStatus.isError && <ErrorState error={providerStatus.error} onRetry={() => void providerStatus.refetch()} />}
      {providerStatus.data && <RemoteModelProviderStatus status={providerStatus.data} />}

      <ModelDownloadsPanel />

      <Panel className="p-4 sm:p-5">
        <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_auto_auto] lg:items-end">
          <Field label={t("remoteModels.searchLabel")}>
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
        </div>
        <p className="mt-3 flex items-start gap-2 text-xs text-faint">
          <SlidersHorizontal className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          {t("remoteModels.searchHint")}
        </p>
      </Panel>

      {!hasQuery && <Empty icon={<Search className="h-5 w-5" />} title={t("remoteModels.title")}>{t("remoteModels.searchHint")}</Empty>}
      {search.isError && <ErrorState error={search.error} onRetry={() => void search.refetch()} />}
      {hasQuery && search.isLoading && (
        <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
          {[0, 1, 2, 3, 4, 5].map((index) => <Skeleton key={index} className="h-56" />)}
        </div>
      )}
      {hasQuery && data && models.length === 0 && (
        <Empty icon={<Search className="h-5 w-5" />} title={t("remoteModels.noResults")}>
          {t("remoteModels.noResultsDetail")}
        </Empty>
      )}
      {hasQuery && data && models.length > 0 && (
        <>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <p className="eyebrow">{t("remoteModels.results", { count: data.total })}</p>
            {search.isFetching && <span className="text-xs text-faint">{t("remoteModels.searchHint")}</span>}
          </div>
          <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
            {models.map((model) => (
              <RemoteModelCard key={model.repo_id} model={model} onOpen={(selected) => setDetailsRepo(selected.repo_id)} />
            ))}
          </div>
          {(page > 1 || data.has_next) && (
            <div className="flex items-center justify-between gap-3">
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
