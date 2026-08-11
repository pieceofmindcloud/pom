import { describe, expect, it } from "vitest";
import { formatMessage, resolveLocale } from "./locales";

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

describe("formatMessage", () => {
  it("uses the locale's plural form instead of concatenating a sentence", () => {
    expect(formatMessage("en", "cluster.peerCount", { count: 1 })).toBe("1 node");
    expect(formatMessage("en", "cluster.peerCount", { count: 2 })).toBe("2 nodes");
    expect(formatMessage("pt-BR", "cluster.peerCount", { count: 1 })).toBe("1 nó");
    expect(formatMessage("pt-BR", "cluster.peerCount", { count: 2 })).toBe("2 nós");
  });
});
