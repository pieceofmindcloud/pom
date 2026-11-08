// Run a published ComfyUI workflow from the Chat: one field per parameter
// (text, number with its bounds, switch, select, image picker) prefilled with
// the defaults. The plugin coerces and clamps again; this form only helps.
import { useEffect, useId, useState } from "react";
import { useI18n } from "../i18n/provider";
import type { ChatImageWorkflow, ChatWorkflowParam } from "../lib/types";
import { Modal } from "./Confirm";
import { Button } from "./ui";

/** The largest image the plugin takes. */
const MAX_IMAGE_BYTES = 16 * 1024 * 1024;

export type WorkflowValues = Record<string, string | number | boolean>;

/** The defaults of a workflow's parameters (an image has none). */
export function initialValues(params: ChatWorkflowParam[]): WorkflowValues {
  const values: WorkflowValues = {};
  for (const param of params) {
    if (param.type === "image") continue;
    if (param.default != null) values[param.key] = param.default;
    else if (param.type === "boolean") values[param.key] = false;
    else if (param.type === "choice" && param.options?.length) values[param.key] = param.options[0];
  }
  return values;
}

const control =
  "w-full rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[0.82rem] text-fg focus:border-iris/60 focus:outline-none";

function ParamField({
  param,
  value,
  onChange,
}: {
  param: ChatWorkflowParam;
  value: string | number | boolean | undefined;
  onChange: (value: string | number | boolean | undefined) => void;
}) {
  const { t } = useI18n();
  const id = useId();
  const [imageError, setImageError] = useState("");
  const label = (
    <label htmlFor={id} className="mb-1 block text-[0.74rem] font-medium text-muted">
      {param.label}
    </label>
  );
  const hint = param.description ? <p className="mt-1 text-[0.68rem] text-faint">{param.description}</p> : null;
  if (param.type === "boolean") {
    return (
      <div>
        <div className="flex items-center gap-2">
          <input id={id} type="checkbox" checked={value === true} onChange={(event) => onChange(event.target.checked)} />
          <label htmlFor={id} className="text-[0.8rem] text-fg">{param.label}</label>
        </div>
        {hint}
      </div>
    );
  }
  if (param.type === "choice") {
    return (
      <div>
        {label}
        <select id={id} className={control} value={String(value ?? "")} onChange={(event) => {
          const picked = (param.options ?? []).find((option) => String(option) === event.target.value);
          onChange(picked);
        }}>
          {(param.options ?? []).map((option) => <option key={String(option)} value={String(option)}>{String(option)}</option>)}
        </select>
        {hint}
      </div>
    );
  }
  if (param.type === "number" || param.type === "integer") {
    return (
      <div>
        {label}
        <input
          id={id}
          type="number"
          className={control}
          min={param.min}
          max={param.max}
          step={param.type === "integer" ? 1 : "any"}
          value={value == null ? "" : String(value)}
          onChange={(event) => onChange(event.target.value === "" ? undefined : Number(event.target.value))}
        />
        {hint}
      </div>
    );
  }
  if (param.type === "image") {
    return (
      <div>
        {label}
        <input
          id={id}
          type="file"
          accept="image/png,image/jpeg"
          className="block w-full text-[0.76rem] text-muted"
          onChange={(event) => {
            const file = event.target.files?.[0];
            setImageError("");
            if (!file) return onChange(undefined);
            if (!["image/png", "image/jpeg"].includes(file.type) || file.size > MAX_IMAGE_BYTES) {
              setImageError(t("chatImage.formImageRefused"));
              return onChange(undefined);
            }
            const reader = new FileReader();
            reader.onload = () => onChange(typeof reader.result === "string" ? reader.result : undefined);
            reader.readAsDataURL(file);
          }}
        />
        {imageError && <p className="mt-1 text-[0.68rem] text-rose">{imageError}</p>}
        {hint}
      </div>
    );
  }
  return (
    <div>
      {label}
      <textarea id={id} rows={3} className={control} value={String(value ?? "")} onChange={(event) => onChange(event.target.value)} />
      {hint}
    </div>
  );
}

export function WorkflowForm({
  workflow,
  open,
  onClose,
  onRun,
}: {
  workflow: ChatImageWorkflow;
  open: boolean;
  onClose: () => void;
  onRun: (values: WorkflowValues) => void;
}) {
  const { t } = useI18n();
  const params = workflow.params ?? [];
  const [values, setValues] = useState<WorkflowValues>(() => initialValues(params));
  useEffect(() => {
    if (open) setValues(initialValues(workflow.params ?? []));
  }, [open, workflow]);
  return (
    <Modal
      open={open}
      title={workflow.name}
      size="lg"
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>{t("common.cancel")}</Button>
          <Button onClick={() => onRun(values)}>{t("chatImage.formRun")}</Button>
        </>
      }
    >
      {workflow.description && <p className="mb-3 text-[0.8rem] text-muted">{workflow.description}</p>}
      <div className="space-y-3">
        {params.map((param) => (
          <ParamField
            key={param.key}
            param={param}
            value={values[param.key]}
            onChange={(value) => setValues((current) => {
              const next = { ...current };
              if (value === undefined) delete next[param.key];
              else next[param.key] = value;
              return next;
            })}
          />
        ))}
      </div>
    </Modal>
  );
}
