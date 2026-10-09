/**
 * The settings an inference service's engine publishes, drawn generically.
 *
 * The node knows no engine: a plugin publishes its settings with the service
 * (`PluginService.settings`) and this draws one row per setting in the
 * interface's language. An empty field keeps the engine's own default, so a
 * value only reaches the deployment (`RuntimeParams.engine_settings`) when the
 * operator sets one. The plugin validates the values when it loads the model.
 */
import type { EngineSetting, EngineSettingValues } from "../lib/types";
import { useI18n } from "../i18n/provider";
import { InspectorRow, ReadValue } from "./DeploymentInspector";
import { Input, Select } from "./ui";

function inLocale(texts: Record<string, string> | undefined, locale: string): string | undefined {
  if (!texts) return undefined;
  return texts[locale] ?? texts[locale.split("-")[0]] ?? texts.en;
}

/** What a field's text becomes in the values; `undefined` removes the key. */
export function settingValue(setting: EngineSetting, text: string): EngineSettingValues[string] | undefined {
  const trimmed = text.trim();
  if (trimmed === "") return undefined;
  switch (setting.type) {
    case "number":
    case "integer": {
      const n = Number(trimmed);
      if (!Number.isFinite(n)) return undefined;
      return setting.type === "integer" ? Math.trunc(n) : n;
    }
    case "boolean":
      return trimmed === "true";
    case "list":
      return trimmed.split(",").map((name) => name.trim()).filter(Boolean);
    default:
      return trimmed;
  }
}

function settingText(value: EngineSettingValues[string] | undefined): string {
  if (value === undefined) return "";
  return Array.isArray(value) ? value.join(", ") : String(value);
}

export function EngineSettingsFields({
  settings,
  values,
  onChange,
  readOnly = false,
}: {
  settings: EngineSetting[];
  values: EngineSettingValues | undefined;
  onChange?: (next: EngineSettingValues) => void;
  readOnly?: boolean;
}) {
  const { t, locale } = useI18n();
  const current = values ?? {};
  const set = (key: string, value: EngineSettingValues[string] | undefined) => {
    const next = { ...current };
    if (value === undefined) delete next[key];
    else next[key] = value;
    onChange?.(next);
  };
  return (
    <>
      {settings.map((setting) => {
        const label = inLocale(setting.label, locale) ?? setting.key;
        const tip = inLocale(setting.description, locale);
        const text = settingText(current[setting.key]);
        if (readOnly) {
          return (
            <InspectorRow key={setting.key} label={label} tip={tip}>
              <ReadValue>{text || t("inspector.engineDefault")}</ReadValue>
            </InspectorRow>
          );
        }
        if (setting.type === "boolean" || setting.type === "enum") {
          const options = setting.type === "boolean" ? ["true", "false"] : setting.options ?? [];
          return (
            <InspectorRow key={setting.key} label={label} tip={tip}>
              <Select
                aria-label={label}
                name={`engine_${setting.key}`}
                value={text}
                onChange={(event) => set(setting.key, settingValue(setting, event.target.value))}
                compact
              >
                <option value="">{t("inspector.engineDefault")}</option>
                {options.map((option) => (
                  <option key={option} value={option}>
                    {setting.type === "boolean" ? t(option === "true" ? "inspector.engineOn" : "inspector.engineOff") : option}
                  </option>
                ))}
              </Select>
            </InspectorRow>
          );
        }
        const numeric = setting.type === "number" || setting.type === "integer";
        return (
          <InspectorRow key={setting.key} label={label} tip={tip}>
            <Input
              aria-label={label}
              name={`engine_${setting.key}`}
              type={numeric ? "number" : "text"}
              min={setting.min}
              max={setting.max}
              step={setting.type === "integer" ? 1 : "any"}
              placeholder={t("inspector.engineDefault")}
              defaultValue={text}
              onBlur={(event) => set(setting.key, settingValue(setting, event.target.value))}
              compact
              className={numeric ? "text-right font-mono" : undefined}
            />
          </InspectorRow>
        );
      })}
    </>
  );
}
