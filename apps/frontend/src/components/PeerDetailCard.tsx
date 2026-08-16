/**
 * Tudo o que uma máquina reporta, no mesmo desenho para todas elas.
 *
 * Vive aqui, e não dentro do Painel, porque é mostrado em dois lugares: a aba
 * Detalhes empilha um card por máquina da rede, e o duplo clique num card do
 * Fluxo abre este mesmo card numa janela. Um componente só é o que garante que
 * as duas telas não divirjam depois.
 *
 * Máquina sem fatia implantada não é um caso degenerado da máquina com fatia:
 * ela não tem faixa de camadas, não tem papel no pipeline e não tem arquivo. O
 * card mostra o que ela de fato reporta - host, estado, memória, disco, carga -
 * e diz que não há fatia, em vez de escrever "0-0" ou uma barra de camadas
 * vazia que se lê como cobertura.
 */
import clsx from "clsx";
import { motion } from "framer-motion";
import { RefreshCw } from "lucide-react";
import { useState } from "react";
import { Confirm, Modal } from "./Confirm";
import { MachineIcon, machineKind } from "./MachineIcon";
import { trackOf } from "./PipelineRail";
import { ReleaseSlice } from "./ReleaseSlice";
import { RemoveNode } from "./RemoveNode";
import { LeaveNode } from "./LeaveNode";
import { Badge, Button, Dot, Footnote, KeyValue, Meter } from "./ui";
import { devUpdate } from "../lib/api";
import {
  isServing,
  layerRange,
  megabytes,
  num,
  serverStateLabel,
  serverStateTone,
} from "../lib/format";
import { hasSlice } from "../lib/networkPeers";
import { useInvalidate } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { PeerView } from "../lib/types";
import { useI18n } from "../i18n/provider";

/** O botão de atualizar e reiniciar, só visível quando o peer aceita o pedido. */
function UpdateNode({
  nodeId,
  hostname,
  isSelf,
}: {
  nodeId: string;
  hostname: string;
  isSelf: boolean;
}) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const toast = useToast();
  const invalidate = useInvalidate();

  async function run() {
    setBusy(true);
    try {
      await devUpdate(isSelf ? undefined : nodeId);
      toast.success(t("dashboard.updateNode", { hostname }), t("dashboard.updateDetail"));
      invalidate();
    } catch (e) {
      toast.fromError(t("dashboard.updateFailed", { hostname }), e);
    } finally {
      setBusy(false);
      setOpen(false);
    }
  }

  return (
    <>
      <Button size="sm" variant="outline" onClick={() => setOpen(true)}>
        <RefreshCw className="h-3.5 w-3.5" />
        {t("dashboard.updateRestart")}
      </Button>
      <Confirm
        open={open}
        busy={busy}
        tone="primary"
        title={t("dashboard.updateTitle")}
        confirmLabel={t("dashboard.updateRestart")}
        onCancel={() => setOpen(false)}
        onConfirm={() => void run()}
        description={
          <>
            <p>{t("dashboard.updateDescription")}</p>
            <p className="mt-2 text-xs text-faint">{t("dashboard.updateWarning")}</p>
          </>
        }
      />
    </>
  );
}

/**
 * A faixa que este card pode mostrar, ou `null` quando não existe nenhuma.
 *
 * O anúncio do peer é a fonte; a config local entra só quando o peer ainda não
 * anunciou a fatia que a máquina já carregou. Nunca se inventa faixa.
 */
export function cardRange(peer: PeerView): [number, number] | null {
  if (hasSlice(peer)) return [peer.start_layer, peer.end_layer];
  const cfg = peer.node?.config;
  if (cfg && cfg.total_layers > 0) return [cfg.start_layer, cfg.end_layer];
  return null;
}

