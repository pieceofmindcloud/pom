import { describe, expect, it } from "vitest";
import reference from "./locales.reference.json";
import { formatMessage, messageKeys, type MessageValues } from "./locales";

type ReferenceDump = {
  valuesByCase: Record<string, MessageValues>;
  records: Array<{
    locale: "en" | "pt-BR";
    key: string;
    case: string;
    result: string;
  }>;
};

const dump = reference as ReferenceDump;

// The dump is the frozen pre-ICU baseline. Keep intentional product-copy
// revisions here rather than rewriting the generated 1.8 MB single-line file.
const copyUpdates: Record<string, string> = {
  "en:plugins.state.active": "License active",
  "pt-BR:plugins.state.active": "Licença ativa",
  "en:deploy.knob.hidden_state_dtypeHint":
    "Controls bytes sent between inference peers. f16 remains the default; f32 preserves the in-memory activation values but doubles payload size. f8 and groupwise int4 are opt-in lossy formats and require every selected peer to advertise support.",
  "pt-BR:deploy.knob.hidden_state_dtypeHint":
    "Controla os bytes enviados entre peers de inferência. f16 continua como padrão; f32 preserva os valores das ativações em memória, mas dobra o tamanho do payload. f8 e int4 por grupo são formatos com perda opcionais e exigem suporte anunciado por todos os peers selecionados.",
};

describe("ICU locale equivalence", () => {
  it("keeps the CSV key order stable", () => {
    expect(messageKeys("en")).toEqual([...messageKeys("en")].sort());
  });

  it("keeps every key from the reference dump in both locales", () => {
    for (const locale of ["en", "pt-BR"] as const) {
      const expected = new Set(dump.records.filter((record) => record.locale === locale).map((record) => record.key));
      expect([...expected].filter((key) => !messageKeys(locale).includes(key))).toEqual([]);
    }
  });

  it("renders reference cases identically, with explicit product-copy revisions", () => {
    for (const record of dump.records) {
      const expected = copyUpdates[`${record.locale}:${record.key}`] ?? record.result;
      expect(
        formatMessage(record.locale, record.key, dump.valuesByCase[record.case]),
        `${record.locale} ${record.key} ${record.case}`,
      ).toBe(expected);
    }
  });
});
