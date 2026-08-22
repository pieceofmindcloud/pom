/**
 * Os parâmetros de execução de uma implantação, num formulário só.
 *
 * Estava embutido na tela de implantar. Saiu de lá quando a tela de
 * implantações ganhou "reimplantar mexendo num parâmetro": os dois precisam da
 * **mesma** lista de campos, e duas cópias divergem — um botão novo entra num
 * lugar e não no outro, e o operador passa a ver coisas diferentes conforme por
 * onde chegou.
 *
 * O componente é controlado (`value`/`onChange`) mas guarda um estado próprio: o
 * texto meio digitado. Sem isso, apagar o "8192" para escrever "16384" fazia o
 * campo pular para zero no primeiro caractere apagado, porque o valor volta como
 * número a cada tecla. O rascunho vive enquanto o campo está em foco e some no
 * `blur`, quando o número já é número.
 */
import { useState } from "react";
import { Check } from "lucide-react";
import { Field, Input } from "./ui";
import { num } from "../lib/format";
import type { RuntimeParams } from "../lib/types";
import { useI18n } from "../i18n/provider";

type Knob = {
  key: keyof RuntimeParams;
  min: number;
  max?: number;
  step?: number;
};

/** Os campos que aparecem sempre. */
export const KNOBS: Knob[] = [
  { key: "n_gpu_layers", min: 0 },
  { key: "n_ctx", min: 512, max: 250000, step: 512 },
  { key: "n_batch", min: 32, step: 32 },
  { key: "max_new_tokens", min: 16 },
  { key: "max_sessions", min: 1, max: 16 },
  { key: "prefill_chunk_tokens", min: 32, step: 32 },
  { key: "temperature", min: 0, max: 2, step: 0.05 },
  { key: "top_p", min: 0, max: 1, step: 0.05 },
  { key: "top_k", min: 0 },
  { key: "repeat_penalty", min: 1, max: 2, step: 0.01 },
  { key: "repeat_last_n", min: 0 },
];

/**
 * Só aparecem com a especulação ligada: fora dali são dois números sem efeito
 * nenhum, e a aba já tem onze campos.
 */
export const SPECULATIVE_KNOBS: Knob[] = [
  { key: "speculative_k", min: 1, max: 16 },
  { key: "speculative_lookup_window", min: 1, max: 8 },
];

/**
 * O piso de confiança do MTP é **fixo**, não uma sugestão inicial: marcar o MTP
 * o grava, e não há campo para movê-lo depois. Fica aqui, ao lado dos outros
 * números do MTP, para que as duas telas que ligam o MTP gravem o mesmo valor -
 * duas constantes iguais em lugares diferentes é como elas passam a divergir.
 */
export const MTP_MIN_PROBABILITY = 0.75;

/**
 * Só aparecem com o MTP ligado. `mtp_draft_tokens` tem o mesmo teto sensato da
 * largura do rascunho por lookup. `mtp_min_probability` saiu daqui por deixar
 * de ser editável: continua na tela, como número mostrado, não como campo.
 */
export const MTP_KNOBS: Knob[] = [
  { key: "mtp_draft_tokens", min: 1, max: 8 },
];

/**
 * A caixinha de marcar da tela de implantar, movida junto com o formulário para
 * que os dois lugares continuem com o mesmo desenho e a mesma semântica de
 * acessibilidade (`role="checkbox"` + `aria-label`), que é como os testes a
 * encontram.
 */
export function Tick({
  on,
  label,
  onToggle,
}: {
  on: boolean;
  label: string;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      role="checkbox"
      aria-checked={on}
      aria-label={label}
      onClick={onToggle}
      className={`flex h-[18px] w-[18px] items-center justify-center rounded-[5px] border transition-colors duration-150 ${
        on
          ? "border-iris bg-iris text-white"
          : "border-line-strong bg-ink text-transparent hover:border-iris/60"
      }`}
    >
      <Check className="h-3 w-3" strokeWidth={3} />
    </button>
  );
}

export function Switch({
  checked,
  onChange,
  label,
  hint,
  disabled,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: string;
  hint?: string;
  disabled?: boolean;
}) {
  return (
    <div className={`flex items-start gap-2.5 ${disabled ? "opacity-55" : ""}`}>
      <span className="mt-0.5">
        {disabled ? (
          <Tick on={false} label={label} onToggle={() => {}} />
        ) : (
          <Tick on={checked} label={label} onToggle={() => onChange(!checked)} />
        )}
      </span>
      <span className="min-w-0">
        <span className={`block text-[0.82rem] ${disabled ? "text-muted" : "text-fg"}`}>
          {label}
        </span>
        {hint && <span className="mt-0.5 block text-xs text-faint">{hint}</span>}
      </span>
    </div>
  );
}

