import { webcrypto } from "node:crypto";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fingerprintFromTrustDocument, sha256 } from "./trustFingerprint";

const hex = (bytes: Uint8Array) => Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
const utf8 = (text: string) => new TextEncoder().encode(text);

describe("sha256", () => {
  // Vetores da FIPS 180-4, inclusive a mensagem de dois blocos.
  it("matches the test vectors of the standard", () => {
    expect(hex(sha256(utf8("")))).toBe("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    expect(hex(sha256(utf8("abc")))).toBe("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    expect(hex(sha256(utf8("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")))).toBe(
      "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
    );
  });
});

describe("fingerprintFromTrustDocument", () => {
  afterEach(() => vi.unstubAllGlobals());

  const document = JSON.stringify({ anchor: { principal: { pubkey: "ab".repeat(32) } } });

  /**
   * O assistente de primeiro acesso é aberto por HTTP no IP da máquina, onde o
   * navegador não oferece `crypto.subtle`. A impressão digital tem de sair igual.
   */
  it("computes the same fingerprint without crypto.subtle", async () => {
    vi.stubGlobal("crypto", webcrypto);
    const withSubtle = await fingerprintFromTrustDocument(document);

    vi.stubGlobal("crypto", {});
    const withoutSubtle = await fingerprintFromTrustDocument(document);

    expect(withSubtle).toMatch(/^[0-9a-f]{64}$/);
    expect(withoutSubtle).toBe(withSubtle);
  });

  it("returns null for what is not a trust document", async () => {
    expect(await fingerprintFromTrustDocument("não é json")).toBeNull();
    expect(await fingerprintFromTrustDocument(JSON.stringify({ anchor: {} }))).toBeNull();
  });
});
