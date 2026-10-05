/**
 * As propriedades de uma implantação, num inspetor só.
 *
 * Vive aqui, e não dentro da tela de implantar, porque é lido em dois lugares:
 * o operador **edita** estas propriedades ao montar a implantação, e **lê** as
 * mesmas propriedades no Histórico, ao abrir a versão com que uma implantação
 * subiu. Duas listas divergem - um campo novo entra num lado e não no outro, e
 * o operador passa a ver coisas diferentes conforme por onde chegou.
 *
 * Por isso o mesmo componente tem `readOnly`: no modo de leitura não há
 * `<input>` desabilitado nenhum, há o valor escrito. Campo desabilitado ainda
 * se lê como "posso mexer, mas não agora"; um número escrito se lê como o que
 * é - o registro do que aquela implantação usou.
 *
 * **Três categorias, e a fronteira entre elas é de quem é o parâmetro:**
 *
 * * *Configurações padrões* - o que o llama.cpp já tem: contexto, lote,
 *   amostragem (temperatura, top-p/top-k/min-p, penalidades, DRY e semente),
 *   cache KV, e os interruptores de recurso do próprio motor, com onde o MTP
 *   fica logo abaixo dos números do MTP.
 * * *Configurações POM* - o que existe porque a inferência é distribuída:
 *   desativar embeddings, ajuste de memória, chunks de prefill em voo, fusão
 *   de fatias e a precisão do estado oculto entre peers.
 * * *Configurações POM (DESENV)* - o motor de inferência e o descarregamento
 *   ocioso, que ainda estão sendo medidos.
 *
 * **O ícone de informação vem antes do nome**, e não depois: os nomes têm
 * larguras diferentes, e um `[i]` colado ao fim de cada um deixava a coluna de
 * ícones serrilhada. Antes do nome ela é uma coluna reta, do topo ao pé do
 * inspetor. É a mesma razão de o controle ter largura fixa à direita.
 */
import { useState, type ReactNode } from "react";
import { AlertTooltip, InfoTip, Input, Select } from "./ui";
import { MTP_MIN_PROBABILITY, Tick } from "./RuntimeParamsForm";
import { SidecarPicker, type SidecarPick } from "./SidecarPicker";
import {
  DEFAULT_SAMPLER_SEED,
  reasoningBudgetText,
  reasoningBudgetValue,
  seedText,
  withLlamaCppName,
} from "../lib/llamaCppNames";
import type { MtpPlacement, SidecarOption, SidecarSlot } from "../lib/types";
import type {
  DeploymentCapabilities,
  InferenceMode,
  KvCacheType,
  RuntimeCapability,
  RuntimeParams,
  WireDtype,
} from "../lib/types";
import { useI18n } from "../i18n/provider";

/** Toda linha do inspetor: `[i]` · nome · controle. */
const ROW =
  "grid grid-cols-[0.9rem_minmax(0,1fr)_6.5rem] items-center gap-x-1.5 gap-y-0.5 text-[0.65rem]";

type NumericKey =
  | "n_ctx"
  | "max_sessions"
  | "max_new_tokens"
  | "n_batch"
  | "prefill_chunk_tokens"
  | "prefill_max_inflight_chunks"
  | "temperature"
  | "top_p"
  | "top_k"
  | "repeat_penalty"
  | "repeat_last_n"
  | "presence_penalty"
  | "frequency_penalty"
  | "min_p"
  | "dry_multiplier"
  | "dry_base"
  | "dry_allowed_length"
  | "dry_penalty_last_n"
  | "n_ubatch"
  | "speculative_k"
  | "speculative_lookup_window"
  | "mtp_draft_tokens"
  | "mtp_min_probability"
  | "idle_minutes";

/**
 * `step` só existe onde o valor não é inteiro: sem ele o navegador recusa
 * "0.05" num campo numérico e o operador não consegue digitar a temperatura.
 */
