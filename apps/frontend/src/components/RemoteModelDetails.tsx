import { Check, Download, FileText, LockKeyhole, X } from "lucide-react";
import { useI18n } from "../i18n/provider";
import { useRemoteModelDetails } from "../lib/queries";
import { bytes, num, since } from "../lib/format";
import type { RemoteGgufFile, RemoteModelDetails as RemoteModelDetailsData } from "../lib/types";
import { Modal } from "./Confirm";
import { Badge, Button, Empty, ErrorState, Panel, Skeleton } from "./ui";

export function RemoteModelDetails({
  repoId,
  revision,
  open,
  onClose,
  onChoose,
}: {
  repoId: string | null;
  revision?: string;
  open: boolean;
  onClose: () => void;
  onChoose: (file: RemoteGgufFile, details: RemoteModelDetailsData) => void;
}) {
  const { t } = useI18n();
  const details = useRemoteModelDetails(repoId, revision);
  const model = details.data;

  return (
    <Modal
      open={open}
      title={model?.repo_id ?? t("remoteModels.detailsTitle")}
      onClose={onClose}
      size="lg"
      footer={
        <Button variant="ghost" icon={<X className="h-3.5 w-3.5" />} onClick={onClose}>
          {t("common.close")}
        </Button>
      }
    >
      {details.isLoading && (
        <div className="space-y-3">
          <Skeleton className="h-5 w-2/3" />
          <Skeleton className="h-16 w-full" />
          <Skeleton className="h-32 w-full" />
        </div>
      )}
      {details.isError && <ErrorState error={details.error} onRetry={() => void details.refetch()} />}
      {model && <DetailsBody details={model} onChoose={onChoose} />}
    </Modal>
  );
}

function DetailsBody({
  details,
  onChoose,
}: {
  details: RemoteModelDetailsData;
  onChoose: (file: RemoteGgufFile, details: RemoteModelDetailsData) => void;
}) {
  const { t } = useI18n();
  return (
    <div className="space-y-5">
      <div>
        <div className="flex flex-wrap items-center gap-2">
          {details.gated && (
            <Badge tone="amber">
              <LockKeyhole className="h-3 w-3" />
              {t("remoteModels.gated")}
            </Badge>
          )}
          {details.private && <Badge tone="amber">{t("remoteModels.private")}</Badge>}
          {details.revision && <Badge mono>{details.revision}</Badge>}
          {details.last_modified && <span className="text-xs text-faint">{since(details.last_modified)}</span>}
        </div>
        <p className="mt-2 break-path text-sm leading-relaxed text-muted">
          {details.description || t("remoteModels.searchHint")}
        </p>
      </div>

      <dl className="grid gap-4 sm:grid-cols-3">
        <div>
          <dt className="eyebrow">{t("remoteModels.license")}</dt>
          <dd className="mt-1 text-sm text-fg">
            {details.license.detected
              ? details.license.name || details.license.id || details.license.raw_value
              : t("remoteModels.licenseUnknown")}
          </dd>
        </div>
        <div>
          <dt className="eyebrow">{t("remoteModels.downloads")}</dt>
          <dd className="mt-1 font-mono text-sm text-fg">{num(details.downloads)}</dd>
        </div>
        <div>
          <dt className="eyebrow">{t("remoteModels.likes")}</dt>
          <dd className="mt-1 font-mono text-sm text-fg">{num(details.likes)}</dd>
        </div>
      </dl>

      {details.tags.length > 0 && (
        <div>
          <div className="eyebrow mb-2">{t("remoteModels.tags")}</div>
          <div className="flex flex-wrap gap-1.5">
            {details.tags.map((tag) => <Badge key={tag}>{tag}</Badge>)}
          </div>
        </div>
      )}

      <div>
        <div className="mb-2 flex items-center justify-between gap-3">
          <div className="eyebrow">{t("remoteModels.ggufFiles")}</div>
          <span className="font-mono text-xs text-faint">{details.gguf_files.length}</span>
        </div>
        {details.gguf_files.length === 0 ? (
          <Empty icon={<FileText className="h-4 w-4" />} title={t("remoteModels.noResults")}>
            {t("remoteModels.searchHint")}
          </Empty>
        ) : (
          <div className="grid gap-2 sm:grid-cols-2">
            {details.gguf_files.map((file) => (
              <QuantizationRow key={file.filename} file={file} details={details} onChoose={onChoose} />
            ))}
          </div>
        )}
      </div>

      {details.files.length > details.gguf_files.length && (
        <p className="flex items-start gap-2 text-xs text-faint">
          <FileText className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          {details.files.length - details.gguf_files.length} {t("remoteModels.files").toLowerCase()}
        </p>
      )}
    </div>
  );
}

function QuantizationRow({
  file,
  details,
  onChoose,
}: {
  file: RemoteGgufFile;
  details: RemoteModelDetailsData;
  onChoose: (file: RemoteGgufFile, details: RemoteModelDetailsData) => void;
}) {
  const { t } = useI18n();
  const status = file.installed
    ? t("remoteModels.installed")
    : file.downloading
      ? t("remoteModels.downloading")
      : null;
  return (
    <Panel className="flex min-w-0 items-center justify-between gap-3 p-3">
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-mono text-sm font-semibold text-fg">{file.quantization || "GGUF"}</span>
          {status && <Badge tone={file.installed ? "aqua" : "amber"}>{status}</Badge>}
        </div>
        <p className="break-path mt-1 text-xs text-muted" title={file.filename}>{file.filename}</p>
        <p className="mt-1 font-mono text-xs text-faint">{bytes(file.size_bytes)}</p>
      </div>
      <Button
        size="sm"
        variant={status ? "ghost" : "primary"}
        icon={status ? <Check className="h-3.5 w-3.5" /> : <Download className="h-3.5 w-3.5" />}
        disabled={Boolean(status)}
        data-guide={status ? undefined : "model-file"}
        data-guide-quantization={(file.quantization ?? "").toLowerCase()}
        onClick={() => onChoose(file, details)}
      >
        {status || t("remoteModels.download")}
      </Button>
    </Panel>
  );
}
