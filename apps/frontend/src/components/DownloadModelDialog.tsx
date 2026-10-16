import { AlertTriangle, HardDriveDownload } from "lucide-react";
import { useState } from "react";
import { useI18n } from "../i18n/provider";
import { ApiError, startRemoteModelDownload } from "../lib/api";
import { bytes } from "../lib/format";
import type { RemoteGgufFile, RemoteModelDetails } from "../lib/types";
import { useToast } from "../lib/toast";
import { Modal } from "./Confirm";
import { Badge, Button, KeyValue } from "./ui";

function errorCode(error: unknown): string | null {
  if (!(error instanceof ApiError)) return null;
  const body = error.body as { error?: { code?: unknown } } | undefined;
  return typeof body?.error?.code === "string" ? body.error.code : null;
}

export function DownloadModelDialog({
  open,
  details,
  file,
  onClose,
  onStarted,
}: {
  open: boolean;
  details: RemoteModelDetails | null;
  file: RemoteGgufFile | null;
  onClose: () => void;
  onStarted: () => void;
}) {
  const { t } = useI18n();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  if (!details || !file) return null;
  const selectedDetails = details;
  const selectedFile = file;
  const insufficient =
    selectedFile.size_bytes != null &&
    selectedDetails.storage.available_bytes != null &&
    selectedFile.size_bytes > selectedDetails.storage.available_bytes;
  const blocked = insufficient || !selectedDetails.storage.destination_available;

  async function download() {
    setBusy(true);
    try {
      await startRemoteModelDownload({
        repoId: selectedDetails.repo_id,
        revision: selectedDetails.revision ?? undefined,
        filename: selectedFile.filename,
        quantization: selectedFile.quantization ?? undefined,
      });
      toast.success(
        t("remoteModels.downloadStarted"),
        t("remoteModels.downloadStartedDetail", { name: selectedFile.quantization || selectedFile.filename }),
      );
      onStarted();
      onClose();
    } catch (error) {
      const code = errorCode(error);
      const key =
        code === "cli_not_installed"
          ? "remoteModels.cliMissing"
          : code === "authentication_required"
          ? "remoteModels.authRequired"
          : code === "access_denied"
            ? "remoteModels.accessDenied"
            : code === "repository_not_found"
              ? "remoteModels.notFound"
              : code === "insufficient_space"
                ? "remoteModels.insufficientSpace"
                : code === "destination_unavailable"
                  ? "remoteModels.destinationUnavailable"
                  : code === "download_in_progress"
                    ? "remoteModels.downloadInProgress"
                    : code === "model_installed"
                      ? "remoteModels.modelInstalled"
                      : "remoteModels.downloadFailed";
      toast.error(t(key), code === "authentication_required" ? t("remoteModels.authDetail") : undefined);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      open={open}
      title={t("remoteModels.downloadTitle")}
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose} disabled={busy}>{t("common.cancel")}</Button>
          <Button
            variant="primary"
            loading={busy}
            disabled={blocked}
            icon={<HardDriveDownload className="h-3.5 w-3.5" />}
            data-guide="model-download-confirm"
            onClick={() => void download()}
          >
            {t("remoteModels.confirmDownload")}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <div>
          <p className="font-display text-base font-semibold text-fg">{selectedDetails.repo_id}</p>
          <p className="break-path mt-1 font-mono text-xs text-muted">{selectedFile.filename}</p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Badge mono tone="iris">{selectedFile.quantization || "GGUF"}</Badge>
          {selectedDetails.gated && <Badge tone="amber">{t("remoteModels.gated")}</Badge>}
          {selectedDetails.license.detected ? (
            <Badge tone="neutral">{selectedDetails.license.name || selectedDetails.license.id}</Badge>
          ) : (
            <Badge tone="amber">{t("remoteModels.licenseUnknown")}</Badge>
          )}
        </div>
        <dl className="grid gap-3 sm:grid-cols-2">
          <KeyValue label={t("remoteModels.destination")}>
            {selectedDetails.storage.destination_paths.length > 0 ? (
              <ul className="break-path space-y-1 font-mono text-xs">
                {selectedDetails.storage.destination_paths.map((path) => <li key={path}>{path}</li>)}
              </ul>
            ) : (
              t("remoteModels.destination")
            )}
          </KeyValue>
          <KeyValue label={t("remoteModels.requiredSpace")}>{bytes(selectedFile.size_bytes)}</KeyValue>
          <KeyValue label={t("remoteModels.availableSpace")}>
            {bytes(selectedDetails.storage.available_bytes)}
          </KeyValue>
          <KeyValue label={t("remoteModels.revision")}>{selectedDetails.revision || "main"}</KeyValue>
        </dl>
        {selectedDetails.gated && (
          <p className="flex items-start gap-2 rounded-lg border border-amber/30 bg-amber/10 px-3 py-2.5 text-xs leading-relaxed text-muted">
            <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-amber" />
            {t("remoteModels.gatedDetail")}
          </p>
        )}
        {insufficient && (
          <p className="rounded-lg border border-rose/30 bg-rose/10 px-3 py-2.5 text-xs text-muted">
            {t("remoteModels.insufficientSpace")}
          </p>
        )}
        {!details.storage.destination_available && (
          <p className="rounded-lg border border-rose/30 bg-rose/10 px-3 py-2.5 text-xs text-muted">
            {t("remoteModels.destinationUnavailable")}
          </p>
        )}
      </div>
    </Modal>
  );
}