export function RuntimeParamsForm({
  value,
  onChange,
  disabled,
  /**
   * Num grupo de tensor parallelism o `fit` nativo do llama.cpp não sabe lidar
   * com `split-mode tensor`, então o botão fica desligado e travado — mostrar
   * um interruptor que não faz nada é pior que mostrar um desabilitado.
   */
  isTp = false,
}: {
  value: RuntimeParams;
  onChange: (next: RuntimeParams) => void;
  disabled?: boolean;
  isTp?: boolean;
}) {
  const { t } = useI18n();
  const [drafts, setDrafts] = useState<Record<string, string>>({});

  function setKnob(key: keyof RuntimeParams, text: string) {
    setDrafts((d) => ({ ...d, [key]: text }));
    const n = Number(text);
    if (text !== "" && Number.isFinite(n)) {
      onChange({ ...value, [key]: n });
    }
  }

  const numeric = (k: Knob, withHint: boolean) => (
    <Field
      key={k.key}
      label={t("deploy.knob." + k.key)}
      hint={withHint ? t("deploy.knob." + k.key + "Hint") : undefined}
    >
      <Input
        type="number"
        name={k.key}
        min={k.min}
        max={k.max}
        step={k.step}
        disabled={disabled}
        value={drafts[k.key] ?? String(value[k.key] ?? "")}
        onChange={(e) => setKnob(k.key, e.target.value)}
        onBlur={() =>
          setDrafts((d) => {
            const { [k.key]: _drop, ...rest } = d;
            return rest;
          })
        }
      />
    </Field>
  );

  return (
    <div className="space-y-5">
      <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-1">
        {KNOBS.map((k) => numeric(k, false))}
      </div>

      {/* `offload_kqv` não está aqui: ele é propriedade da memória de uma
          máquina, não do pipeline, e por isso vive na tabela de máquinas. */}
      <div className="grid gap-4 border-t border-line pt-4 sm:grid-cols-2 xl:grid-cols-1">
        <Switch
          checked={!isTp && (value.fit_memory ?? true)}
          onChange={(v) => onChange({ ...value, fit_memory: v })}
          disabled={disabled || isTp}
          label={t("deploy.fitMemory")}
          hint={t(isTp ? "deploy.fitMemoryTpHint" : "deploy.fitMemoryHint")}
        />
        <Switch
          checked={value.no_thinking ?? true}
          onChange={(v) => onChange({ ...value, no_thinking: v })}
          disabled={disabled}
          label={t("deploy.noThinking")}
          hint={t("deploy.noThinkingHint")}
        />
        <Switch
          checked={value.no_embeddings ?? false}
          onChange={(v) => onChange({ ...value, no_embeddings: v })}
          disabled={disabled}
          label={t("deploy.noEmbeddings")}
          hint={t("deploy.noEmbeddingsHint")}
        />
        <Switch
          checked={value.speculative_decoding_enabled ?? false}
          onChange={(v) => onChange({ ...value, speculative_decoding_enabled: v })}
          disabled={disabled}
          label={t("deploy.speculative")}
          hint={t("deploy.speculativeHint")}
        />
        <Switch
          checked={value.mtp_enabled ?? false}
          onChange={(v) =>
            onChange({
              ...value,
              mtp_enabled: v,
              ...(v ? { mtp_min_probability: MTP_MIN_PROBABILITY } : {}),
            })
          }
          disabled={disabled}
          label={t("deploy.mtp")}
          hint={t("deploy.mtpHint")}
        />
      </div>

      {(value.speculative_decoding_enabled ?? false) && (
        <div className="grid gap-4 border-t border-line pt-4 sm:grid-cols-2 xl:grid-cols-1">
          {SPECULATIVE_KNOBS.map((k) => numeric(k, true))}
        </div>
      )}

      {(value.mtp_enabled ?? false) && (
        <div className="grid gap-4 border-t border-line pt-4 sm:grid-cols-2 xl:grid-cols-1">
          {MTP_KNOBS.map((k) => numeric(k, true))}
          <Field
            label={t("deploy.knob.mtp_min_probability")}
            hint={t("deploy.knob.mtp_min_probabilityHint")}
          >
            <span className="block font-mono text-sm text-muted">
              {num(MTP_MIN_PROBABILITY, 2)}
            </span>
          </Field>
        </div>
      )}
    </div>
  );
}
