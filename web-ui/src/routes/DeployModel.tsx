/**
 * Putting a set of slices that already exists into service.
 *
 * The screen is a preview of a decision, not a form: the operator picks the
 * model, and the node answers with who would serve which range and what has to
 * be copied first. Nothing here computes that in the browser — the plan comes
 * from `/api/ui/deploy/plan`, and pressing the button recomputes it server-side
 * rather than submitting the preview, because a machine can leave the cluster
 * between looking and deciding.
 *
 * The copies matter enough to be the loudest thing on screen. A 10 GB slice
 * takes about seventeen minutes on the real link, so "3 fatias, 12 GB" is the
 * difference between a deploy that finishes now and one that finishes after
 * lunch, and the operator should learn that before pressing, not after.
 */
import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { AnimatePresence, motion } from "framer-motion";
import {
  ArrowRight,
  CircleAlert,
  Download,
  Layers,
  Rocket,
  ServerCog,
  TriangleAlert,
} from "lucide-react";
import { api } from "../lib/api";
import { useClusterCatalog, useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import { bytes, layerRange } from "../lib/format";
import {
  Badge,
  Button,
  Empty,
  ErrorState,
  Footnote,
  PageHeader,
  Panel,
  PanelHeader,
  ScrollX,
  Select,
  Skeleton,
} from "../components/ui";
import type { DeployPlan } from "../lib/types";

export function DeployModel() {
  const cluster = useClusterCatalog();
  const [group, setGroup] = useState("");
  const [running, setRunning] = useState(false);
  const toast = useToast();
  const invalidate = useInvalidate();

  /** Every slice set anyone in the cluster holds, named once. */
  const groups = useMemo(() => {
    const seen = new Set<string>();
    for (const peer of cluster.data?.peers ?? []) {
      for (const set of peer.shard_sets ?? []) seen.add(set.group);
    }
    return [...seen].sort();
  }, [cluster.data]);

  const chosen = group || groups[0] || "";

  const plan = useQuery({
    queryKey: ["deploy-plan", chosen],
    queryFn: () => api.post<DeployPlan>("/api/ui/deploy/plan", { group: chosen }),
    enabled: chosen.length > 0,
    staleTime: 10_000,
  });

  async function deploy() {
    setRunning(true);
    try {
      await api.post<{ job_id: string }>("/api/ui/deploy", { group: chosen });
      toast.success(
        "Implantação começou",
        "Acompanhe no sino: o progresso vem dos nós, então recarregar não perde nada.",
      );
      invalidate();
    } catch (e) {
      toast.fromError("A implantação não começou", e);
    } finally {
      setRunning(false);
    }
  }

  const p = plan.data;
  const copies = p?.steps.filter((s) => !s.ready && s.target_node_id) ?? [];
  const canDeploy = !!p && !p.blocked && p.steps.length > 0;

  return (
    <>
      <PageHeader
        title="Implantar um modelo fatiado"
        subtitle="Pegue um conjunto de fatias que já existe e coloque-o para atender na rede."
      />

      {cluster.isLoading && <Skeleton className="h-40 w-full" />}
      {cluster.isError && (
        <ErrorState error={cluster.error} onRetry={() => void cluster.refetch()} />
      )}

      {cluster.data && groups.length === 0 && (
        <Panel>
          <Empty icon={<Layers className="h-5 w-5" />} title="Nenhum conjunto de fatias na rede">
            Nenhuma máquina do cluster tem fatias. Fatie um modelo em Modelos ›
            Fatiar, ou receba as fatias de outra máquina.
          </Empty>
        </Panel>
      )}

      {groups.length > 0 && (
        <div className="space-y-5">
          <Panel>
            <PanelHeader
              title="Modelo a implantar"
              subtitle="Conjuntos de fatias que alguma máquina do cluster já tem."
            />
            <div className="px-5 py-4">
              <Select
                aria-label="Conjunto de fatias"
                value={chosen}
                onChange={(e) => setGroup(e.target.value)}
                className="max-w-md"
              >
                {groups.map((g) => (
                  <option key={g} value={g}>
                    {g}
                  </option>
                ))}
              </Select>
            </div>
          </Panel>

          {plan.isLoading && <Skeleton className="h-52 w-full" />}
          {plan.isError && (
            <ErrorState error={plan.error} onRetry={() => void plan.refetch()} />
          )}

          <AnimatePresence mode="wait">
            {p && (
              <motion.div
                key={p.group}
                initial={{ opacity: 0, y: 8 }}
                animate={{ opacity: 1, y: 0 }}
                exit={{ opacity: 0, y: -6 }}
                transition={{ duration: 0.22, ease: [0.22, 1, 0.36, 1] }}
                className="space-y-5"
              >
                {p.blocked && (
                  <div className="flex items-start gap-3 rounded-xl border border-rose/30 bg-rose/[0.07] px-4 py-3.5">
                    <CircleAlert className="mt-0.5 h-4 w-4 shrink-0 text-rose" />
                    <div className="min-w-0">
                      <p className="text-sm font-medium text-fg">
                        Não dá para implantar deste jeito
                      </p>
                      <p className="mt-1 text-sm text-muted">{p.blocked}</p>
                    </div>
                  </div>
                )}

                {!p.blocked && !p.complete && (
                  <div className="flex items-start gap-3 rounded-xl border border-amber/30 bg-amber/[0.07] px-4 py-3.5">
                    <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
                    <div className="min-w-0">
                      <p className="text-sm font-medium text-fg">
                        As fatias não cobrem o modelo inteiro
                      </p>
                      <p className="mt-1 text-sm text-muted">
                        Dá para subir assim, mas o planejador recusa pipeline com
                        buraco — a rede não vai atender até a cobertura fechar.
                      </p>
                    </div>
                  </div>
                )}

                <Panel>
                  <PanelHeader
                    eyebrow="Prévia"
                    title="Como as fatias ficam distribuídas"
                    subtitle="Da primeira camada à última. Quem fica com a camada 0 processa o prompt inteiro de cada requisição, então essa vai para a máquina com mais espaço."
                    actions={
                      <Button
                        variant="primary"
                        icon={<Rocket className="h-4 w-4" />}
                        loading={running}
                        disabled={!canDeploy}
                        onClick={() => void deploy()}
                      >
                        Implantar
                      </Button>
                    }
                  />

                  <ScrollX>
                    <table className="w-full min-w-[46rem] text-sm">
                      <thead>
                        <tr className="border-b border-line text-left">
                          {["Camadas", "Fatia", "Tamanho", "Vai para", "O que acontece"].map(
                            (h) => (
                              <th key={h} className="px-5 py-2.5 text-xs font-medium text-faint">
                                {h}
                              </th>
                            ),
                          )}
                        </tr>
                      </thead>
                      <tbody className="divide-y divide-line">
                        {p.steps.map((s) => (
                          <tr key={`${s.start_layer}-${s.end_layer}`}>
                            <td className="whitespace-nowrap px-5 py-3 font-mono text-[0.82rem] text-iris">
                              {layerRange(s.start_layer, s.end_layer)}
                            </td>
                            <td className="max-w-[16rem] px-5 py-3">
                              <span className="block truncate text-[0.82rem] text-fg">
                                {s.name}
                              </span>
                            </td>
                            <td className="whitespace-nowrap px-5 py-3 font-mono text-[0.8rem] text-muted">
                              {bytes(s.size_bytes)}
                            </td>
                            <td className="whitespace-nowrap px-5 py-3">
                              {s.target_node_id ? (
                                <span className="flex items-center gap-1.5 text-[0.82rem] text-fg">
                                  <ServerCog className="h-3.5 w-3.5 text-faint" />
                                  {s.target_host}
                                  {s.start_layer === 0 && (
                                    <Badge tone="iris" className="ml-1">
                                      processa o prompt
                                    </Badge>
                                  )}
                                </span>
                              ) : (
                                <span className="text-[0.8rem] text-rose">
                                  sem máquina livre
                                </span>
                              )}
                            </td>
                            <td className="whitespace-nowrap px-5 py-3">
                              {!s.target_node_id ? (
                                <span className="text-[0.8rem] text-faint">—</span>
                              ) : s.ready ? (
                                <span className="text-[0.8rem] text-muted">já está lá</span>
                              ) : (
                                <span className="flex items-center gap-1.5 text-[0.8rem] text-amber">
                                  <Download className="h-3.5 w-3.5" />
                                  copiar de {s.source_host ?? "—"}
                                </span>
                              )}
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </ScrollX>

                  <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line px-5 py-3">
                    <span className="text-[0.82rem] text-muted">
                      {p.steps.length} {p.steps.length === 1 ? "fatia" : "fatias"} ·{" "}
                      {p.total_layers} camadas
                    </span>
                    {copies.length === 0 ? (
                      <Badge tone="aqua">nada a copiar</Badge>
                    ) : (
                      <Badge tone="amber">
                        {copies.length} {copies.length === 1 ? "cópia" : "cópias"} ·{" "}
                        {bytes(p.transfer_bytes)} pela rede
                      </Badge>
                    )}
                  </div>
                </Panel>

                <Footnote icon={<ArrowRight className="h-3.5 w-3.5" />}>
                  As cópias usam o mesmo transporte da tela de Transferências, com
                  retomada e conferência de MD5. O progresso fica no sino e vem dos
                  nós, então fechar esta aba não interrompe nada.
                </Footnote>
              </motion.div>
            )}
          </AnimatePresence>
        </div>
      )}
    </>
  );
}
