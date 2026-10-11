import { describe, expect, it, vi } from "vitest";
import {
  formatMessage,
  messageKeys,
  resolveLocale,
  SUPPORTED_LOCALES,
} from "./locales";

describe("resolveLocale", () => {
  it("uses a supported explicit preference before browser hints", () => {
    expect(resolveLocale("en", ["pt-BR"])).toBe("en");
  });

  it("maps Portuguese browser hints to Brazilian Portuguese", () => {
    expect(resolveLocale(null, ["pt-PT", "en-US"])).toBe("pt-BR");
  });

  it("falls back to English for unavailable or unsupported browser locales", () => {
    expect(resolveLocale("fr-FR", [])).toBe("en");
  });
});

/**
 * A new text written only in English breaks nothing: `formatMessage` falls back to
 * English when the key is missing, and the pt-BR screen ends up with a sentence in
 * English in the middle of it, with no error in the console at all. This test is the only place that
 * notices that.
 */
describe("translation coverage", () => {
  it("defines exactly the same keys in both languages", () => {
    const [first, ...rest] = SUPPORTED_LOCALES;
    const reference = new Set(messageKeys(first));
    for (const locale of rest) {
      const keys = new Set(messageKeys(locale));
      expect([...reference].filter((k) => !keys.has(k)).sort()).toEqual([]);
      expect([...keys].filter((k) => !reference.has(k)).sort()).toEqual([]);
    }
  });
});

describe("formatMessage", () => {
  it("uses the locale's plural form instead of concatenating a sentence", () => {
    expect(formatMessage("en", "cluster.peerCount", { count: 1 })).toBe("1 node");
    expect(formatMessage("en", "cluster.peerCount", { count: 2 })).toBe("2 nodes");
    expect(formatMessage("pt-BR", "cluster.peerCount", { count: 1 })).toBe("1 nó");
    expect(formatMessage("pt-BR", "cluster.peerCount", { count: 2 })).toBe("2 nós");
  });

  it("names the Hugging Face search entry and screen explicitly", () => {
    expect(formatMessage("pt-BR", "nav.remoteModels")).toBe("Encontrar Modelos HF");
    expect(formatMessage("pt-BR", "remoteModels.title")).toBe(
      "Encontrar modelos no Hugging Face",
    );
  });

  it("returns a safe fallback instead of throwing when an ICU value is missing", () => {
    const warning = vi.spyOn(console, "warn").mockImplementation(() => {});

    expect(formatMessage("en", "keys.revoke")).toBe("Revoke {label}");
    expect(warning).toHaveBeenCalledWith(
      expect.stringContaining('Could not format locale message "keys.revoke"'),
      expect.anything(),
    );

    warning.mockRestore();
  });
});

/**
 * Text that comes from the node carries a machine name. Translating the sentence and losing the
 * name would be worse than not translating; leaving it in Portuguese on an English screen
 * was what happened with the outcome of a transfer.
 */
