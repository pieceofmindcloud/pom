/**
 * The properties of a deployment, in a single inspector.
 *
 * It lives here, and not inside the deploy screen, because it is read in two places:
 * the operator **edits** these properties when assembling the deployment, and **reads** the
 * same properties in the History, when opening the version a deployment
 * came up with. Two lists diverge - a new field goes in on one side and not the other, and
 * the operator starts to see different things depending on how they got there.
 *
 * That is why the same component has `readOnly`: in read mode there is no disabled
 * `<input>` at all, there is the written value. A disabled field still
 * reads as "I can touch it, but not now"; a written number reads as what it
 * is - the record of what that deployment used.
 *
 * **Three categories, and the border between them is whose the parameter is:**
 *
 * * *Default settings* - what llama.cpp already has: context, batch,
 *   sampling (temperature, top-p/top-k/min-p, penalties, DRY and seed),
 *   KV cache, MoE experts in RAM and the GPU expert cache, and the feature
 *   switches of the engine itself, with where the MTP sits right below the MTP
 *   numbers.
 * * *POM settings* - what exists because inference is distributed:
 *   disabling embeddings, memory adjustment, in-flight prefill chunks, the
 *   automatic prefill chunk, fusion of slices and the precision of the hidden
 *   state between peers. A *Beta* group for settings still being measured lives
 *   inside it, commented out while it has none.
 * * *POM settings (DEV)* - the inference engine and idle
 *   unloading, which are still being measured.
 *
 * Each category has a help button that opens, in a dialog,
 * its guide in the Documentation (`guides/deploySettings.ts`): what each
 * property does and how to change it.
 *
 * **The information icon comes before the name**, and not after: the names have
 * different widths, and an `[i]` glued to the end of each one left the column of
 * icons jagged. Before the name it is a straight column, from the top to the bottom of the
 * inspector. It is the same reason the control has a fixed width on the right.
 */
import { EngineSettingsFields } from "./EngineSettings";
import { useState, type ReactNode } from "react";
import { Check } from "lucide-react";
import { AlertTooltip, InfoTip, Input, Select } from "./ui";
import { GuideDialog, GuideHelpButton } from "./GuideDialog";
import { DEPLOY_SETTINGS_GUIDES } from "../guides/deploySettings";
import { speculationModeOf, speculationPatch, type SpeculationMode } from "../lib/speculation";
import { SidecarPicker, type SidecarPick } from "./SidecarPicker";
import {
  DEFAULT_SAMPLER_SEED,
  reasoningBudgetText,
  reasoningBudgetValue,
  seedText,
  withLlamaCppName,
} from "../lib/llamaCppNames";
import type { EngineSetting, InferenceRole, MtpPlacement, PomSetting, SidecarOption, SidecarSlot } from "../lib/types";
import type {
  DeploymentCapabilities,
  InferenceMode,
  KvCacheType,
  RuntimeCapability,
  RuntimeParams,
  WireDtype,
} from "../lib/types";
import { useI18n } from "../i18n/provider";

