const HEX = "0123456789abcdef";

/**
 * A compact UUID for client-side correlation IDs.
 *
 * `randomUUID` is restricted to secure contexts, while `getRandomValues` is
 * available to the HTTP LAN deployment too. The final fallback keeps old
 * browsers usable for non-secret IDs when neither Web Crypto method exists.
 */
export function randomId(): string {
  const webCrypto = globalThis.crypto;
  if (typeof webCrypto?.randomUUID === "function") {
    return webCrypto.randomUUID().replaceAll("-", "");
  }

  const bytes = new Uint8Array(16);
  if (typeof webCrypto?.getRandomValues === "function") {
    webCrypto.getRandomValues(bytes);
  } else {
    for (let index = 0; index < bytes.length; index += 1) {
      bytes[index] = Math.floor(Math.random() * 256);
    }
  }
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  return Array.from(bytes, (byte) => HEX[byte >> 4] + HEX[byte & 0x0f]).join("");
}