const NUMERIC_STEP: Partial<Record<NumericKey, string>> = {
  temperature: "0.05",
  top_p: "0.01",
  repeat_penalty: "0.01",
  presence_penalty: "0.05",
  frequency_penalty: "0.05",
  min_p: "0.01",
  dry_multiplier: "0.05",
  dry_base: "0.05",
  mtp_min_probability: "0.01",
};

/** A ordem em que a categoria "padrões" mostra os números do llama.cpp. */
const DEFAULT_NUMERIC_KNOBS: NumericKey[] = [
  "n_ctx",
  "max_sessions",
  "max_new_tokens",
  "n_batch",
  "n_ubatch",
  "prefill_chunk_tokens",
  // Amostragem na ordem do llama.cpp: temperatura e os cortes do conjunto de
  // candidatos, depois as penalidades de repetição, depois o DRY (que também é
  // uma penalidade de repetição, por sequência). A semente vem logo depois.
  "temperature",
  "top_p",
  "top_k",
  "min_p",
  "repeat_penalty",
  "repeat_last_n",
  "presence_penalty",
  "frequency_penalty",
  "dry_multiplier",
  "dry_base",
  "dry_allowed_length",
  "dry_penalty_last_n",
];

/** O DRY se desliga com o multiplicador em 0; os outros números dele só valem ligado. */
const DRY_DETAIL_KNOBS: NumericKey[] = ["dry_base", "dry_allowed_length", "dry_penalty_last_n"];

export const CACHE_TYPE_LABELS: Record<KvCacheType, string> = {
  f32: "F32",
  f16: "F16",
  bf16: "BF16",
  q8_0: "Q8_0",
  q4_0: "Q4_0",
  q4_1: "Q4_1",
  iq4_nl: "IQ4_NL",
  q5_0: "Q5_0",
  q5_1: "Q5_1",
};

/** O cache KV é da instalação, não da implantação: quem o edita passa isto. */
export interface CacheTypeControl {
  k: KvCacheType;
  v: KvCacheType;
  choices: KvCacheType[];
  busy?: boolean;
  onChange: (part: "k" | "v", value: KvCacheType) => void;
}

/** A fusão de fatias vizinhas só existe quando há fatias. */
export interface MergeControl {
  value: boolean;
  onChange: (next: boolean) => void;
}

/** Vision is a deployment control, even though only the pipeline head uses it. */
/** One sidecar choice in the inspector: the pick, what the plan did with it. */
export interface SidecarChoiceControl {
  value: SidecarPick;
  onChange: (next: SidecarPick) => void;
  slot?: SidecarSlot | null;
  /** Describes the model's saved link; `null` when none is saved. */
  linkLabel: string | null;
}

/**
 * Vision projector for the head and MTP draft for the tail, picked among the
 * compatible files of the whole network (`DeployPlan.sidecars.options`).
 */
export interface SidecarControls {
  options: SidecarOption[];
  vision: SidecarChoiceControl;
  mtp: SidecarChoiceControl & { allowEmbedded: boolean };
}

export interface VisionControl {
  available: boolean;
  enabled: boolean;
  path?: string | null;
  candidates: string[];
  reason?: string | null;
  onChange: (next: boolean) => void;
  onPathChange: (next: string) => void;
}

function Section({
  title,
  tone,
  children,
}: {
  title: string;
  tone: "iris" | "aqua" | "amber";
  children: ReactNode;
}) {
  const head = {
    iris: "border-iris/20 bg-iris/10 text-iris",
    aqua: "border-aqua/20 bg-aqua/10 text-aqua",
    amber: "border-amber/25 bg-amber/10 text-amber",
  }[tone];
  const body = {
    iris: "border-iris/20 bg-iris/[0.04]",
    aqua: "border-aqua/20 bg-aqua/[0.035]",
    amber: "border-amber/25 bg-amber/[0.04]",
  }[tone];
  return (
    <section className={`border-b ${body}`}>
      <p
        className={`border-b px-2 py-1.5 text-[0.6rem] font-semibold uppercase tracking-[0.12em] ${head}`}
      >
        {title}
      </p>
      <div className="space-y-1 px-2 py-2">{children}</div>
    </section>
  );
}

