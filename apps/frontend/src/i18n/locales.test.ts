import { describe, expect, it } from "vitest";
import {
  formatMessage,
  localizeServerText,
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
describe("cobertura das traduções", () => {
  it("define exatamente as mesmas chaves nos dois idiomas", () => {
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
});

/**
 * Texto que vem do nó carrega nome de máquina. Traduzir a frase e perder o
 * nome seria pior que não traduzir; deixar em português numa tela em inglês
 * era o que acontecia com o desfecho de uma transferência.
 */
describe("localizeServerText", () => {
  it("traduz o motivo mantendo o nome da máquina", () => {
    expect(localizeServerText("en", "recusado por ksia")).toBe("refused by ksia");
    expect(localizeServerText("en", "cancelado por atena")).toBe("cancelled by atena");
  });

  it("em pt-BR o texto do nó passa intacto", () => {
    expect(localizeServerText("pt-BR", "recusado por ksia")).toBe("recusado por ksia");
  });

  it("traduz a etapa que o nó reporta enquanto pede o digest", () => {
    expect(localizeServerText("en", "conferindo o arquivo na origem")).toBe(
      "checking the file at the source",
    );
  });
});
