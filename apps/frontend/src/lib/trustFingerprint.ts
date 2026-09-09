/**
 * Calculate the same public anchor fingerprint used by the Rust trust module.
 *
 * The document is the only value pasted into the form. The operator confirms
 * this calculated value against a separate message from the administrator;
 * the server still parses the complete chain and verifies it again.
 */
export async function fingerprintFromTrustDocument(document: string): Promise<string | null> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(document);
  } catch {
    return null;
  }

  const publicKey = (parsed as { anchor?: { principal?: { pubkey?: unknown } } })?.anchor?.principal?.pubkey;
  if (typeof publicKey !== "string" || !publicKey.trim()) return null;

  const subtle = globalThis.crypto?.subtle;
  if (!subtle) return null;
  const bytes = new TextEncoder().encode(publicKey);
  const digest = await subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function groupFingerprint(fingerprint: string): string {
  return (fingerprint.match(/.{1,8}/g) ?? [fingerprint]).join(" ");
}
