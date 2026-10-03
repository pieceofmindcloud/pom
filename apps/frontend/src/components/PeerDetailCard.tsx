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
import { ChevronRight } from "lucide-react";
import { type ReactNode } from "react";
import { Modal } from "./Confirm";
import { GpuTemperature } from "./GpuTemperature";
import { MachineIcon, gpuDisplayName, machineKind } from "./MachineIcon";
import { trackOf } from "./PipelineRail";
import { ReleaseSlice } from "./ReleaseSlice";
import { RemoveNode } from "./RemoveNode";
import { LeaveNode } from "./LeaveNode";
import { isCpuBuffer, ModelPlacementSection } from "./ModelPlacement";
import { SpeedBadges, type PeerSpeed } from "./SpeedBadge";
import { Badge, Button, Dot, InfoTip, KeyValue, Meter } from "./ui";
import {
  isServing,
  layerRange,
  megabytes,
  num,
  serverStateLabel,
  serverStateTone,
} from "../lib/format";
import { hasSlice } from "../lib/networkPeers";
import { usePeerRuntime } from "../lib/queries";
import type { ChatTelemetryPeerPerf, DiskStat, GpuStat, PeerLoadParams, PeerRuntimeHealth, PeerView, ThermalPolicy } from "../lib/types";
import { temperatureBand } from "../lib/thermal";
import { useI18n } from "../i18n/provider";

/**
 * Um bloco que se abre. `<details>` porque a semântica já existe no navegador:
 * abre por clique e por teclado, e o estado é do elemento, não de um `useState`
 * que se perde quando a lista de máquinas se renova sob o card.
 */
function Accordion({
  title,
  children,
  tip,
}: {
  title: string;
  children: ReactNode;
  tip?: string;
}) {
  return (
    <details className="group border-t border-line">
      <summary className="flex cursor-pointer list-none items-center gap-1.5 px-3 py-2 text-muted hover:text-fg [&::-webkit-details-marker]:hidden">
        <ChevronRight
          aria-hidden="true"
          className="h-3 w-3 shrink-0 transition-transform group-open:rotate-90"
        />
        <span className="eyebrow">{title}</span>
        {tip && (
          <span onClick={(event) => event.preventDefault()}>
            <InfoTip label={title} message={tip} />
          </span>
        )}
      </summary>
      <div className="pb-1">{children}</div>
    </details>
  );
}

/**
 * O que este nó reporta sobre o modelo carregado: MTP, parâmetros de carga e
 * colocação das camadas.
 *
 * A saúde chega pronta do card, e não de uma busca própria: o distintivo de
 * MTP subiu para a barra de ferramentas, e duas chamadas para o mesmo
 * `/health` - uma para a barra, outra para esta seção - seriam duas respostas
 * possivelmente diferentes na mesma tela.
 */
