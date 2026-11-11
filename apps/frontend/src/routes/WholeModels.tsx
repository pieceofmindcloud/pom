import { FolderSearch, RefreshCw, Trash2 } from "lucide-react";
import { SendToSelect } from "../components/SendToSelect";
import { Transfers } from "./Transfers";
import { isActiveTransfer } from "../lib/transferStatus";
import { Link, useSearchParams } from "react-router-dom";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  FullBleedHeader,
  Mono,
  ScrollX,
  Skeleton,
  Tabs,
} from "../components/ui";
import { bytes, num, optionalBasename } from "../lib/format";
import { deleteModel } from "../lib/deleteModel";
import { useLocalCatalog, useOffers, useTransfers } from "../lib/queries";
import { useToast } from "../lib/toast";
import {
  isWholeModel,
  type DiscoveredModel,
  type ImageModelFile,
  type ModelSource,
  type Sidecar,
  type SidecarKind,
} from "../lib/types";
import { useI18n } from "../i18n/provider";
import { isDemoUser } from "../lib/session";
import { useState } from "react";
import { Confirm, Modal } from "../components/Confirm";
import { SidecarLinkEditor } from "../components/SidecarLinkEditor";
import { useClusterCatalog } from "../lib/queries";

const SOURCE_TONE: Record<string, "sky" | "amber" | "neutral"> = {
  lmstudio: "sky",
  ollama: "amber",
};

type DeleteConfirmation = {
  message: string;
  resolve: (confirmed: boolean) => void;
};

function useModelFileDeletion(refetchCatalog: () => void) {
  const { t } = useI18n();
  const toast = useToast();
  const [confirmation, setConfirmation] = useState<DeleteConfirmation | null>(null);

  function askForConfirmation(message: string): Promise<boolean> {
    return new Promise((resolve) => setConfirmation({ message, resolve }));
  }

  function answerConfirmation(confirmed: boolean) {
    if (!confirmation) return;
    const pending = confirmation;
    setConfirmation(null);
    pending.resolve(confirmed);
  }

  async function remove(path: string, name: string) {
    try {
      const out = await deleteModel(path, name, askForConfirmation);
      if (out.status === "deleted") {
        toast.success(t("models.deleted", { name, bytes: bytes(out.freedBytes) }));
        refetchCatalog();
      }
    } catch (error) {
      toast.fromError(t("models.deleteFailed"), error);
    }
  }

  return {
    remove,
    dialog: (
      <Confirm
        open={confirmation !== null}
        title={t("models.deleteDialogTitle")}
        description={<p className="whitespace-pre-line break-words">{confirmation?.message}</p>}
        confirmLabel={t("models.deleteButton")}
        tone="danger"
        onConfirm={() => answerConfirmation(true)}
        onCancel={() => answerConfirmation(false)}
      />
    ),
  };
}

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

export function ModelRow({
  model,
  onDelete,
}: {
  model: DiscoveredModel;
  onDelete: (path: string, name: string) => Promise<void>;
}) {
  const { t } = useI18n();
  const mmprojName = optionalBasename(model.mmproj_path);
  const [linkOpen, setLinkOpen] = useState(false);

  return (
    <>
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
        {!isDemoUser() && (
          <Button size="sm" variant="ghost" className="ml-2 h-6 px-2 text-[0.68rem]" onClick={() => setLinkOpen(true)}>
            {t("sidecars.linkTitle")}
          </Button>
        )}
        {linkOpen && <WholeModelLinks model={model} onClose={() => setLinkOpen(false)} />}
      </td>
      <td className="whitespace-nowrap px-5 py-3.5">
        <Mono>{bytes(model.size_bytes)}</Mono>
      </td>
      <td className="whitespace-nowrap px-5 py-3.5">
        {/* A demonstration visitor sees the catalog, not its actions. */}
        <div className={isDemoUser() ? "hidden" : "flex shrink-0 items-center gap-2"}>
          {!model.multipart && <SendToSelect path={model.path} name={model.name} />}
          <Button
            size="sm"
            variant="ghost"
            icon={<Trash2 className="h-3.5 w-3.5" />}
            aria-label={t("models.deleteAria", { name: model.name })}
            onClick={() => void onDelete(model.path, model.name)}
          >
            {t("models.deleteButton")}
          </Button>
        </div>
      </td>
    </tr>
    {model.multipart && <MultipartFiles model={model} onDelete={onDelete} />}
    </>
  );
}

