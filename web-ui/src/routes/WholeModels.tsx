import { Boxes, FolderSearch, RefreshCw, Scissors, Upload } from "lucide-react";
import { Link } from "react-router-dom";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Footnote,
  PageHeader,
  Panel,
  PanelHeader,
  Skeleton,
} from "../components/ui";
import { bytes, num } from "../lib/format";
import { useLocalCatalog } from "../lib/queries";
import { isWholeModel, type DiscoveredModel, type ModelSource } from "../lib/types";
import { useI18n } from "../i18n/provider";

const SOURCE_TONE: Record<string, "sky" | "amber" | "neutral"> = {
  lmstudio: "sky",
  ollama: "amber",
};

export function ModelRow({
  model,
  actions,
}: {
  model: DiscoveredModel;
  actions?: React.ReactNode;
}) {
  const { t } = useI18n();
  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-2 px-5 py-3.5 transition-colors duration-150 hover:bg-raised">
      <div className="min-w-[14rem] flex-1">
        <div className="flex flex-wrap items-center gap-2">
          <span className="truncate font-medium text-fg" title={model.name}>
            {model.name}
          </span>
          <Badge tone={SOURCE_TONE[model.source] ?? "neutral"}>{model.source_label}</Badge>
          {model.quant && (
            <Badge mono tone="neutral">
              {model.quant}
            </Badge>
          )}
        </div>
        <p className="break-path mt-1 font-mono text-[0.7rem] text-faint">{model.path}</p>
      </div>

      <dl className="flex shrink-0 items-center gap-5">
        <div>
          <dt className="eyebrow">{t("models.size")}</dt>
          <dd className="font-mono text-[0.82rem] text-fg">{bytes(model.size_bytes)}</dd>
        </div>
        <div>
          <dt className="eyebrow">{t("models.layers")}</dt>
          <dd className="font-mono text-[0.82rem] text-fg">
            {model.total_layers != null ? num(model.total_layers) : "—"}
          </dd>
        </div>
        <div className="hidden sm:block">
          <dt className="eyebrow">{t("models.architecture")}</dt>
          <dd className="text-[0.82rem] text-fg">{model.architecture ?? "—"}</dd>
        </div>
      </dl>

      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </div>
  );
}

export function ScannedRoots({
  roots,
}: {
  roots: Array<{ path: string; exists: boolean; found: number; source: ModelSource }>;
}) {
  const { t } = useI18n();
  return (
    <div className="border-t border-line px-5 py-3">
      <div className="eyebrow mb-2">{t("models.scannedFolders")}</div>
      <ul className="space-y-1">
        {roots.map((r) => (
          <li
            key={r.path}
            className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-0.5"
          >
            <span className="break-path font-mono text-[0.72rem] text-muted">{r.path}</span>
            <span className="shrink-0 text-[0.72rem] text-faint">
              {r.exists ? t("models.fileCount", { count: r.found }) : t("models.folderMissing")}
            </span>
          </li>
        ))}
      </ul>
      <p className="mt-2.5 text-xs text-faint">
        {t("models.folderSettings")}
      </p>
    </div>
  );
}

export function WholeModels() {
  const { t } = useI18n();
  const catalog = useLocalCatalog();
  // Fatias têm `shard_range` preenchido e ficam fora da lista de modelos
  // inteiros — o catálogo continua trazendo as duas, e quem precisa das fatias
  // lê `shard_sets` (ou faz a pergunta inversa).
  const models = (catalog.data?.models ?? []).filter(isWholeModel);

  return (
    <>
      <PageHeader
        title={t("models.fullTitle")}
        subtitle={t("models.fullSubtitle")}
        actions={
          <Button
            icon={<RefreshCw className="h-3.5 w-3.5" />}
            loading={catalog.isFetching}
            onClick={() => void catalog.refetch()}
          >
            {t("models.rescan")}
          </Button>
        }
      />

      {catalog.isError && (
        <ErrorState error={catalog.error} onRetry={() => void catalog.refetch()} />
      )}

      {catalog.isLoading && (
        <Panel className="space-y-3 p-5">
          {[0, 1, 2].map((i) => (
            <Skeleton key={i} className="h-14 w-full" />
          ))}
        </Panel>
      )}

      {catalog.data && (
        <Panel>
          <PanelHeader
            eyebrow={catalog.data.hostname}
            title={t("models.machineCount", { count: models.length })}
            subtitle={t("models.slowScan")}
          />
          {models.length === 0 ? (
            <Empty
              icon={<FolderSearch className="h-5 w-5" />}
              title={t("models.emptyTitle")}
              action={
                <Link to="/config/pastas">
                  <Button variant="primary">{t("models.chooseFolders")}</Button>
                </Link>
              }
            >
              A varredura cobriu as pastas listadas abaixo e não achou nenhum `.gguf`
              completo.
            </Empty>
          ) : (
            <div className="divide-y divide-line">
              {models.map((m) => (
                <ModelRow
                  key={m.id}
                  model={m}
                  actions={
                    <>
                      <Link
                        to={`/modelos/fatiar?path=${encodeURIComponent(m.path)}&name=${encodeURIComponent(m.name)}`}
                      >
                        <Button size="sm" icon={<Scissors className="h-3.5 w-3.5" />}>
                          {t("models.slice")}
                        </Button>
                      </Link>
                      <Link
                        to={`/transferencias/enviar?path=${encodeURIComponent(m.path)}`}
                      >
                        <Button
                          size="sm"
                          variant="ghost"
                          icon={<Upload className="h-3.5 w-3.5" />}
                          aria-label={t("models.sendAria", { name: m.name })}
                        >
                          {t("models.send")}
                        </Button>
                      </Link>
                    </>
                  }
                />
              ))}
            </div>
          )}
          {catalog.data.scanned.length > 0 && (
            <ScannedRoots roots={catalog.data.scanned} />
          )}
        </Panel>
      )}

      {catalog.data && models.length > 0 && (
        <Footnote
          className="mt-4"
          icon={<Boxes className="h-3.5 w-3.5" />}
        >
          {t("models.localSlice")}
        </Footnote>
      )}
    </>
  );
}
