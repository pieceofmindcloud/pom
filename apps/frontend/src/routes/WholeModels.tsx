import { Boxes, FolderSearch, RefreshCw, Scissors, Trash2 } from "lucide-react";
import { Link } from "react-router-dom";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Footnote,
  FullBleedHeader,
  Mono,
  ScrollX,
  Skeleton,
} from "../components/ui";
import { bytes, num, optionalBasename } from "../lib/format";
import { deleteModel } from "../lib/deleteModel";
import { useLocalCatalog } from "../lib/queries";
import { useToast } from "../lib/toast";
import { isWholeModel, type DiscoveredModel, type ModelSource } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { useState } from "react";
import { Modal } from "../components/Confirm";
import { SidecarLinkEditor } from "../components/SidecarLinkEditor";
import { useClusterCatalog } from "../lib/queries";

const SOURCE_TONE: Record<string, "sky" | "amber" | "neutral"> = {
  lmstudio: "sky",
  ollama: "amber",
};

/** The vision/MTP link of one whole model, in a window over the table. */
function WholeModelLinks({ model, onClose }: { model: DiscoveredModel; onClose: () => void }) {
  const { t } = useI18n();
  const cluster = useClusterCatalog();
  return (
    <Modal
      open
      size="lg"
      title={`${t("sidecars.linkTitle")} · ${model.name}`}
      onClose={onClose}
      footer={<Button variant="ghost" onClick={onClose}>{t("common.close")}</Button>}
    >
      <SidecarLinkEditor
        modelKey={model.name}
        model={model}
        embeddedMtp={model.capabilities?.mtp === true}
        peers={cluster.data?.peers}
      />
    </Modal>
  );
}

export function ModelRow({ model }: { model: DiscoveredModel }) {
  const { t } = useI18n();
  const toast = useToast();
  const catalog = useLocalCatalog();
  const mmprojName = optionalBasename(model.mmproj_path);
  const [linkOpen, setLinkOpen] = useState(false);

  async function remove() {
    try {
      const out = await deleteModel(model.path, model.name, (m) => window.confirm(m));
      if (out.status === "deleted") {
        toast.success(t("models.deleted", { name: model.name, bytes: bytes(out.freedBytes) }));
        void catalog.refetch();
      }
    } catch (e) {
      toast.fromError(t("models.deleteFailed"), e);
    }
  }

  return (
    <tr className="border-t border-line align-top transition-colors duration-150 hover:bg-raised">
      <td className="min-w-[16rem] px-5 py-3.5">
        <div className="flex flex-wrap items-center gap-2">
          <span className="truncate font-medium text-fg" title={model.name}>
            {model.name}
          </span>
          {model.quant && (
            <Badge mono tone="neutral">
              {model.quant}
            </Badge>
          )}
        </div>
        <p className="break-path mt-1 font-mono text-[0.7rem] text-faint">{model.path}</p>
      </td>
      <td className="whitespace-nowrap px-5 py-3.5">
        <Badge tone={SOURCE_TONE[model.source] ?? "neutral"}>{model.source_label}</Badge>
      </td>
      <td className="whitespace-nowrap px-5 py-3.5">
        <Mono>{model.total_layers != null ? num(model.total_layers) : "—"}</Mono>
      </td>
      <td className="hidden whitespace-nowrap px-5 py-3.5 text-fg sm:table-cell">
        {model.architecture ?? "—"}
      </td>
      <td className="min-w-[10rem] px-5 py-3.5">
        {mmprojName ? (
          <span className="break-path font-mono text-[0.78rem] text-fg" title={model.mmproj_path ?? undefined}>
            {mmprojName}
          </span>
        ) : (
          <span className="text-[0.78rem] text-faint">{t("models.mmprojNone")}</span>
        )}
        <Button size="sm" variant="ghost" className="ml-2 h-6 px-2 text-[0.68rem]" onClick={() => setLinkOpen(true)}>
          {t("sidecars.linkTitle")}
        </Button>
        {linkOpen && <WholeModelLinks model={model} onClose={() => setLinkOpen(false)} />}
      </td>
      <td className="whitespace-nowrap px-5 py-3.5">
        <Mono>{bytes(model.size_bytes)}</Mono>
      </td>
      <td className="whitespace-nowrap px-5 py-3.5">
        <div className="flex shrink-0 items-center gap-2">
          <Link
            to={`/models/slice?path=${encodeURIComponent(model.path)}&name=${encodeURIComponent(model.name)}`}
          >
            <Button size="sm" icon={<Scissors className="h-3.5 w-3.5" />}>
              {t("models.slice")}
            </Button>
          </Link>
          <Button
            size="sm"
            variant="ghost"
            icon={<Trash2 className="h-3.5 w-3.5" />}
            aria-label={t("models.deleteAria", { name: model.name })}
            onClick={() => void remove()}
          >
            {t("models.deleteButton")}
          </Button>
        </div>
      </td>
    </tr>
  );
}

