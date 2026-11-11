/**
 * Where this machine keeps models, slices, and the shared project workspace.
 *
 * The distinction matters: the whole-model list is many folders and is only
 * read; the slice folder is one folder, is written to, and is the **only**
 * place a peer is allowed to pull a file from. Saying so on the screen keeps
 * the second field from looking like one more entry in the first list.
 */
import { AnimatePresence, motion } from "framer-motion";
import { FolderTree, Lock, Plus, RefreshCw, RotateCcw, Save, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import {
  Badge,
  Button,
  ErrorState,
  Field,
  Footnote,
  Input,
  PageHeader,
  Panel,
  PanelHeader,
  Skeleton,
} from "../components/ui";
import { api, rescanModels } from "../lib/api";
import { useInvalidate, usePaths } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { PathsResponse } from "../lib/types";
import { useI18n } from "../i18n/provider";

const KIND: Record<string, { label: string; tone: "sky" | "amber" | "neutral" }> = {
  lmstudio: { label: "LM Studio", tone: "sky" },
  ollama: { label: "Ollama", tone: "amber" },
  folder: { label: "Folder", tone: "neutral" },
};

export function ModelPaths() {
  const { t } = useI18n();
  const paths = usePaths();
  const toast = useToast();
  const invalidate = useInvalidate();

  const [dirs, setDirs] = useState<string[]>([]);
  const [shardDir, setShardDir] = useState("");
  const [workspaceRoot, setWorkspaceRoot] = useState("");
  const [shardMaxGb, setShardMaxGb] = useState("100");
  const [saving, setSaving] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [rescanning, setRescanning] = useState(false);

  // A file copied into a folder by hand shows up without waiting for the
  // catalog's next walk.
  async function rescan() {
    setRescanning(true);
    try {
      await rescanModels();
      toast.info(t("paths.rescanStarted"));
      invalidate();
    } catch (e) {
      toast.fromError(t("paths.rescanFailed"), e);
    } finally {
      setRescanning(false);
    }
  }

  // Load once: re-syncing on every poll would fight the operator's typing.
  useEffect(() => {
    if (loaded || !paths.data) return;
    setDirs(paths.data.model_dirs.map((d) => d.path));
    setShardDir(paths.data.shard_dir.path);
    setWorkspaceRoot(paths.data.workspace_root.path);
    setShardMaxGb(String(paths.data.shard_max_gb || 100));
    setLoaded(true);
  }, [paths.data, loaded]);

  async function save() {
    setSaving(true);
    try {
      const saved = await api.post<PathsResponse>("/api/ui/paths", {
        model_dirs: dirs.map((d) => d.trim()).filter(Boolean),
        shard_dir: shardDir.trim(),
        workspace_root: workspaceRoot.trim(),
        shard_max_gb: Number.parseFloat(shardMaxGb.replace(",", ".")),
      });
      setDirs(saved.model_dirs.map((d) => d.path));
      setShardDir(saved.shard_dir.path);
      setWorkspaceRoot(saved.workspace_root.path);
      toast.success(t("paths.saved"), t("paths.savedDetail"));
      invalidate();
    } catch (e) {
      toast.fromError(t("paths.saveFailed"), e);
    } finally {
      setSaving(false);
    }
  }

  const detected = paths.data?.detected ?? [];
  const notYetAdded = detected.filter((d) => !dirs.includes(d.path));

  return (
    <>
      <PageHeader
        title={t("paths.title")}
        subtitle={t("paths.subtitle")}
        actions={
          <>
            <Button
              variant="ghost"
              icon={<RefreshCw className="h-3.5 w-3.5" />}
              loading={rescanning}
              onClick={() => void rescan()}
            >
              {t("paths.rescan")}
            </Button>
            <Button
              variant="primary"
              icon={<Save className="h-3.5 w-3.5" />}
              loading={saving}
              onClick={() => void save()}
              disabled={!loaded}
            >
              {t("paths.save")}
            </Button>
          </>
        }
      />

      {paths.isError && <ErrorState error={paths.error} onRetry={() => void paths.refetch()} />}
      {paths.isLoading && <Skeleton className="h-64 w-full" />}

      {paths.data && (
        <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]">
          <div className="space-y-5">
            <Panel>
              <PanelHeader
                title={t("paths.workspaceFolder")}
                subtitle={t("paths.workspaceDetail")}
              />
              <div className="space-y-3 px-5 py-4">
                <Field
                  label={t("paths.path")}
                  hint={t("paths.workspaceHint", { path: paths.data.default_workspace_root })}
                >
                  <Input
                    value={workspaceRoot}
                    spellCheck={false}
                    onChange={(e) => setWorkspaceRoot(e.target.value)}
                    placeholder={paths.data.default_workspace_root}
                  />
                </Field>
                <Footnote icon={<FolderTree className="h-3.5 w-3.5" />}>
                  {t("paths.workspaceLayout")}
                </Footnote>
              </div>
            </Panel>

            <Panel>
              <PanelHeader
                title={t("paths.fullModels")}
                subtitle={t("paths.fullDetail")}
                actions={
                  paths.data.model_dirs_env ? (
                    <Badge tone="neutral">{t("paths.fromEnvBadge", { name: paths.data.model_dirs_env })}</Badge>
                  ) : paths.data.using_defaults ? (
                    <Badge tone="amber">{t("paths.autoDetect")}</Badge>
                  ) : (
                    <Badge tone="neutral">{t("paths.customList")}</Badge>
                  )
                }
              />
              <div className="space-y-2 px-5 py-4">
                <AnimatePresence initial={false}>
                  {dirs.map((d, i) => (
                    <motion.div
                      key={i}
                      layout
                      initial={{ opacity: 0, height: 0 }}
                      animate={{ opacity: 1, height: "auto" }}
                      exit={{ opacity: 0, height: 0 }}
                      transition={{ duration: 0.16 }}
                      className="flex items-center gap-2 overflow-hidden"
                    >
                      <Input
                        value={d}
                        spellCheck={false}
                        disabled={Boolean(paths.data?.model_dirs_env)}
                        aria-label={t("paths.folderAria", { count: i + 1 })}
                        onChange={(e) =>
                          setDirs((l) => l.map((x, j) => (j === i ? e.target.value : x)))
                        }
                      />
                      <button
                        disabled={Boolean(paths.data?.model_dirs_env)}
                        onClick={() => setDirs((l) => l.filter((_, j) => j !== i))}
                        aria-label={t("paths.removeFolderAria", { path: d })}
                        className="shrink-0 rounded p-1.5 text-faint transition-colors hover:text-rose"
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </button>
                    </motion.div>
                  ))}
                </AnimatePresence>
                <Button
                  size="sm"
                  variant="ghost"
                  icon={<Plus className="h-3.5 w-3.5" />}
                  disabled={Boolean(paths.data.model_dirs_env)}
                  onClick={() => setDirs((l) => [...l, ""])}
                >
                  {t("paths.addFolder")}
                </Button>
                {paths.data.model_dirs_env && (
                  <Footnote icon={<Lock className="h-3.5 w-3.5" />}>
                    {t("paths.fromEnvNote", { name: paths.data.model_dirs_env })}
                  </Footnote>
                )}
                {dirs.length === 0 && (
                  <p className="pt-1 text-xs leading-relaxed text-amber">
                    {t("paths.emptyList")}
                  </p>
                )}
              </div>
            </Panel>

          </div>

          <Panel className="self-start">
            <PanelHeader
              title={t("paths.detected")}
              subtitle={t("paths.detectedSubtitle")}
            />
            <div className="space-y-2 px-5 py-4">
              {detected.length === 0 && (
                <p className="text-[0.82rem] text-faint">
                  {t("paths.noneDetected")}
                </p>
              )}
              {detected.map((d) => (
                <div key={d.path} className="min-w-0">
                  <div className="flex items-center gap-2">
                    <Badge tone={KIND[d.kind]?.tone ?? "neutral"}>
                      {d.kind === "folder" ? t("paths.folder") : (KIND[d.kind]?.label ?? d.kind)}
                    </Badge>
                    {!d.exists && <Badge tone="rose">{t("paths.missing")}</Badge>}
                  </div>
                  <p className="break-path mt-1 font-mono text-[0.72rem] text-muted">
                    {d.path}
                  </p>
                </div>
              ))}
              {notYetAdded.length > 0 && (
                <Button
                  size="sm"
                  variant="ghost"
                  className="mt-2"
                  icon={<RotateCcw className="h-3.5 w-3.5" />}
                  onClick={() =>
                    setDirs((l) => [...l, ...notYetAdded.map((d) => d.path)])
                  }
                >
                  {t("paths.addMissing", { count: notYetAdded.length })}
                </Button>
              )}
            </div>
            <Footnote
              className="border-t border-line px-5 py-3"
              icon={<FolderTree className="h-3.5 w-3.5" />}
            >
              {t("paths.validation")}
            </Footnote>
          </Panel>
        </div>
      )}
    </>
  );
}
