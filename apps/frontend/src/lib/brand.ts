/**
 * The product identity, on the screen side.
 *
 * Same source as the binary: `brand.json` at the repository root, copied in by
 * `scripts/build-brand.mjs` before every dev/build/test run. Nothing here is
 * typed by hand, so the screen and the node can never disagree about what the
 * product is called.
 *
 * `storageKey` is here and not next to each caller because every persisted key
 * carries the product name (`pom.theme`, `pom.locale`, ...). Deriving them from
 * the slug is what makes a rename one edit instead of a hunt - and it is also
 * why a rename logs everyone out of their local preferences, which is the
 * intended, honest consequence: they are a different product's keys now.
 */
import manifest from "../brand/brand.json";

export type Brand = {
  name: string;
  tagline: string;
  description: string;
  slug: string;
  /** Lockup por tema: `logo.light` no tema claro, `logo.dark` no escuro. */
  logo: Record<"light" | "dark", string>;
  mark: string | null;
  icon: string | null;
  favicon: string | null;
  icons: Record<string, string>;
  /** Bandeira por locale, para o seletor de idioma. */
  flags: Record<string, string>;
};

export const brand: Brand = manifest as Brand;

/** `localStorage` key for this product: `storageKey("theme")` -> `pom.theme`. */
export function storageKey(name: string): string {
  return `${brand.slug}.${name}`;
}
