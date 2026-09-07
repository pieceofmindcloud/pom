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

describe("ICU locale equivalence", () => {
  it("keeps the CSV key order stable", () => {
    expect(messageKeys("en")).toEqual([...messageKeys("en")].sort());
  });

  it("keeps every key from the reference dump in both locales", () => {
    for (const locale of ["en", "pt-BR"] as const) {
      const expected = new Set(dump.records.filter((record) => record.locale === locale).map((record) => record.key));
      expect(new Set(messageKeys(locale))).toEqual(expected);
    }
  });

  it("renders every reference locale/key/value case identically", () => {
    for (const record of dump.records) {
      expect(
        formatMessage(record.locale, record.key, dump.valuesByCase[record.case]),
        `${record.locale} ${record.key} ${record.case}`,
      ).toBe(record.result);
    }
  });
});