/** The inspector's checkbox (`role="checkbox"` + `aria-label`, which is how tests find it). */
export function Tick({
  on,
  label,
  onToggle,
  disabled = false,
}: {
  on: boolean;
  label: string;
  onToggle: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      role="checkbox"
      aria-checked={on}
      aria-label={label}
      disabled={disabled}
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

/** Every row of the inspector: `[i]` · name · control. */
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
  | "idle_minutes"
  | "moe_cache_mib";

/**
 * `step` only exists where the value is not an integer: without it the browser refuses
 * "0.05" in a numeric field and the operator cannot type the temperature.
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

/** The order in which the "defaults" category shows the llama.cpp numbers. */
const DEFAULT_NUMERIC_KNOBS: NumericKey[] = [
  "n_ctx",
  "max_sessions",
  "max_new_tokens",
  "n_batch",
  "n_ubatch",
  "prefill_chunk_tokens",
  // Sampling in llama.cpp order: temperature and the cuts of the set of
  // candidates, then the repetition penalties, then DRY (which is also
  // a repetition penalty, per sequence). The seed comes right after.
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

/** DRY is turned off with the multiplier at 0; its other numbers only apply when on. */
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

/** The KV cache belongs to the installation, not the deployment: whoever edits it passes this. */
export interface CacheTypeControl {
  k: KvCacheType;
  v: KvCacheType;
  choices: KvCacheType[];
  busy?: boolean;
  onChange: (part: "k" | "v", value: KvCacheType) => void;
}

/** Merging neighboring slices only exists when there are slices. */
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
  guide,
  children,
}: {
  title: string;
  tone: "iris" | "aqua" | "amber";
  /** The `?guide=` id the help button opens in a dialog. */
  guide?: string;
  children: ReactNode;
}) {
  const { t } = useI18n();
  const [helpOpen, setHelpOpen] = useState(false);
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
      <div className={`flex items-center justify-between gap-2 border-b px-2 py-1 ${head}`}>
        <p className="text-[0.6rem] font-semibold uppercase tracking-[0.12em]">{title}</p>
        {guide && (
          <GuideHelpButton
            label={t("inspector.guideHelp", { section: title })}
            onClick={() => setHelpOpen(true)}
          />
        )}
      </div>
      <div className="space-y-1 px-2 py-2">{children}</div>
      {guide && <GuideDialog guideId={guide} open={helpOpen} onClose={() => setHelpOpen(false)} />}
    </section>
  );
}

// The Beta group is off while it has no setting. It comes back for the next one
// still being measured, together with its guide (`beta-settings`, removed from
// `docs/guides/settings/` and `DEPLOY_SETTINGS_GUIDES` until then: restore both
// from git history).
// /**
//  * A group inside a category, for settings that work but are still being
//  * measured. It carries its own short explanation and a help button, because
//  * their value depends on what each route measures.
//  */
// function BetaGroup({ children }: { children: ReactNode }) {
//   const { t } = useI18n();
//   const [helpOpen, setHelpOpen] = useState(false);
//   const title = t("inspector.sectionBeta");
//   return (
//     <div className="mt-2 rounded-md border border-amber/25 bg-amber/[0.05]">
//       <div className="flex items-center justify-between gap-2 border-b border-amber/20 px-2 py-1 text-amber">
//         <p className="text-[0.6rem] font-semibold uppercase tracking-[0.12em]">{title}</p>
//         <GuideHelpButton
//           label={t("inspector.guideHelp", { section: title })}
//           onClick={() => setHelpOpen(true)}
//         />
//       </div>
//       <button
//         type="button"
//         onClick={() => setHelpOpen(true)}
//         className="block w-full px-2 pt-1.5 text-left text-[0.6rem] leading-relaxed text-muted hover:text-fg"
//       >
//         {t("inspector.betaInfo")}{" "}
//         <span className="text-amber underline-offset-2 hover:underline">{t("inspector.betaInfoLink")}</span>
//       </button>
//       <div className="space-y-1 px-2 py-2">{children}</div>
//       <GuideDialog guideId={DEPLOY_SETTINGS_GUIDES.beta} open={helpOpen} onClose={() => setHelpOpen(false)} />
//     </div>
//   );
// }

/**
 * One property: the `[i]` in the left column, the name, and the control.
 *
 * The capacity warning goes in the **same** column as the `[i]`, and not beside the
 * control: the column stays straight, and the row that has something to warn about
 * is distinguished by the color of the icon.
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
 * The number as the operator wrote it, not as the `f32` stored it.
 *
 * The temperature leaves the node as 0.699999988079071 - it is the same 0.7 after a
 * round trip through single precision -, and printing it that way fills the column with digits
 * that nobody typed and that mean nothing. The rounding is display
 * only: the value sent remains what was in the field.
 */
function showNumber(value: unknown): string {
  if (typeof value !== "number" || !Number.isFinite(value)) return String(value ?? "");
  return String(Number(value.toFixed(6)));
}

/** The value, when there is nothing to change in it. */
const MTP_PLACEMENT_LABELS: Record<MtpPlacement, string> = {
  auto: "deploy.mtpPlacementAuto",
  vram: "deploy.mtpPlacementVram",
  ram: "deploy.mtpPlacementRam",
};