/**
 * Uma propriedade: o `[i]` na coluna da esquerda, o nome, e o controle.
 *
 * O aviso de capacidade entra na **mesma** coluna do `[i]`, e não ao lado do
 * controle: a coluna continua reta, e a linha que tem algo a avisar se
 * distingue pela cor do ícone.
 */
export function InspectorRow({
  label,
  tip,
  alert,
  children,
}: {
  label: string;
  tip?: string;
  alert?: string;
  children: ReactNode;
}) {
  return (
    <div className={ROW}>
      <span className="flex h-4 items-center justify-center">
        {alert ? (
          <AlertTooltip compact label={label} message={alert} />
        ) : tip ? (
          <InfoTip label={label} message={tip} />
        ) : null}
      </span>
      <span className="min-w-0 leading-snug text-muted">{label}</span>
      <span className="flex min-w-0 justify-end">{children}</span>
    </div>
  );
}

/**
 * O número como o operador o escreveu, não como o `f32` o guardou.
 *
 * A temperatura sai do nó como 0.699999988079071 - é o mesmo 0,7 depois de uma
 * volta por precisão simples -, e imprimi-lo assim enche a coluna de dígitos
 * que ninguém digitou e que não significam nada. O arredondamento é só de
 * exibição: o valor enviado continua sendo o que estava no campo.
 */
function showNumber(value: unknown): string {
  if (typeof value !== "number" || !Number.isFinite(value)) return String(value ?? "");
  return String(Number(value.toFixed(6)));
}

/** O valor, quando não há nada a mexer nele. */
const MTP_PLACEMENT_LABELS: Record<MtpPlacement, string> = {
  auto: "deploy.mtpPlacementAuto",
  vram: "deploy.mtpPlacementVram",
  ram: "deploy.mtpPlacementRam",
};

function ReadValue({ children }: { children: ReactNode }) {
  return (
    <span className="truncate text-right font-mono text-[0.65rem] text-fg">{children}</span>
  );
}