function MultipartFiles({
  model,
  onDelete,
}: {
  model: DiscoveredModel;
  onDelete: (path: string, name: string) => Promise<void>;
}) {
  const { t } = useI18n();
  const parts = model.multipart_parts ?? [];
  return (
    <tr>
      <td colSpan={7} className="px-5 pb-4">
        <div className="rounded-xl border border-line bg-surface/50 p-4">
          <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
            <p className="text-sm font-medium text-fg">{t("models.multipartFiles", { count: parts.length })}</p>
            <Mono>{bytes(model.size_bytes)}</Mono>
          </div>
          {model.multipart_error && (
            <p className="mb-3 rounded-lg border border-amber/30 bg-amber/5 px-3 py-2 text-xs text-rose">
              {model.multipart_error}
            </p>
          )}
          {parts.length > 0 && (
            <ul className="divide-y divide-line">
              {parts.map((part) => {
                const name = optionalBasename(part.path) ?? part.path;
                return (
                  <li key={part.path} className="flex flex-wrap items-center justify-between gap-3 py-2 first:pt-0 last:pb-0">
                    <div className="min-w-0">
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="break-path font-mono text-xs text-fg">{name}</span>
                        <Badge mono tone="neutral">{t("models.multipartPart", { index: part.index + 1, count: parts.length })}</Badge>
                      </div>
                      <p className="break-path mt-1 font-mono text-[0.68rem] text-faint">{part.path}</p>
                    </div>
                    <div className="flex shrink-0 items-center gap-3">
                      <Mono>{bytes(part.bytes)}</Mono>
                      {!isDemoUser() && (
                        <Button
                          size="sm"
                          variant="ghost"
                          icon={<Trash2 className="h-3.5 w-3.5" />}
                          aria-label={t("models.deleteAria", { name })}
                          onClick={() => onDelete(part.path, name)}
                        >
                          {t("models.deleteButton")}
                        </Button>
                      )}
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      </td>
    </tr>
  );
}

/** One image model file: no layers, no slicing; delete only. */
export function ImageModelRow({
  file,
  onDelete,
}: {
  file: ImageModelFile;
  onDelete: (path: string, name: string) => Promise<void>;
}) {
  const { t } = useI18n();

  return (
    <tr className="border-t border-line align-top transition-colors duration-150 hover:bg-raised">
      <td className="min-w-[16rem] px-5 py-3.5">
        <span className="truncate font-medium text-fg" title={file.name}>
          {file.name}
        </span>
        <p className="break-path mt-1 font-mono text-[0.7rem] text-faint">{file.path}</p>
        {file.metadata_error && (
          <p className="mt-1 text-[0.7rem] text-rose">
            {t("models.imageUnreadable", { error: file.metadata_error })}
          </p>
        )}
      </td>
      <td className="whitespace-nowrap px-5 py-3.5">
        <Badge tone={SOURCE_TONE[file.source] ?? "neutral"}>{file.source_label}</Badge>
      </td>
      <td className="whitespace-nowrap px-5 py-3.5">
        <Badge tone={file.category === "other" ? "neutral" : "iris"}>
          {t(`models.category.${file.category}`)}
        </Badge>
      </td>
      <td className="hidden whitespace-nowrap px-5 py-3.5 text-fg sm:table-cell">{file.family ?? "—"}</td>
      <td className="whitespace-nowrap px-5 py-3.5">
        <Mono>{file.dtype ?? "—"}</Mono>
      </td>
      <td className="whitespace-nowrap px-5 py-3.5">
        <Mono>{bytes(file.size_bytes)}</Mono>
      </td>
      <td className="whitespace-nowrap px-5 py-3.5">
        {!isDemoUser() && <div className="flex shrink-0 items-center gap-2">
        <SendToSelect path={file.path} name={file.name} />
        <Button
          size="sm"
          variant="ghost"
          icon={<Trash2 className="h-3.5 w-3.5" />}
          aria-label={t("models.deleteAria", { name: file.name })}
          onClick={() => void onDelete(file.path, file.name)}
        >
          {t("models.deleteButton")}
        </Button>
        </div>}
      </td>
    </tr>
  );
}

function SidecarRow({
  sidecar,
  onDelete,
}: {
  sidecar: Sidecar;
  onDelete: (path: string, name: string) => Promise<void>;
}) {
  const { t } = useI18n();

  return (
    <tr className="border-t border-line align-top transition-colors duration-150 hover:bg-raised">
      <td className="min-w-[16rem] px-5 py-3.5">
        <span className="break-path font-mono text-xs text-fg">{sidecar.file_name}</span>
        <p className="break-path mt-1 font-mono text-[0.68rem] text-faint">{sidecar.path}</p>
      </td>
      <td className="min-w-[12rem] px-5 py-3.5 text-sm text-fg">{sidecar.general_name ?? "—"}</td>
      <td className="whitespace-nowrap px-5 py-3.5 text-sm text-fg">{sidecar.architecture ?? "—"}</td>
      <td className="whitespace-nowrap px-5 py-3.5"><Mono>{sidecar.dim != null ? num(sidecar.dim) : "—"}</Mono></td>
      <td className="whitespace-nowrap px-5 py-3.5">{sidecar.quant ?? "—"}</td>
      <td className="whitespace-nowrap px-5 py-3.5"><Mono>{bytes(sidecar.size_bytes)}</Mono></td>
      <td className="whitespace-nowrap px-5 py-3.5">
        {!isDemoUser() && (
          <div className="flex shrink-0 items-center gap-2">
            <SendToSelect path={sidecar.path} name={sidecar.file_name} />
            <Button
              size="sm"
              variant="ghost"
              icon={<Trash2 className="h-3.5 w-3.5" />}
              aria-label={t("models.deleteAria", { name: sidecar.file_name })}
              onClick={() => void onDelete(sidecar.path, sidecar.file_name)}
            >
              {t("models.deleteButton")}
            </Button>
          </div>
        )}
      </td>
    </tr>
  );
}

function SidecarTable({
  sidecars,
  kind,
  onDelete,
}: {
  sidecars: Sidecar[];
  kind: SidecarKind;
  onDelete: (path: string, name: string) => Promise<void>;
}) {
  const { t } = useI18n();
  const files = sidecars.filter((sidecar) => sidecar.kind === kind);
  if (files.length === 0) {
    const isMmproj = kind === "mmproj";
    return (
      <Empty
        icon={<FolderSearch className="h-5 w-5" />}
        title={t(isMmproj ? "models.sidecarEmptyMmprojTitle" : "models.sidecarEmptyMtpTitle")}
      >
        {t(isMmproj ? "models.sidecarEmptyMmprojDetail" : "models.sidecarEmptyMtpDetail")}
      </Empty>
    );
  }

  return (
    <ScrollX>
      <table className="w-full min-w-[64rem] text-sm">
        <thead className="sticky top-0 z-10 bg-surface/95 backdrop-blur-sm">
          <tr className="text-left text-xs text-faint">
            <th className="px-5 py-2">{t("models.columnName")}</th>
            <th className="px-5 py-2">{t("models.columnFamily")}</th>
            <th className="px-5 py-2">{t("models.sidecarArchitecture")}</th>
            <th className="px-5 py-2">{t("models.sidecarDimension")}</th>
            <th className="px-5 py-2">{t("models.quant")}</th>
            <th className="px-5 py-2">{t("models.size")}</th>
            <th className="px-5 py-2">{t("models.columnActions")}</th>
          </tr>
        </thead>
        <tbody>{files.map((sidecar) => <SidecarRow key={sidecar.path} sidecar={sidecar} onDelete={onDelete} />)}</tbody>
      </table>
    </ScrollX>
  );
}

function ImageModelTable({
  files,
  onDelete,
}: {
  files: ImageModelFile[];
  onDelete: (path: string, name: string) => Promise<void>;
}) {
  const { t } = useI18n();
  if (files.length === 0) {
    return (
      <Empty icon={<FolderSearch className="h-5 w-5" />} title={t("models.imageEmptyTitle")}>
        {t("models.imageEmptyDetail")}
      </Empty>
    );
  }
  return (
    <ScrollX>
      <table className="w-full min-w-[56rem] text-sm">
        <thead className="sticky top-0 z-10 bg-surface/95 backdrop-blur-sm">
          <tr className="text-left text-xs text-faint">
            <th className="px-5 py-2">{t("models.columnName")}</th>
            <th className="px-5 py-2">{t("models.columnSource")}</th>
            <th className="px-5 py-2">{t("models.columnCategory")}</th>
            <th className="hidden px-5 py-2 sm:table-cell">{t("models.columnFamily")}</th>
            <th className="px-5 py-2">{t("models.columnDtype")}</th>
            <th className="px-5 py-2">{t("models.size")}</th>
            <th className="px-5 py-2">{t("models.columnActions")}</th>
          </tr>
        </thead>
        <tbody>
          {files.map((file) => (
            <ImageModelRow key={file.id} file={file} onDelete={onDelete} />
          ))}
        </tbody>
      </table>
    </ScrollX>
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
  const deletion = useModelFileDeletion(() => void catalog.refetch());

  // Slices have `shard_range` filled in and stay out of the list of whole
  // models - the catalog still brings both, and whoever needs the slices
  // reads `shard_sets` (or asks the inverse question).
  const models = (catalog.data?.models ?? []).filter(isWholeModel);
  const imageFiles = catalog.data?.image_files ?? [];
  const sidecars = catalog.data?.sidecars ?? [];
  const [params, setParams] = useSearchParams();
  // The Transfers tab shows only while a file is moving or an offer waits.
  const transfers = useTransfers();
  const offers = useOffers();
  const transferring =
    (transfers.data?.jobs ?? []).some((job) => isActiveTransfer(job.state)) ||
    (offers.data?.offers ?? []).some((offer) => !offer.kind || offer.kind === "slice");
  const requestedKind = params.get("kind");
  const kind = requestedKind === "image" || requestedKind === "mmproj" || requestedKind === "mtp"
    ? requestedKind
    : requestedKind === "transfers" && transferring
      ? "transfers"
      : "llm";
  const chooseKind = (next: "llm" | "image" | "mmproj" | "mtp" | "transfers") => {
    const updated = new URLSearchParams(params);
    if (next === "llm") updated.delete("kind");
    else updated.set("kind", next);
    setParams(updated, { replace: true });
  };

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

      <Tabs
        items={[
          { value: "llm", label: t("models.kindLlm") },
          { value: "image", label: t("models.kindImage") },
          { value: "mmproj", label: t("models.mmproj") },
          { value: "mtp", label: t("models.kindMtp") },
          ...(transferring ? [{ value: "transfers" as const, label: t("transfers.title") }] : []),
        ]}
        value={kind}
        onChange={chooseKind}
        label={t("models.fullTitle")}
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

        {kind === "transfers" && <Transfers embedded />}
        {catalog.data && kind === "image" && <ImageModelTable files={imageFiles} onDelete={deletion.remove} />}
        {catalog.data && kind === "mmproj" && <SidecarTable sidecars={sidecars} kind="mmproj" onDelete={deletion.remove} />}
        {catalog.data && kind === "mtp" && <SidecarTable sidecars={sidecars} kind="mtp" onDelete={deletion.remove} />}

        {catalog.data && kind === "llm" && models.length === 0 && (
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

        {catalog.data && kind === "llm" && models.length > 0 && (
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
                  <ModelRow key={m.id} model={m} onDelete={deletion.remove} />
                ))}
              </tbody>
            </table>
          </ScrollX>
        )}

        {catalog.data && catalog.data.scanned.length > 0 && (
          <ScannedRoots roots={catalog.data.scanned} />
        )}
      </div>

      {deletion.dialog}
    </div>
  );
}
