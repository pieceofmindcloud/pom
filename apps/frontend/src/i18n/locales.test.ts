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
 * Um texto novo escrito só em inglês não quebra nada: `formatMessage` cai no
 * inglês quando a chave falta, e a tela em pt-BR passa a ter uma frase em
 * inglês no meio, sem erro nenhum no console. Este teste é o único lugar que
 * repara nisso.
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
 * Texto que vem do nó carrega nome de máquina. Traduzir a frase e perder o
 * nome seria pior que não traduzir; deixar em português numa tela em inglês
 * era o que acontecia com o desfecho de uma transferência.
 */
