import { AlertTriangle, HardDriveDownload } from "lucide-react";
import { useEffect, useState } from "react";
import { useI18n } from "../i18n/provider";
import { ApiError, startRemoteModelDownload } from "../lib/api";
import { bytes } from "../lib/format";
import type { RemoteGgufFile, RemoteModelDetails } from "../lib/types";
import { useServingMachines } from "../lib/queries";
import { useToast } from "../lib/toast";
import { Modal } from "./Confirm";
import { Badge, Button, Field, KeyValue, Select } from "./ui";

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
  // "" is this machine; another id makes that machine download with its own
  // token into its own folder.
  const [target, setTarget] = useState("");
  const machines = useServingMachines();
  useEffect(() => {
    if (!open) setTarget("");
  }, [open]);
  if (!details || !file) return null;
  const remote = machines.find((machine) => machine.nodeId === target) ?? null;
  const selectedDetails = details;
  const selectedFile = file;
  const insufficient =
    selectedFile.size_bytes != null &&
    selectedDetails.storage.available_bytes != null &&
    selectedFile.size_bytes > selectedDetails.storage.available_bytes;
  // The space and folder shown are this machine's; another machine checks
  // its own when it starts.
  const blocked = !remote && (insufficient || !selectedDetails.storage.destination_available);

  async function download() {
    setBusy(true);
    try {
      await startRemoteModelDownload({
        repoId: selectedDetails.repo_id,
        revision: selectedDetails.revision ?? undefined,
        filename: selectedFile.filename,
        quantization: selectedFile.quantization ?? undefined,
        nodeId: remote?.nodeId,
      });
      toast.success(
        t("remoteModels.downloadStarted"),
        remote
          ? t("remoteModels.downloadStartedOn", { name: selectedFile.quantization || selectedFile.filename, machine: remote.name })
          : t("remoteModels.downloadStartedDetail", { name: selectedFile.quantization || selectedFile.filename }),
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
                      : code === "node_not_found" || code === "node_unreachable"
                        ? "remoteModels.machineUnreachable"
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
        {machines.length > 0 && (
          <Field label={t("remoteModels.downloadOn")}>
            <Select
              value={target}
              onChange={(event) => setTarget(event.target.value)}
              data-guide="model-download-machine"
            >
              <option value="">{t("remoteModels.thisMachine")}</option>
              {machines.map((machine) => (
                <option key={machine.nodeId} value={machine.nodeId}>{machine.name}</option>
              ))}
            </Select>
          </Field>
        )}
        {remote ? (
          <p className="rounded-lg border border-line bg-raised px-3 py-2.5 text-xs leading-relaxed text-muted">
            {t("remoteModels.remoteChecks", { machine: remote.name })}
          </p>
        ) : null}
        <dl className="grid gap-3 sm:grid-cols-2">
          {!remote && <KeyValue label={t("remoteModels.destination")}>
            {selectedDetails.storage.destination_paths.length > 0 ? (
              <ul className="break-path space-y-1 font-mono text-xs">
                {selectedDetails.storage.destination_paths.map((path) => <li key={path}>{path}</li>)}
              </ul>
            ) : (
              t("remoteModels.destination")
            )}
          </KeyValue>}
          <KeyValue label={t("remoteModels.requiredSpace")}>{bytes(selectedFile.size_bytes)}</KeyValue>
          {!remote && <KeyValue label={t("remoteModels.availableSpace")}>
            {bytes(selectedDetails.storage.available_bytes)}
          </KeyValue>}
          <KeyValue label={t("remoteModels.revision")}>{selectedDetails.revision || "main"}</KeyValue>
        </dl>
        {selectedDetails.gated && (
          <p className="flex items-start gap-2 rounded-lg border border-amber/30 bg-amber/10 px-3 py-2.5 text-xs leading-relaxed text-muted">
            <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-amber" />
            {t("remoteModels.gatedDetail")}
          </p>
        )}
        {!remote && insufficient && (
          <p className="rounded-lg border border-rose/30 bg-rose/10 px-3 py-2.5 text-xs text-muted">
            {t("remoteModels.insufficientSpace")}
          </p>
        )}
        {!remote && !details.storage.destination_available && (
          <p className="rounded-lg border border-rose/30 bg-rose/10 px-3 py-2.5 text-xs text-muted">
            {t("remoteModels.destinationUnavailable")}
          </p>
        )}
      </div>
    </Modal>
  );
}