export function ReadValue({ children }: { children: ReactNode }) {
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
   * The engine is only a question when the model is MoE: in a dense model there is no
   * choice to make, and a combo with a single answer is noise. The History
   * always shows it, because there it is a record of what came up, not a question.
   */
  showInferenceMode = false,
  inferenceModeAlert,
  /** The hidden-state precision only exists when the pipeline crosses machines. */
  showHiddenStateDtype = false,
  machineRoles,
  onMachineRole,
  pomDevMode = false,
  inferenceService,
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
  /** A whole model on several machines split into prefill and decode: each machine's role, edited in the POM card. */
  machineRoles?: { nodeId: string; label: string; role: InferenceRole }[];
  onMachineRole?: (nodeId: string, role: InferenceRole) => void;
  pomDevMode?: boolean;
  /** A plugin's inference service runs the model (Strata): its card holds only
   *  what that engine reads - the context, the concurrent requests, idle
   *  unloading and the engine's own settings. The llama.cpp knobs do not
   *  reach it. The POM settings the service declares (`pomSettings`) get their
   *  own card, shown when its slices cross machines. */
  inferenceService?: {
    name: string;
    settings?: EngineSetting[];
    pomSettings?: PomSetting[];
    /** Its engine takes a drafter file for this model (`draft_architectures`). */
    takesDraft?: boolean;
  };
}) {
  const { t } = useI18n();
  /*
   * The half-typed text. Without it, erasing "8192" to write "16384"
   * made the field jump to zero on the first erased character, because the
   * value comes back as a number on every keystroke.
  */
  const [drafts, setDrafts] = useState<Record<string, string>>({});

  /**
   * The draft holds while it **is** the value - or while it is empty, which is
   * the moment it exists for anything. As soon as the value changes from
   * outside (going back to the node's plan, switching models), the draft stops matching and
   * the screen shows the real value, with nobody needed to clear it.
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
   * `auto` is no longer in the combo, but may arrive from an API or from a
   * persisted deployment. The presentation falls back visually to `dense` to
   * keep the selection valid, without mutating the received state; the backend keeps
   * applying the explicit choice.
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
            min={key === "idle_minutes" ? 1 : key === "moe_cache_mib" ? 0 : undefined}
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
   * A feature switch, with what the node answered about it.
   *
   * `unsupported` disappears from the screen when it is not `alwaysVisible`: offering what the
   * runtime proved not to have is inviting a deployment that does not come up.
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
    const topologyWarning = capability?.reason.startsWith("control visible");
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

  const dryOn = (value.dry_multiplier ?? 0) > 0;
  const speculation = speculationModeOf(value);
  const speculationCapability = (mode: SpeculationMode) =>
    mode === "mtp" ? capabilities?.mtp : mode === "lookup" ? capabilities?.speculative_decoding : undefined;
  const chosenCapability = speculationCapability(speculation);
  // A choice whose support the node cannot yet tell stays available, but with a warning.
  const speculationAlert = chosenCapability?.state === "unknown"
    ? `${t("inspector.capabilityUnknown")} ${chosenCapability.reason}`
    : chosenCapability?.warning || undefined;

  /** The hidden-state precision between machines: the POM's, whatever engine runs the slices. */
  const hiddenStateField = () => (
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
  );

  /** A whole-model machine's part in a prefill/decode split (the POM's, whatever the engine): the deployment's, or
   *  one row per machine when the whole model runs on several of them. */
  const roleRow = (key: string, label: string, role: InferenceRole, onRole: (role: InferenceRole) => void) => (
    <InspectorRow key={key} label={label} tip={t("deploy.knob.inference_roleHint")}>
      {readOnly ? (
        <ReadValue>{t(role === "prefill" ? "deploy.knob.inference_rolePrefill" : "deploy.knob.inference_roleBoth")}</ReadValue>
      ) : (
        <Select
          aria-label={label}
          name={key}
          value={role === "shared" ? "both" : role}
          onChange={(event) => onRole(event.target.value as InferenceRole)}
          compact
        >
          <option value="both">{t("deploy.knob.inference_roleBoth")}</option>
          <option value="prefill">{t("deploy.knob.inference_rolePrefill")}</option>
        </Select>
      )}
    </InspectorRow>
  );
  const inferenceRoleField = () =>
    machineRoles && machineRoles.length > 1
      ? machineRoles.map((machine) =>
          roleRow(
            `inference_role_${machine.nodeId}`,
            `${t("deploy.knob.inference_role")} · ${machine.label}`,
            machine.role,
            (role) => onMachineRole?.(machine.nodeId, role),
          ))
      : roleRow("inference_role", t("deploy.knob.inference_role"), value.inference_role ?? "both", (role) =>
          set({ inference_role: role }));

  /** The prefill/decode split's threshold; empty takes the engine's default. */
  const prefillMinTokensField = () => (
    <InspectorRow key="prefill_min_tokens" label={t("deploy.knob.prefill_min_tokens")} tip={t("deploy.knob.prefill_min_tokensHint")}>
      {readOnly ? (
        <ReadValue>{value.prefill_min_tokens == null ? "-" : showNumber(value.prefill_min_tokens)}</ReadValue>
      ) : (
        <Input
          aria-label={t("deploy.knob.prefill_min_tokens")}
          name="prefill_min_tokens"
          type="number"
          min={0}
          placeholder="4096"
          value={value.prefill_min_tokens ?? ""}
          onChange={(event) =>
            set({
              prefill_min_tokens: event.target.value === "" ? null : Math.max(0, Math.round(Number(event.target.value) || 0)),
            })
          }
          compact
          className="text-right font-mono"
        />
      )}
    </InspectorRow>
  );

  // the projector's choice: a llama.cpp head and a plugin engine's (whose bridge takes the deployment's mmproj_path)
  const visionSection = vision ? (
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
            <InspectorRow label={t("deployStudio.visionModel")} tip={t("deployStudio.visionModelTip")}>
              <Select
                aria-label={t("deployStudio.visionProjector")}
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
  ) : null;

  if (inferenceService) {
    // The POM carries the hidden state between the plugin's stages, so its own
    // settings only exist when the slices cross machines. The prefill/decode
    // role belongs to a machine that runs the whole model: a deployment on one
    // machine, a whole model split into prefill and decode, or the prefill
    // machine a sliced deployment holds beside its stages (`machineRoles`).
    const declared = inferenceService.pomSettings ?? [];
    const perMachineRoles = (machineRoles?.length ?? 0) > 1;
    const pomSettings = declared.filter((setting) =>
      setting === "inference_role" || setting === "prefill_min_tokens"
        ? !showHiddenStateDtype || perMachineRoles
        : showHiddenStateDtype,
    );
    // The engine's own settings, and the ones the service places in the POM card (whatever the slices).
    const engineSettings = (inferenceService.settings ?? []).filter((setting) => setting.placement !== "pom");
    const pomCardSettings = (inferenceService.settings ?? []).filter((setting) => setting.placement === "pom");
    return (
      <>
        <Section title={t("inspector.sectionInferenceService", { name: inferenceService.name })} tone="iris">
          <p className="px-0.5 pb-1 text-[0.6rem] leading-relaxed text-muted">
            {t("inspector.inferenceServiceNote", { name: inferenceService.name })}
          </p>
          {numeric("n_ctx")}
          {numeric("max_sessions")}
          {toggle({
            key: "unload_if_idle",
            label: t("deploy.unloadIfIdle"),
            alwaysVisible: true,
            checked: value.unload_if_idle ?? false,
            onToggle: (next) => set({ unload_if_idle: next }),
            tip: t("deploy.unloadIfIdleHint"),
          })}
          {(value.unload_if_idle ?? false) && numeric("idle_minutes")}
          {engineSettings.length > 0 && (
            <EngineSettingsFields
              settings={engineSettings}
              values={value.engine_settings}
              onChange={(next) => set({ engine_settings: next })}
              readOnly={readOnly}
            />
          )}
          {inferenceService.takesDraft && sidecars && !readOnly && (
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
        {visionSection}
        {(
          <Section title={t("inspector.sectionPom")} tone="aqua" guide={DEPLOY_SETTINGS_GUIDES.pom}>
            {toggle({
              key: "yield_gpu_to_images",
              label: t("inspector.yieldGpuToImages"),
              alwaysVisible: true,
              checked: value.yield_gpu_to_images ?? false,
              onToggle: (next) => set({ yield_gpu_to_images: next }),
              tip: t("inspector.yieldGpuToImagesHint"),
            })}
            {pomSettings.includes("prefill_max_inflight_chunks") && numeric("prefill_max_inflight_chunks")}
            {pomSettings.includes("hidden_state_dtype") && hiddenStateField()}
            {pomSettings.includes("inference_role") && inferenceRoleField()}
            {pomSettings.includes("prefill_min_tokens") && prefillMinTokensField()}
            {pomCardSettings.length > 0 && (
              <EngineSettingsFields
                settings={pomCardSettings}
                values={value.engine_settings}
                onChange={(next) => set({ engine_settings: next })}
                readOnly={readOnly}
              />
            )}
          </Section>
        )}
      </>
    );
  }

  return (
    <>
      <Section title={t("inspector.sectionDefaults")} tone="iris" guide={DEPLOY_SETTINGS_GUIDES.defaults}>
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

        {/* llama.cpp's --cpu-moe and --moe-cache-mib, for a MoE larger than VRAM */}
        {toggle({
          key: "moe_experts_in_ram",
          label: t("inspector.moeExpertsInRam"),
          alwaysVisible: true,
          checked: value.moe_experts_in_ram ?? false,
          onToggle: (next) => set({ moe_experts_in_ram: next }),
          tip: t("deploy.moeExpertsInRamHint"),
        })}
        {numeric("moe_cache_mib")}

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
        <InspectorRow
          label={t("deploy.speculationMode")}
          tip={t("deploy.speculationModeHint")}
          alert={speculationAlert}
        >
          {readOnly ? (
            <ReadValue>{t(`deploy.speculationMode.${speculation}`)}</ReadValue>
          ) : (
            <Select
              aria-label={t("deploy.speculationMode")}
              name="speculation_mode"
              value={speculation}
              onChange={(event) => set(speculationPatch(event.target.value as SpeculationMode, value))}
              compact
            >
              {(["none", "mtp", "lookup"] as const)
                .filter((mode) => mode === "none" || mode === speculation || speculationCapability(mode)?.state !== "unsupported")
                .map((mode) => (
                  <option key={mode} value={mode}>{t(`deploy.speculationMode.${mode}`)}</option>
                ))}
            </Select>
          )}
        </InspectorRow>
        {speculation === "lookup" && capabilities?.speculative_decoding?.state !== "unsupported" && (
          <>
            {numeric("speculative_k")}
            {numeric("speculative_lookup_window")}
          </>
        )}
        {speculation === "mtp" && capabilities?.mtp?.state !== "unsupported" && (
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
        {speculation === "mtp" && sidecars && !readOnly && (
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

      {visionSection}

      <Section title={t("inspector.sectionPom")} tone="aqua" guide={DEPLOY_SETTINGS_GUIDES.pom}>
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
        {toggle({
          key: "adaptive_prefill_chunks",
          label: t("inspector.adaptivePrefillChunks"),
          alwaysVisible: true,
          checked: value.adaptive_prefill_chunks ?? true,
          onToggle: (next) => set({ adaptive_prefill_chunks: next }),
          tip: t("deploy.adaptivePrefillChunksHint"),
        })}
        {toggle({
          key: "yield_gpu_to_images",
          label: t("inspector.yieldGpuToImages"),
          alwaysVisible: true,
          checked: value.yield_gpu_to_images ?? false,
          onToggle: (next) => set({ yield_gpu_to_images: next }),
          tip: t("inspector.yieldGpuToImagesHint"),
        })}
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
        {showHiddenStateDtype && hiddenStateField()}
        {/* <BetaGroup>{...settings still being measured}</BetaGroup> */}
      </Section>

      {pomDevMode && <Section title={t("inspector.sectionPomDev")} tone="amber">
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
                {/* "Automático" continues to exist in the backend; it leaves here because
                    the interface now decides per llama.cpp. */}
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
