import { HardDriveDownload, RefreshCw } from "lucide-react";
import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useI18n } from "../i18n/provider";
import { rescanModels, startRemoteModelDownload } from "../lib/api";
import { isModelDownloadActive, percentOfDownload } from "../lib/modelProvider";
import { keys, useMachineDownloads } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { WholeModelEntry } from "../lib/wholeModels";
import { Button, Progress } from "./ui";

/**
 * What a machine card says when the machine lacks the selected model: its
 * download running there, or **Download here** (the same repository and file
 * a holder downloaded) and **Rescan** for a file placed by hand. The card
 * cannot take a range until the model is there.
 */
export function ModelPresence({ nodeId, machine, model }: { nodeId: string; machine: string; model: WholeModelEntry }) {
  const { t } = useI18n();
  const toast = useToast();
  const queryClient = useQueryClient();
  const downloads = useMachineDownloads(nodeId);
  const [busy, setBusy] = useState<"download" | "rescan" | null>(null);
  const source = model.source;
  const running = (downloads.data ?? []).find(
    (job) => isModelDownloadActive(job) && source != null && job.repo_id === source.repo_id && job.filename === source.filename,
  );

  async function download() {
    if (!source) return;
    setBusy("download");
    try {
      await startRemoteModelDownload({
        repoId: source.repo_id,
        revision: source.revision ?? undefined,
        filename: source.filename,
        nodeId,
        extraFiles: source.extra_files,
      });
      toast.success(t("remoteModels.downloadStarted"), t("remoteModels.downloadStartedOn", { name: source.filename, machine }));
      void downloads.refetch();
    } catch (error) {
      toast.fromError(t("remoteModels.downloadFailed"), error);
    } finally {
      setBusy(null);
    }
  }

  async function rescan() {
    setBusy("rescan");
    try {
      await rescanModels(nodeId);
      toast.info(t("paths.rescanStarted"));
      void queryClient.invalidateQueries({ queryKey: keys.clusterModels });
    } catch (error) {
      toast.fromError(t("paths.rescanFailed"), error);
    } finally {
      setBusy(null);
    }
  }

  const stop = { onClick: (event: React.MouseEvent) => event.stopPropagation(), onPointerDown: (event: React.PointerEvent) => event.stopPropagation(), onPointerUp: (event: React.PointerEvent) => event.stopPropagation() };

  if (running) {
    const percent = percentOfDownload(running) ?? running.progress ?? null;
    return (
      <div className="mt-3 rounded-lg border border-iris/30 bg-iris/10 px-2 py-1.5" data-testid="model-presence-downloading">
        <p className="text-[10px] text-iris">{t("deployStudio.downloadingHere", { pct: percent == null ? "-" : Math.round(percent) })}</p>
        <Progress value={percent} tone="iris" className="mt-1" />
      </div>
    );
  }
  return (
    <div className="mt-3 rounded-lg border border-amber/30 bg-amber/10 px-2 py-1.5" data-testid="model-presence-missing" {...stop}>
      <p className="text-[10px] leading-snug text-amber">
        {source ? t("deployStudio.lacksModel") : t("deployStudio.lacksModelPlace")}
      </p>
      <div className="mt-1.5 flex flex-wrap gap-1">
        {source && (
          <Button size="sm" variant="outline" loading={busy === "download"} icon={<HardDriveDownload className="h-3 w-3" />} onClick={() => void download()}>
            {t("deployStudio.downloadHere")}
          </Button>
        )}
        <Button size="sm" variant="ghost" loading={busy === "rescan"} icon={<RefreshCw className="h-3 w-3" />} onClick={() => void rescan()}>
          {t("deployStudio.rescan")}
        </Button>
      </div>
    </div>
  );
}