export function DeploymentInspector({
  value,
  onChange,
  readOnly = false,
  capabilities,
  cacheTypes,
  merge,
  vision,
  sidecars,
  /**
   * O motor só é uma pergunta quando o modelo é MoE: num modelo denso não há
   * escolha a fazer, e um combo com uma resposta só é ruído. O Histórico
   * mostra sempre, porque ali é registro do que subiu, não pergunta.
   */
  showInferenceMode = false,
  inferenceModeAlert,
  /** A precisão do estado oculto só existe quando o pipeline cruza máquinas. */
  showHiddenStateDtype = false,
  pomDesenv = false,
}: {
  value: RuntimeParams;
  onChange?: (next: RuntimeParams) => void;
  readOnly?: boolean;
  capabilities?: DeploymentCapabilities;
  cacheTypes?: CacheTypeControl;
  merge?: MergeControl;
  vision?: VisionControl;
  sidecars?: SidecarControls;
  showInferenceMode?: boolean;
  inferenceModeAlert?: string | null;
  showHiddenStateDtype?: boolean;
  pomDesenv?: boolean;
}) {
  const { t } = useI18n();
  /*
   * O texto meio digitado. Sem ele, apagar o "8192" para escrever "16384"
   * fazia o campo pular para zero no primeiro caractere apagado, porque o
   * valor volta como número a cada tecla.
  */
  const [drafts, setDrafts] = useState<Record<string, string>>({});

  /**
   * O rascunho vale enquanto ele **é** o valor - ou enquanto está vazio, que é
   * o momento em que ele existe para alguma coisa. Assim que o valor muda por
   * fora (voltar ao plano do nó, trocar de modelo), o rascunho deixa de casar e
   * a tela mostra o valor de verdade, sem precisar de ninguém para limpá-lo.
   */
  function shown(key: NumericKey): string {
    const draft = drafts[key];
    if (draft !== undefined && (draft === "" || Number(draft) === value[key])) return draft;
    return showNumber(value[key]);
  }
  const set = (patch: Partial<RuntimeParams>) => onChange?.({ ...value, ...patch });

  const hiddenStateDtype = value.hidden_state_dtype ?? "f16";
  const hiddenStateCapability = hiddenStateDtype === "f8"
    ? capabilities?.hidden_state_f8
    : hiddenStateDtype === "i4g64"
      ? capabilities?.hidden_state_i4g64
      : hiddenStateDtype === "i4g128"
        ? capabilities?.hidden_state_i4g128
        : undefined;
  const hiddenStateUnavailable = hiddenStateCapability
    ? hiddenStateCapability.state !== "supported"
    : false;
  const f8Unavailable = capabilities?.hidden_state_f8
    ? capabilities.hidden_state_f8.state !== "supported"
    : false;
  const i4g64Unavailable = capabilities?.hidden_state_i4g64?.state !== "supported";
  const i4g128Unavailable = capabilities?.hidden_state_i4g128?.state !== "supported";

  /*
   * `auto` não está mais na combo, mas pode chegar de uma API ou de uma
   * implantação persistida. A apresentação cai visualmente em `dense` para
   * manter a seleção válida, sem mutar o estado recebido; o backend continua
   * aplicando a escolha explícita.
   */

  function setKnob(key: NumericKey, text: string) {
    setDrafts((current) => ({ ...current, [key]: text }));
    const parsed = Number(text);
    if (text !== "" && Number.isFinite(parsed)) set({ [key]: parsed } as Partial<RuntimeParams>);
  }

  function numeric(key: NumericKey) {
    const label = t("inspector.knob." + key);
    return (
      <InspectorRow key={key} label={label} tip={withLlamaCppName(t("deploy.knob." + key + "Hint"), key)}>
        {readOnly ? (
          <ReadValue>{value[key] == null ? "-" : showNumber(value[key])}</ReadValue>
        ) : (
          <Input
            aria-label={label}
            name={key}
            type="number"
            min={key === "idle_minutes" ? 1 : undefined}
            step={NUMERIC_STEP[key]}
            value={shown(key)}
            onChange={(event) => setKnob(key, event.target.value)}
            onBlur={() =>
              setDrafts((current) => {
                const { [key]: _drop, ...rest } = current;
                return rest;
              })
            }
            compact
            className="text-right font-mono"
          />
        )}
      </InspectorRow>
    );
  }

  /**
   * Um interruptor de recurso, com o que o nó respondeu sobre ele.
   *
   * `unsupported` some da tela quando não é `alwaysVisible`: oferecer o que o
   * runtime provou não ter é convidar a uma implantação que não sobe.
   */
  function toggle({
    key,
    label,
    capability,
    alwaysVisible = false,
    checked,
    onToggle,
    tip,
  }: {
    key: string;
    label: string;
    capability?: RuntimeCapability;
    alwaysVisible?: boolean;
    checked: boolean;
    onToggle: (next: boolean) => void;
    tip?: string;
  }) {
    if (capability?.state === "unsupported" && !alwaysVisible) return null;
    const unknown = capability?.state === "unknown";
    const topologyWarning = capability?.reason.startsWith("controle visível");
    const warning = capability?.warning;
    const alert =
      capability && (unknown || topologyWarning || warning)
        ? `${
            unknown
              ? t("inspector.capabilityUnknown")
              : topologyWarning
                ? t("inspector.capabilityTopology")
                : t("inspector.capabilityLimited")
          } ${t("inspector.capabilitySource", { source: capability.source })} ${capability.reason}${
            warning ? ` ${warning}` : ""
          }`
        : undefined;
    return (
      <InspectorRow key={key} label={label} tip={tip ? withLlamaCppName(tip, key) : tip} alert={alert}>
        <Tick
          on={checked}
          label={label}
          disabled={readOnly}
          onToggle={() => onToggle(!checked)}
        />
      </InspectorRow>
    );
  }

  const speculativeOn = value.speculative_decoding_enabled ?? false;
  const dryOn = (value.dry_multiplier ?? 0) > 0;
  const mtpOn = value.mtp_enabled ?? false;

  return (
    <>
      <Section title={t("inspector.sectionDefaults")} tone="iris">
        {DEFAULT_NUMERIC_KNOBS.filter((key) => dryOn || !DRY_DETAIL_KNOBS.includes(key)).map((key) => numeric(key))}
        <InspectorRow label={t("inspector.knob.seed")} tip={withLlamaCppName(t("deploy.knob.seedHint"), "seed")}>
          {readOnly ? (
            <ReadValue>{value.seed ?? DEFAULT_SAMPLER_SEED}</ReadValue>
          ) : (
            <Input
              aria-label={t("inspector.knob.seed")}
              name="seed"
              type="text"
              inputMode="numeric"
              value={value.seed ?? DEFAULT_SAMPLER_SEED}
              onChange={(event) => set({ seed: seedText(event.target.value) })}
              onBlur={() => {
                if (!value.seed) set({ seed: DEFAULT_SAMPLER_SEED });
              }}
            />
          )}
        </InspectorRow>

        {cacheTypes &&
          (["k", "v"] as const).map((part) => {
            const label = part === "k" ? t("inspector.cacheK") : t("inspector.cacheV");
            const current = part === "k" ? cacheTypes.k : cacheTypes.v;
            return (
              <InspectorRow key={`cache_${part}`} label={label} tip={withLlamaCppName(t("inspector.cacheHint"), `cache_type_${part}`)}>
                {readOnly ? (
                  <ReadValue>{CACHE_TYPE_LABELS[current]}</ReadValue>
                ) : (
                  <Select
                    aria-label={label}
                    name={`cache_type_${part}`}
                    value={current}
                    disabled={cacheTypes.busy}
                    onChange={(event) =>
                      cacheTypes.onChange(part, event.target.value as KvCacheType)
                    }
                    compact
                  >
                    {cacheTypes.choices.map((choice) => (
                      <option key={choice} value={choice}>
                        {CACHE_TYPE_LABELS[choice]}
                      </option>
                    ))}
                  </Select>
                )}
              </InspectorRow>
            );
          })}

        {toggle({
          key: "no_thinking",
          label: t("inspector.noThinking"),
          capability: capabilities?.no_thinking,
          checked: value.no_thinking ?? false,
          onToggle: (next) => set({ no_thinking: next }),
          tip: t("deploy.noThinkingHint"),
        })}
        {!(value.no_thinking ?? false) && (
          <InspectorRow
            label={t("inspector.knob.reasoning_budget")}
            tip={withLlamaCppName(t("deploy.knob.reasoning_budgetHint"), "reasoning_budget")}
          >
            {readOnly ? (
              <ReadValue>{reasoningBudgetText(value.reasoning_budget) || t("deploy.reasoningBudgetUnlimited")}</ReadValue>
            ) : (
              <Input
                aria-label={t("inspector.knob.reasoning_budget")}
                name="reasoning_budget"
                type="text"
                inputMode="numeric"
                placeholder={t("deploy.reasoningBudgetUnlimited")}
                value={reasoningBudgetText(value.reasoning_budget)}
                onChange={(event) => set({ reasoning_budget: reasoningBudgetValue(event.target.value) })}
              />
            )}
          </InspectorRow>
        )}
        {toggle({
          key: "speculative_decoding_enabled",
          label: t("deploy.speculative"),
          capability: capabilities?.speculative_decoding,
          checked: speculativeOn,
          onToggle: (next) => set({ speculative_decoding_enabled: next }),
          tip: t("deploy.speculativeHint"),
        })}
        {speculativeOn && capabilities?.speculative_decoding?.state !== "unsupported" && (
          <>
            {numeric("speculative_k")}
            {numeric("speculative_lookup_window")}
          </>
        )}
        {toggle({
          key: "mtp_enabled",
          label: t("deploy.mtp"),
          capability: capabilities?.mtp,
          checked: mtpOn,
          onToggle: (next) =>
            set({
              mtp_enabled: next,
              ...(next ? { mtp_min_probability: MTP_MIN_PROBABILITY } : {}),
            }),
          tip: t("deploy.mtpHint"),
        })}
        {mtpOn && capabilities?.mtp?.state !== "unsupported" && (
          <>
            {numeric("mtp_draft_tokens")}
            {numeric("mtp_min_probability")}
                <InspectorRow label={t("deploy.mtpPlacement")} tip={t("deploy.mtpPlacementHint")}>
              {readOnly ? (
                <ReadValue>{t(MTP_PLACEMENT_LABELS[value.mtp_placement ?? "auto"])}</ReadValue>
              ) : (
                <Select
                  aria-label={t("deploy.mtpPlacement")}
                  name="mtp_placement"
                  value={value.mtp_placement ?? "auto"}
                  onChange={(event) => set({ mtp_placement: event.target.value as MtpPlacement })}
                  compact
                >
                  <option value="auto">{t("deploy.mtpPlacementAuto")}</option>
                  <option value="vram">{t("deploy.mtpPlacementVram")}</option>
                  <option value="ram">{t("deploy.mtpPlacementRam")}</option>
                </Select>
              )}
            </InspectorRow>
          </>
        )}
        {mtpOn && sidecars && !readOnly && (
          <div className="px-0.5 py-1.5">
            <p className="mb-1 text-[0.6rem] text-muted">{t("sidecars.mtp")}</p>
            <SidecarPicker
              kind="mtp"
              label={t("sidecars.mtp")}
              options={sidecars.options.filter((option) => option.kind === "mtp")}
              value={sidecars.mtp.value}
              onChange={sidecars.mtp.onChange}
              slot={sidecars.mtp.slot}
              linkLabel={sidecars.mtp.linkLabel}
              allowEmbedded={sidecars.mtp.allowEmbedded}
              compact
            />
          </div>
        )}
      </Section>

      {vision && (
        <Section title={t("deploy.vision")} tone="aqua">
          {vision.available ? (
            <>
              <InspectorRow label={t("deploy.visionToggle")} tip={t("deploy.visionHint")}>
                <Tick
                  on={vision.enabled}
                  label={t("deploy.visionToggle")}
                  disabled={readOnly}
                  onToggle={() => vision.onChange(!vision.enabled)}
                />
              </InspectorRow>
              {vision.enabled && sidecars && !readOnly ? (
                <div className="px-0.5 py-1.5">
                  <p className="mb-1 text-[0.6rem] text-muted">{t("sidecars.vision")}</p>
                  <SidecarPicker
                    kind="mmproj"
                    label={t("sidecars.vision")}
                    options={sidecars.options.filter((option) => option.kind === "mmproj")}
                    value={sidecars.vision.value}
                    onChange={sidecars.vision.onChange}
                    slot={sidecars.vision.slot}
                    linkLabel={sidecars.vision.linkLabel}
                    compact
                  />
                </div>
              ) : vision.candidates.length > 1 ? (
                <InspectorRow label="Modelo de visão" tip="Escolha o projetor compatível que será usado na cabeça da implantação.">
                  <Select
                    aria-label="Projetor de visão"
                    value={vision.path ?? ""}
                    disabled={readOnly}
                    onChange={(event) => vision.onPathChange(event.target.value)}
                    className="h-7 min-w-0 w-full text-[0.58rem]"
                  >
                    {vision.candidates.map((candidate) => (
                      <option key={candidate} value={candidate}>
                        {candidate.split("/").pop() ?? candidate}
                      </option>
                    ))}
                  </Select>
                </InspectorRow>
              ) : vision.path ? (
                <p className="break-all px-0.5 text-[0.6rem] leading-relaxed text-faint">
                  {vision.path}
                </p>
              ) : null}
            </>
          ) : (
            <p role="alert" className="px-0.5 text-[0.6rem] leading-relaxed text-amber">
              {vision.reason || t("deploy.visionUnavailable")}
            </p>
          )}
        </Section>
      )}

      <Section title={t("inspector.sectionPom")} tone="aqua">
        {toggle({
          key: "no_embeddings",
          label: t("inspector.noEmbeddings"),
          capability: capabilities?.no_embeddings,
          alwaysVisible: true,
          checked: value.no_embeddings ?? false,
          onToggle: (next) => set({ no_embeddings: next }),
          tip: t("deploy.noEmbeddingsHint"),
        })}
        {toggle({
          key: "fit_memory",
          label: t("inspector.fitMemory"),
          capability: capabilities?.fit_memory,
          checked: value.fit_memory ?? true,
          onToggle: (next) => set({ fit_memory: next }),
          tip: t("deploy.fitMemoryHint"),
        })}
        {numeric("prefill_max_inflight_chunks")}
        {merge && (
          <InspectorRow label={t("deploy.merge")} tip={t("deploy.mergeHint")}>
            <Tick
              on={merge.value}
              label={t("deploy.merge")}
              disabled={readOnly}
              onToggle={() => merge.onChange(!merge.value)}
            />
          </InspectorRow>
        )}
        {showHiddenStateDtype && (
          <>
            <InspectorRow
              label={t("deploy.knob.hidden_state_dtype")}
              tip={t("deploy.knob.hidden_state_dtypeHint")}
            >
              {readOnly ? (
                <ReadValue>{value.hidden_state_dtype ?? "f16"}</ReadValue>
              ) : (
                <Select
                  aria-label={t("deploy.knob.hidden_state_dtype")}
                  name="hidden_state_dtype"
                  value={hiddenStateDtype}
                  onChange={(event) =>
                    set({ hidden_state_dtype: event.target.value as WireDtype })
                  }
                  compact
                >
                  <option value="f32">{t("deploy.knob.hidden_state_dtypeF32")}</option>
                  <option value="f16">{t("deploy.knob.hidden_state_dtypeF16")}</option>
                  <option value="f8" disabled={f8Unavailable}>
                    {t("deploy.knob.hidden_state_dtypeF8")}
                  </option>
                  <option value="i4g64" disabled={i4g64Unavailable}>
                    {t("deploy.knob.hidden_state_dtypeI4G64")}
                  </option>
                  <option value="i4g128" disabled={i4g128Unavailable}>
                    {t("deploy.knob.hidden_state_dtypeI4G128")}
                  </option>
                </Select>
              )}
            </InspectorRow>
            {!readOnly && hiddenStateCapability && hiddenStateUnavailable && (
              <p role="alert" className="px-0.5 text-[0.6rem] leading-relaxed text-amber">
                {t("deploy.knob.hidden_state_dtypeUnavailable")}
                {" "}
                {hiddenStateCapability.reason}
              </p>
            )}
          </>
        )}
      </Section>

      {pomDesenv && <Section title={t("inspector.sectionPomDev")} tone="amber">
        {showInferenceMode && (
          <InspectorRow
            label={t("inspector.inferenceMode")}
            tip={t("inspector.inferenceModeHint")}
            alert={inferenceModeAlert ?? undefined}
          >
            {readOnly ? (
              <ReadValue>{t("inspector.engine." + (value.inference_mode ?? "dense"))}</ReadValue>
            ) : (
              <Select
                aria-label={t("inspector.inferenceMode")}
                name="inference_mode"
                value={value.inference_mode === "auto" ? "dense" : (value.inference_mode ?? "dense")}
                onChange={(event) =>
                  set({ inference_mode: event.target.value as InferenceMode })
                }
                compact
              >
                {/* "Automático" continua existindo no backend; sai daqui porque
                    a interface passou a decidir por llama.cpp. */}
                <option value="dense">{t("inspector.engine.dense")}</option>
                <option value="moe">{t("inspector.engine.moe")}</option>
              </Select>
            )}
          </InspectorRow>
        )}
        {toggle({
          key: "unload_if_idle",
          label: t("deploy.unloadIfIdle"),
          alwaysVisible: true,
          checked: value.unload_if_idle ?? false,
          onToggle: (next) => set({ unload_if_idle: next }),
          tip: t("deploy.unloadIfIdleHint"),
        })}
        {numeric("idle_minutes")}
      </Section>}
    </>
  );
}
