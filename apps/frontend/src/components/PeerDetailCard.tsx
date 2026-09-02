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
import { GpuTemperature } from "./GpuTemperature";
import { MachineIcon, gpuLabel, machineKind } from "./MachineIcon";
import { trackOf } from "./PipelineRail";
import { ReleaseSlice } from "./ReleaseSlice";
import { RemoveNode } from "./RemoveNode";
import { LeaveNode } from "./LeaveNode";
import { ModelPlacementSection } from "./ModelPlacement";
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
import { useInvalidate, usePeerRuntime } from "../lib/queries";
import { useToast } from "../lib/toast";
import type { PeerLoadParams, PeerView } from "../lib/types";
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
 * A seção de colocação, com a busca que a alimenta.
 *
 * Separada do card porque tem um ciclo de vida próprio: é a única parte que faz
 * uma chamada extra, e ela só acontece com a seção montada. Enquanto a resposta
 * não chega, a seção não existe - um esqueleto piscando num card que já está
 * cheio de números seria ruído, e a informação aqui nunca é urgente.
 */
function PeerPlacement({
  nodeId,
  enabled,
  kqvRequested,
  mtpRequested,
}: {
  nodeId: string;
  enabled: boolean;
  kqvRequested: boolean;
  mtpRequested: boolean;
}) {
  const { t } = useI18n();
  const { data } = usePeerRuntime(nodeId, enabled);
  const health = data?.health;
  if (!data || !health) return null;
  return (
    <>
      {mtpRequested && (
        <section
          role="status"
          aria-label={t("dashboard.mtpStatus")}
          className="border-t border-line px-5 py-3"
        >
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span className="eyebrow">{t("dashboard.mtpStatus")}</span>
            <Badge
              tone={health.mtp_ready ? "aqua" : "rose"}
              className="px-1.5 py-0 text-[0.68rem]"
            >
              {health.mtp_ready
                ? health.mtp_allocation === "ram"
                  ? t("dashboard.mtpReadyRam")
                  : health.mtp_allocation === "vram"
                    ? t("dashboard.mtpReadyVram")
                    : t("dashboard.mtpReadyUnknown")
                : t("dashboard.mtpUnavailable")}
            </Badge>
          </div>
          {!health.mtp_ready && (
            <>
              <p className="mt-1.5 text-xs leading-relaxed text-rose">
                {t("dashboard.mtpUnavailableDetail")}
              </p>
              <p className="mt-1 break-words text-xs leading-relaxed text-muted">
                {health.mtp_unavailable_reason ?? t("dashboard.mtpReasonUnknown")}
              </p>
            </>
          )}
        </section>
      )}
      {health.load_params && <LoadParamsSection params={health.load_params} />}
      <ModelPlacementSection
        placement={health.placement ?? null}
        contextMemory={health.context_memory?.buffers ?? null}
        offloadKqv={health.offload_kqv ?? kqvRequested}
        fitApplied={health.fit_memory_applied ?? false}
      />
    </>
  );
}

/**
 * Com que parâmetros de llama.cpp o contexto deste nó subiu.
 *
 * Existe para o operador que quer comparar a máquina com uma linha de comando
 * do `llama-cli` sem abrir o `/health` na mão. Todo valor vem do contexto
 * vivo do nó; nada aqui é derivado da configuração pedida, porque o fallback
 * de KV em RAM sobe um contexto diferente do que foi pedido.
 */
