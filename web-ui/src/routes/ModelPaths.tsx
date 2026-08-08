/**
 * Where this machine looks for models, and where it writes slices.
 *
 * The distinction matters: the whole-model list is many folders and is only
 * read; the slice folder is one folder, is written to, and is the **only**
 * place a peer is allowed to pull a file from. Saying so on the screen keeps
 * the second field from looking like one more entry in the first list.
 */
import { AnimatePresence, motion } from "framer-motion";
import { FolderTree, HardDriveDownload, Plus, RotateCcw, Save, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import {
  Badge,
  Button,
  ErrorState,
  Field,
  Input,
  PageHeader,
  Panel,
  PanelHeader,
  Skeleton,
} from "../components/ui";
import { api } from "../lib/api";
import { useInvalidate, usePaths } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { PathsResponse } from "../lib/types";

const KIND: Record<string, { label: string; tone: "sky" | "amber" | "neutral" }> = {
  lmstudio: { label: "LM Studio", tone: "sky" },
  ollama: { label: "Ollama", tone: "amber" },
  folder: { label: "Pasta", tone: "neutral" },
};

export function ModelPaths() {
  const paths = usePaths();
  const toast = useToast();
  const invalidate = useInvalidate();

  const [dirs, setDirs] = useState<string[]>([]);
  const [shardDir, setShardDir] = useState("");
  const [saving, setSaving] = useState(false);
  const [loaded, setLoaded] = useState(false);

  // Load once: re-syncing on every poll would fight the operator's typing.
  useEffect(() => {
    if (loaded || !paths.data) return;
    setDirs(paths.data.model_dirs.map((d) => d.path));
    setShardDir(paths.data.shard_dir.path);
    setLoaded(true);
  }, [paths.data, loaded]);

  async function save() {
    setSaving(true);
    try {
      const saved = await api.post<PathsResponse>("/api/ui/paths", {
        model_dirs: dirs.map((d) => d.trim()).filter(Boolean),
        shard_dir: shardDir.trim(),
      });
      setDirs(saved.model_dirs.map((d) => d.path));
      setShardDir(saved.shard_dir.path);
      toast.success("Pastas salvas", "A varredura recomeça em segundo plano.");
      invalidate();
    } catch (e) {
      toast.fromError("As pastas não foram salvas", e);
    } finally {
      setSaving(false);
    }
  }

  const detected = paths.data?.detected ?? [];
  const notYetAdded = detected.filter((d) => !dirs.includes(d.path));

  return (
    <>
      <PageHeader
        title="Pastas de modelos"
        subtitle="Onde procurar modelos inteiros, e onde gravar as fatias."
        actions={
          <Button
            variant="primary"
            icon={<Save className="h-3.5 w-3.5" />}
            loading={saving}
            onClick={() => void save()}
            disabled={!loaded}
          >
            Salvar
          </Button>
        }
      />

      {paths.isError && <ErrorState error={paths.error} onRetry={() => void paths.refetch()} />}
      {paths.isLoading && <Skeleton className="h-64 w-full" />}

      {paths.data && (
        <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]">
          <div className="space-y-5">
            <Panel>
              <PanelHeader
                title="Modelos inteiros"
                subtitle="Só leitura. Cada pasta é lida pelo que ela é: store do Ollama, biblioteca do LM Studio, ou pasta qualquer de .gguf."
                actions={
                  paths.data.using_defaults ? (
                    <Badge tone="amber">detecção automática</Badge>
                  ) : (
                    <Badge tone="neutral">lista sua</Badge>
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
                        aria-label={`Pasta ${i + 1}`}
                        onChange={(e) =>
                          setDirs((l) => l.map((x, j) => (j === i ? e.target.value : x)))
                        }
                      />
                      <button
                        onClick={() => setDirs((l) => l.filter((_, j) => j !== i))}
                        aria-label={`Remover ${d}`}
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
                  onClick={() => setDirs((l) => [...l, ""])}
                >
                  Mais uma pasta
                </Button>
                {dirs.length === 0 && (
                  <p className="pt-1 text-xs leading-relaxed text-amber">
                    Lista vazia significa "não procure em lugar nenhum" — e não "volte a
                    detectar". Salvar assim deixa o catálogo de modelos inteiros vazio.
                  </p>
                )}
              </div>
            </Panel>

            <Panel>
              <PanelHeader
                title="Pasta de fatias"
                subtitle="Uma só. Destino do fatiamento local, dos arquivos recebidos e a única pasta de onde um peer pode puxar um arquivo."
              />
              <div className="space-y-3 px-5 py-4">
                <Field
                  label="Caminho"
                  hint={`Em branco volta para o padrão: ${paths.data.default_shard_dir}`}
                >
                  <Input
                    value={shardDir}
                    spellCheck={false}
                    onChange={(e) => setShardDir(e.target.value)}
                    placeholder={paths.data.default_shard_dir}
                  />
                </Field>
                <p className="flex items-start gap-2 text-xs leading-relaxed text-faint">
                  <HardDriveDownload className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  Os arquivos que chegam de outra máquina vão para a subpasta{" "}
                  <span className="font-mono">recebidos/</span> dentro desta.
                </p>
              </div>
            </Panel>
          </div>

          <Panel className="self-start">
            <PanelHeader
              title="Detectadas nesta máquina"
              subtitle="O que a detecção automática encontraria."
            />
            <div className="space-y-2 px-5 py-4">
              {detected.length === 0 && (
                <p className="text-[0.82rem] text-faint">
                  Nem LM Studio nem Ollama foram encontrados aqui.
                </p>
              )}
              {detected.map((d) => (
                <div key={d.path} className="min-w-0">
                  <div className="flex items-center gap-2">
                    <Badge tone={KIND[d.kind]?.tone ?? "neutral"}>
                      {KIND[d.kind]?.label ?? d.kind}
                    </Badge>
                    {!d.exists && <Badge tone="rose">não existe</Badge>}
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
                  Adicionar {notYetAdded.length === 1 ? "a que falta" : "as que faltam"}
                </Button>
              )}
            </div>
            <p className="flex items-center gap-2 border-t border-line px-5 py-3 text-xs text-faint">
              <FolderTree className="h-3.5 w-3.5 shrink-0" />
              Salvar recusa caminho que não exista ou que não seja uma pasta — um erro de
              digitação aqui só apareceria muito depois, como um peer que não acha o modelo
              que mandaram ele fatiar.
            </p>
          </Panel>
        </div>
      )}
    </>
  );
}