export function ScannedRoots({
  roots,
}: {
  roots: Array<{ path: string; exists: boolean; found: number; source: ModelSource }>;
}) {
  const { t } = useI18n();
  return (
    <div className="border-t border-line bg-surface/40 px-5 py-3">
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

  // Slices have `shard_range` filled in and stay out of the list of whole
  // models - the catalog still brings both, and whoever needs the slices
  // reads `shard_sets` (or asks the inverse question).
  const models = (catalog.data?.models ?? []).filter(isWholeModel);

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col bg-ink">
      <FullBleedHeader
        title={t("models.fullTitle")}
        subtitle={catalog.data ? t("models.machineCount", { count: models.length }) : t("models.fullSubtitle")}
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

      <div className="min-h-0 min-w-0 flex-1 overflow-auto">
        {catalog.isError && (
          <div className="p-5">
            <ErrorState error={catalog.error} onRetry={() => void catalog.refetch()} />
          </div>
        )}

        {catalog.isLoading && (
          <div className="space-y-3 p-5">
            {[0, 1, 2].map((i) => (
              <Skeleton key={i} className="h-14 w-full" />
            ))}
          </div>
        )}

        {catalog.data && models.length === 0 && (
          <Empty
            icon={<FolderSearch className="h-5 w-5" />}
            title={t("models.emptyTitle")}
            action={
              <Link to="/config/folders">
                <Button variant="primary">{t("models.chooseFolders")}</Button>
              </Link>
            }
          >
            {t("models.emptyDetail")}
          </Empty>
        )}

        {catalog.data && models.length > 0 && (
          <ScrollX>
            <table className="w-full min-w-[64rem] text-sm">
              <thead className="sticky top-0 z-10 bg-surface/95 backdrop-blur-sm">
                <tr className="text-left text-xs text-faint">
                  <th className="px-5 py-2">{t("models.columnName")}</th>
                  <th className="px-5 py-2">{t("models.columnSource")}</th>
                  <th className="px-5 py-2">{t("models.layers")}</th>
                  <th className="hidden px-5 py-2 sm:table-cell">{t("models.architecture")}</th>
                  <th className="px-5 py-2">{t("models.mmproj")}</th>
                  <th className="px-5 py-2">{t("models.size")}</th>
                  <th className="px-5 py-2">{t("models.columnActions")}</th>
                </tr>
              </thead>
              <tbody>
                {models.map((m) => (
                  <ModelRow key={m.id} model={m} />
                ))}
              </tbody>
            </table>
          </ScrollX>
        )}

        {catalog.data && catalog.data.scanned.length > 0 && (
          <ScannedRoots roots={catalog.data.scanned} />
        )}
      </div>

      {catalog.data && models.length > 0 && (
        <Footnote
          className="shrink-0 border-t border-line px-4 py-2.5"
          icon={<Boxes className="h-3.5 w-3.5" />}
        >
          {t("models.localSlice")}
        </Footnote>
      )}
    </div>
  );
}