export function PeerDetailCard({
  peer,
  index,
  group,
  otherRanges,
}: {
  peer: PeerView;
  index: number;
  /** Modelo que o cluster está servindo, que é como o nó agrupa as fatias. */
  group: string;
  /** Faixas dos outros peers deste modelo, para saber se este é o último. */
  otherRanges: [number, number][];
}) {
  const { t } = useI18n();
  const track = trackOf(index);
  const n = peer.node;
  const system = n?.system;
  const kind = machineKind(system);
  const cfg = n?.config;
  const serving = isServing(n?.status.state);
  const gpu = system?.gpus[0];
  const range = cardRange(peer);

  return (
    <motion.div
      layout
      initial={{ opacity: 0, y: 10 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.26, delay: Math.min(index, 6) * 0.04 }}
      className="panel relative overflow-hidden"
    >
      <span
        className={clsx(
          "absolute inset-x-0 top-0 h-[2px]",
          range ? track.fill : "bg-line-strong",
        )}
      />

      <div className="flex flex-wrap items-start justify-between gap-3 border-b border-line px-5 py-4">
        <div className="flex min-w-0 items-start gap-3">
          <div className="mt-0.5 flex h-9 w-9 shrink-0 items-center justify-center rounded-lg border border-line bg-raised">
            <MachineIcon system={system} />
          </div>
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <h3 className="truncate font-display text-[0.95rem] font-semibold text-fg">
                {n?.hostname || peer.short_id}
              </h3>
              {peer.is_self && (
                <Badge tone="neutral" className="px-1.5 py-0 text-[0.62rem]">
                  {t("dashboard.here")}
                </Badge>
              )}
            </div>
            <p className="truncate text-xs text-muted">{kind.label}</p>
          </div>
        </div>
        <div className="flex shrink-0 flex-col items-end gap-1.5">
          {range ? (
            <span className={clsx("font-mono text-sm font-semibold", track.text)}>
              {layerRange(range[0], range[1])}
            </span>
          ) : (
            <Badge tone="neutral" className="px-1.5 py-0 text-[0.68rem]">
              {t("dashboard.noSlice")}
            </Badge>
          )}
          <span className="flex items-center gap-1.5 text-[0.72rem] text-muted">
            <Dot
              tone={peer.unreachable ? "rose" : serverStateTone(n?.status.state)}
              pulse={serving}
            />
            {peer.unreachable ?? serverStateLabel(n?.status.state)}
          </span>
        </div>
      </div>

      {peer.unreachable ? (
        <div className="px-5 py-5 text-sm text-muted">
          {t("dashboard.unreachable", { error: peer.unreachable })}
        </div>
      ) : (
        <>
          {!range && (
            <p className="border-b border-line bg-raised/50 px-5 py-2.5 text-xs leading-relaxed text-muted">
              {t("dashboard.noSliceDetail")}
            </p>
          )}

          <div className="grid gap-4 px-5 py-4 sm:grid-cols-2">
            <Meter
              label="RAM"
              value={system?.ram.used_pct ?? null}
              tone={
                (system?.ram.used_pct ?? 0) > 88
                  ? "rose"
                  : (system?.ram.used_pct ?? 0) > 70
                    ? "amber"
                    : "iris"
              }
              detail={
                system
                  ? t("dashboard.usedOf", {
                      used: megabytes(system.ram.used_mb),
                      total: megabytes(system.ram.total_mb),
                    })
                  : "—"
              }
            />
            {system?.gpu_kind === "nvidia" && gpu ? (
              <Meter
                label={t("dashboard.vram", { name: gpu.name.replace(/^NVIDIA\s+/i, "") })}
                value={gpu.vram_used_pct ?? null}
                tone={(gpu.vram_used_pct ?? 0) > 88 ? "rose" : "lime"}
                detail={t("dashboard.usedOf", {
                  used: megabytes(gpu.vram_used_mb),
                  total: megabytes(gpu.vram_total_mb),
                })}
              />
            ) : system?.gpu_kind === "unified" ? (
              // Only macOS/Apple Silicon ever reports `wired_mb` — `sysmon.rs`
              // hardcodes it to `None` on every other platform. Gating this on
              // `gpu_kind` (instead of showing it whenever there's no VRAM to
              // show) matters: `value={null}` drives `Progress` into its
              // indeterminate/looping animation, meant for "still loading" —
              // not "will never have a value on this platform." A Linux node
              // (bare metal or in a container) with no NVIDIA GPU fell into
              // this branch and bounced forever, since nothing was ever going
              // to populate `wired_mb` for it.
              <Meter
                label={t("dashboard.wiredMemory")}
                value={
                  system?.wired_mb != null && system.ram.total_mb > 0
                    ? (system.wired_mb / system.ram.total_mb) * 100
                    : null
                }
                tone="sky"
                detail={system?.wired_mb != null ? megabytes(system.wired_mb) : "—"}
              />
            ) : (
              <Meter label={t("dashboard.noGpu")} value={0} tone="sky" detail="—" />
            )}
            <Meter
              label={t("dashboard.disk", { path: system?.disk.mount_point || "—" })}
              value={
                system && system.disk.total_mb > 0
                  ? ((system.disk.total_mb - system.disk.free_mb) / system.disk.total_mb) * 100
                  : null
              }
              tone="amber"
              detail={system ? megabytes(system.disk.free_mb) + " " + t("dashboard.free") : "—"}
            />
            <Meter
              label={t("dashboard.load", { count: system?.cpu_count ?? "—" })}
              value={
                system && system.cpu_count > 0
                  ? Math.min(100, (system.load_avg[0] / system.cpu_count) * 100)
                  : null
              }
              tone="iris"
              detail={system ? system.load_avg.map((l) => num(l, 2)).join(" · ") : "—"}
            />
          </div>

          {system?.gpu_note && (
            <p className="border-t border-line px-5 py-2.5 text-xs leading-relaxed text-faint">
              {system.gpu_note}
            </p>
          )}

          <dl className="grid gap-x-6 gap-y-4 border-t border-line px-5 py-4 sm:grid-cols-2">
            <KeyValue label={t("dashboard.model")}>
              {cfg?.model_label ? (
                <span className="font-mono">{cfg.model_label}</span>
              ) : (
                <span className="text-muted">{t("dashboard.noneLoaded")}</span>
              )}
            </KeyValue>
            <KeyValue label={t("dashboard.role")}>
              {cfg
                ? cfg.single_peer
                  ? t("dashboard.single")
                  : cfg.start_layer === 0
                    ? t("dashboard.prompt")
                    : cfg.end_layer + 1 >= cfg.total_layers
                      ? t("dashboard.tokens")
                      : t("dashboard.middle")
                : (n?.role ?? "—")}
            </KeyValue>
            <KeyValue label={t("dashboard.file")}>
              {cfg?.model_path ? <span className="font-mono text-xs">{cfg.model_path}</span> : "—"}
            </KeyValue>
            <KeyValue label={t("dashboard.context")}>
              {cfg ? (
                <span className="font-mono text-xs">
                  n_ctx {num(cfg.n_ctx)} · n_batch {num(cfg.n_batch)} · {cfg.max_sessions}{" "}
                  {t("dashboard.session", { count: cfg.max_sessions })}
                </span>
              ) : (
                "—"
              )}
            </KeyValue>
            <KeyValue label={t("dashboard.inference")}>
              {peer.inference_url ? (
                <span className="font-mono text-xs">{peer.inference_url}</span>
              ) : (
                t("dashboard.notAdvertised")
              )}
            </KeyValue>
            <KeyValue label={t("dashboard.server")}>
              {n ? (
                <span className="text-xs">
                  {n.status.detail || serverStateLabel(n.status.state)}
                  {n.status.pid ? ` · pid ${n.status.pid}` : ""}
                  {n.status.restarts > 0
                    ? " · " + t("dashboard.restarts", { count: n.status.restarts })
                    : ""}
                </span>
              ) : (
                "—"
              )}
            </KeyValue>
            {/* Logo abaixo do servidor, e não escondido no cabeçalho: a pergunta
                que este campo responde é "os nós estão todos na mesma versão?",
                e ela se responde comparando cards lado a lado. */}
            <KeyValue label={t("dashboard.build")}>
              {n?.build_commit ? <span className="font-mono text-xs">{n.build_commit}</span> : "—"}
            </KeyValue>
            {/* Por onde este nó fala com aquele: com vários endereços
                anunciados, "qual deles ganhou" só se responde aqui. */}
            <KeyValue label={t("dashboard.route")}>
              {peer.route ? (
                <span className="font-mono text-xs">
                  {peer.route.addr} · {Math.round(peer.route.rtt_ms)} ms
                </span>
              ) : (
                <span className="text-muted">{t("dashboard.routeUnmeasured")}</span>
              )}
            </KeyValue>
          </dl>

          {/* Only on this machine's own card: the slice is released by the node
              that owns the memory, never by a coordinator reaching in. */}
          {peer.is_self && cfg && (
            <ReleaseSlice startLayer={cfg.start_layer} endLayer={cfg.end_layer} />
          )}

          {/* Duas saídas diferentes, lado a lado porque são as duas coisas que
              se pode querer fazer com *esta* máquina daqui:

              - "Remover" a tira do modelo e a deixa no cluster, pronta para
                receber outra faixa;
              - "Sair da rede" a tira do cluster inteiro.

              Nos dois casos quem libera a memória é o dono dela - estes botões
              só encaminham. A saída da rede aparece sempre; tirar do modelo, só
              quando há um modelo de onde tirar. */}
          <div className="flex flex-wrap items-center justify-end gap-2 border-t border-line px-5 py-3">
            {serving && group && range && (
              <RemoveNode
                nodeId={peer.node_id}
                group={group}
                range={range}
                existingRanges={otherRanges}
              />
            )}
            <LeaveNode
              nodeId={peer.node_id}
              hostname={n?.hostname || peer.short_id}
              isSelf={peer.is_self}
            />
          </div>

          {n?.dev_update && (
            <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line px-5 py-3">
              <Footnote icon={<RefreshCw className="h-3.5 w-3.5" />} className="min-w-0 flex-1">
                {t("dashboard.devUpdate")}
              </Footnote>
              <UpdateNode
                nodeId={peer.node_id}
                hostname={n.hostname || peer.short_id}
                isSelf={peer.is_self}
              />
            </div>
          )}

          {system && system.gpu_processes.length > 0 && (
            <div className="border-t border-line px-5 py-3">
              <div className="eyebrow mb-2">{t("dashboard.gpuProcesses")}</div>
              <ul className="space-y-1">
                {system.gpu_processes.map((p) => (
                  <li
                    key={p.pid}
                    className="flex items-baseline justify-between gap-3 font-mono text-xs"
                  >
                    <span className="truncate text-muted">
                      {p.name} <span className="text-faint">#{p.pid}</span>
                    </span>
                    <span className="shrink-0 text-fg">{megabytes(p.used_mb)}</span>
                  </li>
                ))}
              </ul>
            </div>
          )}
        </>
      )}
    </motion.div>
  );
}

/**
 * O mesmo card, numa janela - o que o duplo clique no Fluxo abre.
 *
 * Reaproveita a moldura de `Modal`: fecha por Esc, por clique fora e pelo botão,
 * e rola dentro de si quando a máquina reporta mais do que cabe na tela.
 */
export function PeerDetailDialog({
  peer,
  group,
  otherRanges,
  onClose,
}: {
  peer: PeerView | null;
  group: string;
  otherRanges: [number, number][];
  onClose: () => void;
}) {
  const { t } = useI18n();
  return (
    <Modal
      open={peer != null}
      size="lg"
      title={
        peer
          ? t("dashboard.machineDetails", { hostname: peer.node?.hostname || peer.short_id })
          : ""
      }
      onClose={onClose}
      footer={
        <Button variant="ghost" onClick={onClose}>
          {t("common.close")}
        </Button>
      }
    >
      {peer && (
        <div className="text-fg">
          <PeerDetailCard peer={peer} index={0} group={group} otherRanges={otherRanges} />
        </div>
      )}
    </Modal>
  );
}