function PeerPlacement({
  health,
  kqvRequested,
  mtpRequested,
}: {
  health: PeerRuntimeHealth | undefined;
  kqvRequested: boolean;
  mtpRequested: boolean;
}) {
  const { t } = useI18n();
  if (!health) return null;
  return (
    <>
      <ContextReservationNotice health={health} />
      {mtpRequested && !health.mtp_ready && (
        <p
          role="status"
          className="border-t border-line px-3 py-2 text-[0.7rem] leading-relaxed text-rose"
        >
          {t("dashboard.mtpUnavailableDetail")}{" "}
          <span className="break-words text-muted">
            {health.mtp_unavailable_reason ?? t("dashboard.mtpReasonUnknown")}
          </span>
        </p>
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
 * O contexto total é multiplicado por `max_sessions` antes de chegar ao
 * llama.cpp. Quando o KV medido está na GPU, duas ou mais sessões podem levar
 * o fit ao limite e transformar uma configuração aparentemente inocente numa
 * degradação grande por chamada. O aviso usa somente o estado efetivo do
 * `/health`, não a configuração pedida pelo deploy.
 */
function ContextReservationNotice({ health }: { health: PeerRuntimeHealth }) {
  const { t } = useI18n();
  const gpuKv =
    health.context_memory?.buffers.some(
      (buffer) => buffer.context > 0 && !isCpuBuffer(buffer.buffer),
    ) ?? false;
  if (health.max_sessions <= 1 || !gpuKv) return null;

  return (
    <p
      role="status"
      className="border-t border-line px-3 py-2 text-[0.7rem] leading-relaxed text-amber"
    >
      {t("dashboard.contextSessionsWarning", {
        sessions: num(health.max_sessions),
        total: num(health.n_ctx_total),
        perSession: num(health.n_ctx_per_session),
        fit: health.fit_memory_applied
          ? t("dashboard.contextFitApplied")
          : t("dashboard.contextFitNotApplied"),
      })}
    </p>
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
    <Accordion
      title={t("dashboard.loadParams")}
      tip={params.fit_memory_applied ? t("dashboard.paramsFitNote") : undefined}
    >
      <dl className="grid gap-x-3 gap-y-2 px-3 pb-2 sm:grid-cols-3">
        <KeyValue compact label={t("dashboard.paramFlashAttn")}>
          <Badge
            tone={params.flash_attn === null ? "neutral" : params.flash_attn ? "aqua" : "amber"}
            className="px-1.5 py-0 text-[0.62rem]"
          >
            {fa}
          </Badge>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramBatch")}>
          <span className="font-mono">
            {num(params.n_batch)} / {num(params.n_ubatch)}
          </span>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramCtx")}>
          <span className="font-mono">
            {num(params.n_ctx)} / {num(params.n_ctx_seq)}
          </span>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramSeqMax")}>
          <span className="font-mono">
            {num(params.n_seq_max)}
            {params.kv_unified ? " · " + t("dashboard.paramKvUnified") : ""}
          </span>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramThreads")}>
          <span className="font-mono">
            {num(params.n_threads)} / {num(params.n_threads_batch)}
          </span>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramGpuLayers")}>
          <span className="font-mono">{num(params.n_gpu_layers)}</span>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramCacheType")}>
          <span className="font-mono">
            {params.cache_type_k} / {params.cache_type_v}
          </span>
        </KeyValue>
        <KeyValue compact label={t("dashboard.paramKqv")}>
          <Badge tone={params.offload_kqv ? "aqua" : "amber"} className="px-1.5 py-0 text-[0.62rem]">
            {t(params.offload_kqv ? "dashboard.paramOnGpu" : "dashboard.paramOnCpu")}
          </Badge>
        </KeyValue>
      </dl>
    </Accordion>
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

/**
 * Um disco, com o mesmo desenho de qualquer outro medidor do card.
 *
 * `disks` traz uma entrada por partição montada, e o rótulo é o ponto de
 * montagem porque é ele que distingue duas partições do mesmo disco; o
 * dispositivo entra no `title` para quem precisa saber de que disco físico se
 * trata. Um nó antigo, que ainda não publica a lista, cai no volume único.
 */
function DiskMeter({ disk, label }: { disk: DiskStat; label: string }) {
  const { t } = useI18n();
  const used = disk.total_mb > 0 ? ((disk.total_mb - disk.free_mb) / disk.total_mb) * 100 : null;
  return (
    <div className="min-w-0" title={disk.device || undefined}>
      <Meter
        compact
        label={label}
        value={used}
        tone={used != null && used > 90 ? "rose" : "amber"}
        detail={disk.total_mb > 0 ? megabytes(disk.free_mb) + " " + t("dashboard.free") : "—"}
      />
    </div>
  );
}

/**
 * Uma placa, inteira num bloco: nome, VRAM, uso, temperatura e potência.
 *
 * A VRAM morava numa grade separada, abaixo de todas as linhas de telemetria,
 * e numa caixa com duas placas era preciso casar "qual barra é de qual linha"
 * pelo nome. Aqui cada placa responde por si.
 */
function GpuTile({
  gpu,
  label,
  policy,
}: {
  gpu: GpuStat;
  label: string;
  policy?: ThermalPolicy;
}) {
  const { t } = useI18n();
  const band = temperatureBand(gpu.temperature_c, policy ?? FALLBACK_THERMAL_POLICY);
  const temperatureLabel = {
    unknown: t("dashboard.temperatureSensorUnavailable"),
    normal: t("dashboard.temperatureNormal"),
    attention: t("dashboard.temperatureAttention"),
    critical: t("dashboard.temperatureCritical"),
  }[band];
  const temperature = band === "unknown"
    ? temperatureLabel
    : `${num(gpu.temperature_c, 1)} °C · ${temperatureLabel}`;
  const tone = {
    unknown: "neutral",
    normal: "aqua",
    attention: "amber",
    critical: "rose",
  }[band] as "neutral" | "aqua" | "amber" | "rose";

  return (
    <div
      data-testid="gpu-telemetry-row"
      className="min-w-0 rounded-lg border border-line bg-raised/60 px-2.5 py-2"
      aria-label={`${label}: ${temperature}`}
    >
      <div className="mb-1.5 truncate text-[0.72rem] font-medium text-fg" title={label}>{label}</div>
      {gpu.vram_used_pct != null && (
        <Meter
          compact
          label="VRAM"
          value={gpu.vram_used_pct}
          tone={gpu.vram_used_pct > 88 ? "rose" : "lime"}
          detail={t("dashboard.usedOf", {
            used: megabytes(gpu.vram_used_mb),
            total: megabytes(gpu.vram_total_mb),
          })}
        />
      )}
      <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[0.66rem]">
        <span className="whitespace-nowrap font-mono text-muted">
          <span className="mr-1 text-faint">{t("dashboard.gpuUsage")}</span>
          {gpu.utilization_pct == null ? num(null) : `${num(gpu.utilization_pct)}%`}
        </span>
        <Badge tone={tone} className="whitespace-nowrap px-1.5 py-0 text-[0.58rem]">
          {temperature}
        </Badge>
        <span className="ml-auto whitespace-nowrap font-mono text-muted">
          <span className="mr-1 text-faint">{t("dashboard.gpuPower")}</span>
          {gpu.power_watts == null || !Number.isFinite(gpu.power_watts)
            ? num(null)
            : `${num(gpu.power_watts)} W`}
        </span>
      </div>
    </div>
  );
}

const FALLBACK_THERMAL_POLICY: ThermalPolicy = {
  limit_c: 85,
  hysteresis_c: 8,
  cooldown_secs: 30,
};

export function PeerDetailCard({
  peer,
  index,
  group,
  otherRanges,
  variant = "panel",
  perf,
  speed,
}: {
  peer: PeerView;
  index: number;
  /** Modelo que o cluster está servindo, que é como o nó agrupa as fatias. */
  group: string;
  /** Faixas dos outros peers deste modelo, para saber se este é o último. */
  otherRanges: [number, number][];
  /**
   * `panel` é o card da aba Detalhes, um entre muitos numa grade. `dialog` é
   * a mesma máquina sozinha na janela do Fluxo: sem moldura (a janela já é
   * uma), com cabeçalho maior e seções nomeadas, porque ali há espaço.
   */
  variant?: "panel" | "dialog";
  /** Velocidade medida e a nota contra as outras fatias, quando o Fluxo a tem. */
  perf?: ChatTelemetryPeerPerf;
  speed?: PeerSpeed;
}) {
  const dialog = variant === "dialog";
  const pad = dialog ? "px-0" : "px-3";
  const { t } = useI18n();
  const track = trackOf(index);
  const n = peer.node;
  const system = n?.system;
  const kind = machineKind(system);
  const cfg = n?.config;
  const serving = isServing(n?.status.state);
  const hasGpu = system != null && system.gpu_kind !== "none";
  // Só as que reportam VRAM: memória unificada cai no ramo `unified` abaixo,
  // que mede outra coisa (wired), e uma barra sem valor viraria animação de
  // "carregando" para sempre.
  const range = cardRange(peer);
  /*
   * O `/health` é pedido uma vez por card, aqui, e não dentro da seção de
   * colocação: o distintivo de MTP subiu para a barra de ferramentas e a
   * seção continua embaixo, e duas buscas para o mesmo lugar poderiam mostrar
   * duas respostas na mesma tela. `enabled` continua sendo o que impede um
   * `/health` por card a cada ciclo do Painel.
   */
  const runtime = usePeerRuntime(peer.node_id, serving);
  const health = serving ? runtime.data?.health : undefined;
  const mtpRequested = cfg?.mtp_enabled ?? false;
  /*
   * A lista de volumes, com o volume do projeto como recuo. A lista vazia é
   * um nó que ainda não publica o campo, não uma máquina sem disco.
   */
  const disks = system ? (system.disks?.length ? system.disks : [system.disk]) : [];

  return (
    <motion.div
      layout
      initial={{ opacity: 0, y: 10 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.26, delay: Math.min(index, 6) * 0.04 }}
      className={dialog ? "relative" : "panel relative overflow-hidden"}
    >
      {!dialog && (
        <span
          className={clsx(
            "absolute inset-x-0 top-0 h-[2px]",
            range ? track.fill : "bg-line-strong",
          )}
        />
      )}

      <div className={clsx("flex flex-wrap items-start justify-between gap-2 border-b border-line", dialog ? "pb-4" : "px-3 py-2.5")}>
        <div className={clsx("flex min-w-0 items-start", dialog ? "gap-3" : "gap-2")}>
          <div className={clsx("mt-0.5 flex shrink-0 items-center justify-center rounded-lg border border-line bg-raised", dialog ? "h-11 w-11" : "h-7 w-7")}>
            <MachineIcon system={system} size={dialog ? "md" : "sm"} />
          </div>
          <div className="min-w-0">
            <div className="flex items-center gap-1.5">
              <h3 className={clsx("truncate font-display font-semibold text-fg", dialog ? "text-lg leading-tight" : "text-[0.86rem]")}>
                {n?.hostname || peer.short_id}
              </h3>
              {peer.is_self && (
                <Badge tone="neutral" className="px-1.5 py-0 text-[0.58rem]">
                  {t("dashboard.here")}
                </Badge>
              )}
            </div>
            <p className={clsx("truncate text-muted", dialog ? "text-[0.75rem]" : "text-[0.66rem]")}>{kind.label}</p>
            {dialog && speed && <div className="mt-2"><SpeedBadges perf={perf} rating={speed} /></div>}
          </div>
        </div>
        <div className={clsx("flex shrink-0 items-center gap-2", dialog && "rounded-lg border border-line bg-raised/50 px-2.5 py-1.5")}>
          {range ? (
            <span className={clsx("font-mono font-semibold", dialog ? "text-sm" : "text-[0.78rem]", track.text)}>
              {layerRange(range[0], range[1])}
            </span>
          ) : (
            // A explicação de "sem fatia" saiu da faixa larga que ocupava uma
            // linha inteira do card e entrou no `[i]` do próprio distintivo.
            <span className="flex items-center gap-1">
              <Badge tone="neutral" className="px-1.5 py-0 text-[0.62rem]">
                {t("dashboard.noSlice")}
              </Badge>
              <InfoTip label={t("dashboard.noSlice")} message={t("dashboard.noSliceDetail")} />
            </span>
          )}
          <span className="flex items-center gap-1 text-[0.66rem] text-muted">
            <Dot
              tone={
                peer.rediscovering
                  ? "amber"
                  : peer.unreachable
                    ? "rose"
                    : serverStateTone(n?.status.state)
              }
              pulse={serving || peer.rediscovering}
            />
            {peer.rediscovering
              ? t("dashboard.rediscoveringShort")
              : (peer.unreachable ?? serverStateLabel(n?.status.state))}
          </span>
        </div>
      </div>

      {peer.rediscovering || peer.unreachable ? (
        <div className="space-y-2 px-3 py-3 text-[0.75rem] text-muted">
          {/*
            Reencontrando não é o mesmo que ilegível. O endereço morreu, o nó
            já pediu o atual, e a próxima volta do gossip costuma resolver:
            dizer só "nada abaixo pode ser lido" mandava o utilizador procurar
            um defeito que se conserta sozinho.
          */}
          {peer.rediscovering
            ? t("dashboard.rediscovering")
            : t("dashboard.unreachable", { error: peer.unreachable ?? "" })}
          <GpuTemperature
            gpuKind={system?.gpu_kind ?? "none"}
            temperature={null}
            policy={n?.thermal}
          />
        </div>
      ) : (
        <>
          {/*
            A barra de ferramentas da máquina, no topo.
            Estavam espalhados pelo pé do card, um por seção, e para reiniciar
            uma máquina era preciso rolar até o fim de tudo o que ela reporta.
            Aqui são o que se faz *com* esta máquina, e ficam onde ela é
            nomeada. O distintivo do MTP vem junto porque é estado de operação,
            não medida: responde "o rascunho está de pé?" e onde ele coube.
          */}
          <div className={clsx("flex flex-wrap items-center gap-1.5 border-b border-line", dialog ? "py-3" : "bg-raised/40 px-3 py-2")}>
            {mtpRequested && health && (
              <span className="flex items-center gap-1">
                <Badge
                  tone={health.mtp_ready ? "aqua" : "rose"}
                  className="px-1.5 py-0 text-[0.62rem]"
                >
                  {health.mtp_ready
                    ? health.mtp_allocation === "ram"
                      ? t("dashboard.mtpReadyRam")
                      : health.mtp_allocation === "vram"
                        ? t("dashboard.mtpReadyVram")
                        : t("dashboard.mtpReadyUnknown")
                    : t("dashboard.mtpUnavailable")}
                </Badge>
                <InfoTip
                  label={t("dashboard.mtpStatus")}
                  message={
                    health.mtp_ready
                      ? t("dashboard.mtpStatus")
                      : `${t("dashboard.mtpUnavailableDetail")} ${
                          health.mtp_unavailable_reason ?? t("dashboard.mtpReasonUnknown")
                        }`
                  }
                />
              </span>
            )}
            <div className="ml-auto flex flex-wrap items-center justify-end gap-1.5">
              {/* Só no card da própria máquina: quem libera a memória é o nó
                  dono dela, nunca um coordenador metendo a mão. */}
              {peer.is_self && cfg && (
                <ReleaseSlice compact startLayer={cfg.start_layer} endLayer={cfg.end_layer} />
              )}
              {/* Duas saídas diferentes: "Remover" tira do modelo e deixa a
                  máquina no cluster; "Sair da rede" tira do cluster inteiro. */}
              {serving && group && range && (
                <RemoveNode
                  nodeId={peer.node_id}
                  group={group}
                  deployId={peer.node?.config?.deploy_id || undefined}
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
          </div>

          {dialog && <h4 className="pt-4 font-display text-sm font-semibold text-fg">{t("dashboard.sectionResources")}</h4>}
          <div className={clsx("grid gap-2.5 sm:grid-cols-2", pad, dialog ? "pb-4 pt-2" : "py-2.5")}>
            {hasGpu && (
              <section data-testid="gpu-telemetry-section" className="min-w-0 sm:col-span-2">
                <div className="mb-1.5 flex items-center gap-1">
                  <span className="eyebrow">{t("dashboard.gpus")}</span>
                  {system?.gpu_note && (
                    <InfoTip label={t("dashboard.gpus")} message={system.gpu_note} />
                  )}
                </div>
                {system?.gpus.length ? (
                  <div className={clsx("grid min-w-0 gap-1.5", system.gpus.length > 1 && "sm:grid-cols-2")}>
                    {system.gpus.map((g) => (
                      <GpuTile
                        key={g.index}
                        gpu={g}
                        policy={n?.thermal}
                        label={gpuDisplayName(g, system.gpus)}
                      />
                    ))}
                  </div>
                ) : null}
                {system?.gpu_kind === "unified" && (
                  <div className="mt-1.5">
                    <Meter
                      compact
                      label={t("dashboard.wiredMemory")}
                      value={
                        system.wired_mb != null && system.ram.total_mb > 0
                          ? (system.wired_mb / system.ram.total_mb) * 100
                          : null
                      }
                      tone="sky"
                      detail={system.wired_mb != null ? megabytes(system.wired_mb) : "—"}
                    />
                  </div>
                )}
              </section>
            )}
            <Meter
              compact
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
            <Meter
              compact
              label={t("dashboard.load", { count: system?.cpu_count ?? "—" })}
              value={
                system && system.cpu_count > 0
                  ? Math.min(100, (system.load_avg[0] / system.cpu_count) * 100)
                  : null
              }
              tone="iris"
              detail={system ? system.load_avg.map((l) => num(l, 2)).join(" · ") : "—"}
            />
            <div className="grid min-w-0 gap-x-3 gap-y-2 sm:col-span-2 sm:grid-cols-2">
              {disks.map((disk) => (
                <DiskMeter
                  key={disk.mount_point + disk.device}
                  disk={disk}
                  label={t("dashboard.disk", { path: disk.mount_point || "—" })}
                />
              ))}
            </div>
          </div>

          {dialog && <h4 className="border-t border-line pt-4 font-display text-sm font-semibold text-fg">{t("dashboard.sectionDeployment")}</h4>}
          <dl className={clsx("grid gap-x-3 gap-y-2 sm:grid-cols-3", pad, dialog ? "pb-4 pt-2" : "border-t border-line py-2.5")}>
            <KeyValue compact label={t("dashboard.model")}>
              {cfg?.model_label ? (
                <span className="font-mono">{cfg.model_label}</span>
              ) : (
                <span className="text-muted">{t("dashboard.noneLoaded")}</span>
              )}
            </KeyValue>
            <KeyValue compact label={t("dashboard.role")}>
              <Badge tone={range ? "iris" : "neutral"} className="px-1.5 py-0 text-[0.62rem]">
                {cfg
                  ? cfg.single_peer
                    ? t("dashboard.single")
                    : cfg.start_layer === 0
                      ? t("dashboard.prompt")
                      : cfg.end_layer + 1 >= cfg.total_layers
                        ? t("dashboard.tokens")
                        : t("dashboard.middle")
                  : /* Sem fatia carregada não há papel na pipeline. O antigo
                       `node.role` (principal/nó) saiu do `/api/ui/node` com o
                       modelo de confiança e já vinha sempre indefinido aqui. */
                    "—"}
              </Badge>
            </KeyValue>
            {/* Logo ao lado do papel, e não escondido no cabeçalho: a pergunta
                que este campo responde é "os nós estão todos na mesma versão?",
                e ela se responde comparando cards lado a lado. */}
            <KeyValue compact label={t("dashboard.build")}>
              {n?.build_commit ? <span className="font-mono">{n.build_commit}</span> : "—"}
            </KeyValue>
            {/* O arquivo é um caminho longo e raramente é a pergunta: fica no
                `[i]`, e o card ganha a linha de volta. */}
            <KeyValue compact label={t("dashboard.context")}>
              <span className="flex min-w-0 items-center gap-1">
                <span className="truncate font-mono">
                  {cfg
                    ? `${num(cfg.n_ctx)} · ${cfg.max_sessions} ${t("dashboard.session", { count: cfg.max_sessions })}`
                    : "—"}
                </span>
                {cfg && (
                  <InfoTip
                    label={t("dashboard.context")}
                    message={`n_ctx ${num(cfg.n_ctx)} · n_batch ${num(cfg.n_batch)}${
                      cfg.model_path ? ` · ${cfg.model_path}` : ""
                    }`}
                  />
                )}
              </span>
            </KeyValue>
            {/* Por onde este nó fala com aquele: com vários endereços
                anunciados, "qual deles ganhou" só se responde aqui. */}
            <KeyValue compact label={t("dashboard.route")}>
              {peer.route ? (
                <span className="font-mono">
                  {peer.route.addr} · {Math.round(peer.route.rtt_ms)} ms
                </span>
              ) : (
                <span className="text-muted">{t("dashboard.routeUnmeasured")}</span>
              )}
            </KeyValue>
            <KeyValue compact label={t("dashboard.server")}>
              <span className="flex min-w-0 items-center gap-1">
                <span className="truncate">
                  {n ? n.status.detail || serverStateLabel(n.status.state) : "—"}
                </span>
                {n && (
                  <InfoTip
                    label={t("dashboard.server")}
                    message={[
                      peer.inference_url ?? t("dashboard.notAdvertised"),
                      n.status.pid ? `pid ${n.status.pid}` : null,
                      n.status.restarts > 0
                        ? t("dashboard.restarts", { count: n.status.restarts })
                        : null,
                    ]
                      .filter(Boolean)
                      .join(" · ")}
                  />
                )}
              </span>
            </KeyValue>
          </dl>

          {/* Só faz pergunta quem tem a quem perguntar: sem modelo carregado
              não há colocação nenhuma a mostrar. */}
          {/* As seções abaixo trazem o próprio recuo; na janela, sem moldura,
              elas se alinham com o resto puxando a margem. */}
          <div className={clsx(dialog && "-mx-3")}>
          {serving && (
            <PeerPlacement
              health={health}
              kqvRequested={cfg?.offload_kqv ?? true}
              mtpRequested={mtpRequested}
            />
          )}

          {system && system.gpu_processes.length > 0 && (
            <Accordion title={t("dashboard.gpuProcesses")}>
              <ul className="space-y-0.5 px-3 pb-2">
                {system.gpu_processes.map((p) => (
                  <li
                    key={p.pid}
                    className="flex items-baseline justify-between gap-3 font-mono text-[0.7rem]"
                  >
                    <span className="truncate text-muted">
                      {p.name} <span className="text-faint">#{p.pid}</span>
                    </span>
                    <span className="shrink-0 text-fg">{megabytes(p.used_mb)}</span>
                  </li>
                ))}
              </ul>
            </Accordion>
          )}
          </div>
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
  perf,
  speed,
}: {
  peer: PeerView | null;
  group: string;
  otherRanges: [number, number][];
  onClose: () => void;
  perf?: ChatTelemetryPeerPerf;
  speed?: PeerSpeed;
}) {
  const { t } = useI18n();
  return (
    <Modal
      open={peer != null}
      size="xl"
      titleHidden
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
          <PeerDetailCard peer={peer} index={0} group={group} otherRanges={otherRanges} variant="dialog" perf={perf} speed={speed} />
        </div>
      )}
    </Modal>
  );
}