function LoadParamsSection({ params }: { params: PeerLoadParams }) {
  const { t } = useI18n();
  // O Flash Attention é o único campo que pode faltar: é o único que depende
  // da ponte de diagnóstico. `null` vira "não sei", nunca "desligado".
  const fa =
    params.flash_attn === null
      ? t("dashboard.paramsUnknown")
      : params.flash_attn
        ? params.flash_attn_auto
          ? t("dashboard.faOnAuto")
          : t("dashboard.faOn")
        : params.flash_attn_auto
          ? t("dashboard.faOffAuto")
          : t("dashboard.faOff");
  return (
    <section aria-label={t("dashboard.loadParams")} className="border-t border-line px-5 py-4">
      <div className="eyebrow mb-3">{t("dashboard.loadParams")}</div>
      <dl className="grid gap-x-6 gap-y-4 sm:grid-cols-2">
        <KeyValue label={t("dashboard.paramFlashAttn")}>
          <Badge
            tone={params.flash_attn === null ? "neutral" : params.flash_attn ? "aqua" : "amber"}
            className="px-1.5 py-0 text-[0.68rem]"
          >
            {fa}
          </Badge>
        </KeyValue>
        <KeyValue label={t("dashboard.paramBatch")}>
          <span className="font-mono">
            {num(params.n_batch)} / {num(params.n_ubatch)}
          </span>
        </KeyValue>
        <KeyValue label={t("dashboard.paramCtx")}>
          <span className="font-mono">
            {num(params.n_ctx)} / {num(params.n_ctx_seq)}
          </span>
        </KeyValue>
        <KeyValue label={t("dashboard.paramSeqMax")}>
          <span className="font-mono">
            {num(params.n_seq_max)}
            {params.kv_unified ? " · " + t("dashboard.paramKvUnified") : ""}
          </span>
        </KeyValue>
        <KeyValue label={t("dashboard.paramThreads")}>
          <span className="font-mono">
            {num(params.n_threads)} / {num(params.n_threads_batch)}
          </span>
        </KeyValue>
        <KeyValue label={t("dashboard.paramGpuLayers")}>
          <span className="font-mono">{num(params.n_gpu_layers)}</span>
        </KeyValue>
        <KeyValue label={t("dashboard.paramCacheType")}>
          <span className="font-mono">
            {params.cache_type_k} / {params.cache_type_v}
          </span>
        </KeyValue>
        <KeyValue label={t("dashboard.paramKqv")}>
          {t(params.offload_kqv ? "dashboard.paramOnGpu" : "dashboard.paramOnCpu")}
        </KeyValue>
      </dl>
      {params.fit_memory_applied && (
        <p className="mt-3 text-xs leading-relaxed text-muted">
          {t("dashboard.paramsFitNote")}
        </p>
      )}
    </section>
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
  const hasGpu = system != null && system.gpu_kind !== "none" && system.gpus.length > 0;
  // Só as que reportam VRAM: memória unificada cai no ramo `unified` abaixo,
  // que mede outra coisa (wired), e uma barra sem valor viraria animação de
  // "carregando" para sempre.
  const vramGpus = (system?.gpus ?? []).filter((g) => g.vram_used_pct != null);
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
        <div className="space-y-3 px-5 py-5 text-sm text-muted">
          {t("dashboard.unreachable", { error: peer.unreachable })}
          <GpuTemperature
            gpuKind={system?.gpu_kind ?? "none"}
            temperature={null}
            policy={n?.thermal}
          />
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
            {vramGpus.length > 0 ? (
              // Uma barra por placa: numa máquina com Radeon e NVIDIA juntas,
              // mostrar só `gpus[0]` escondia metade do hardware de quem decide
              // onde implantar.
              vramGpus.map((g) => (
                <Meter
                  key={g.index}
                  label={t("dashboard.vram", { name: gpuLabel(g) })}
                  value={g.vram_used_pct ?? null}
                  tone={(g.vram_used_pct ?? 0) > 88 ? "rose" : "lime"}
                  detail={t("dashboard.usedOf", {
                    used: megabytes(g.vram_used_mb),
                    total: megabytes(g.vram_total_mb),
                  })}
                />
              ))
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
            {hasGpu && <div className="min-w-0 sm:col-span-2">
              <div className="eyebrow mb-2">{t("dashboard.gpuTemperature")}</div>
              <div className="grid gap-2 sm:grid-cols-2">
                {system?.gpus.map((g) => (
                  <GpuTemperature
                    key={g.index}
                    gpuKind={system?.gpu_kind ?? "none"}
                    temperature={g.temperature_c}
                    policy={n?.thermal}
                    label={gpuLabel(g)}
                  />
                ))}
              </div>
            </div>}
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

          {/* Só faz pergunta quem tem a quem perguntar: sem modelo carregado
              não há colocação nenhuma a mostrar, e o `enabled` evita um
              `/health` por card a cada ciclo do Painel. */}
          {serving && (
            <PeerPlacement
              nodeId={peer.node_id}
              enabled={serving}
              kqvRequested={cfg?.offload_kqv ?? true}
              mtpRequested={cfg?.mtp_enabled ?? false}
            />
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
