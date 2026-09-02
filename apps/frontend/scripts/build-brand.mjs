/**
 * Brings the product identity into the bundle.
 *
 * `brand.json` at the repository root is the single source of truth for the
 * name, the tagline and the logo files (the Rust binary embeds the same file
 * with `include_str!`). Vite cannot import from outside its root, so this
 * script copies it - and the images it points at - into the frontend before
 * every dev/build/test run, the same way `build-docs.mjs` copies the living
 * documentation.
 *
 * Both destinations are gitignored: they are build output, and a committed copy
 * is a copy that drifts. Renaming the product stays one edit to `brand.json`.
 */
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, basename, extname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url)); // apps/frontend/scripts
const repo = join(here, "..", "..", "..");
const brandPath = join(repo, "brand.json");
const brand = JSON.parse(readFileSync(brandPath, "utf8"));

// 1. The manifest the app imports.
const srcDir = join(here, "..", "src", "brand");
mkdirSync(srcDir, { recursive: true });

// 2. The images, served as static assets (referenced by URL, not bundled: the
//    logo is 1.5 MB and only the setup screen shows it at full size).
const publicDir = join(here, "..", "public", "brand");
// Recriado a cada execução: com hash no nome, uma troca de logo deixaria o
// arquivo anterior para trás e o `dist` acumularia marcas velhas.
rmSync(publicDir, { recursive: true, force: true });
mkdirSync(publicDir, { recursive: true });

/**
 * Copies a repo-relative asset into `public/brand/` **with a content hash in
 * the name** and returns its URL.
 *
 * The hash is not decoration: the node serves everything except `index.html`
 * as `immutable, max-age=1y` (`crates/node/src/backend/webapp.rs`), on the
 * premise that asset names carry a content hash. Files under `public/` are
 * copied verbatim by Vite, so a fixed name would pin the old logo in every
 * browser for a year - which is exactly what happened the first time this ran.
 */
function publish(relPath) {
  if (!relPath) return null;
  const bytes = readFileSync(join(repo, relPath));
  const hash = createHash("sha256").update(bytes).digest("hex").slice(0, 8);
  const ext = extname(relPath);
  const name = `${basename(relPath, ext)}.${hash}${ext}`;
  writeFileSync(join(publicDir, name), bytes);
  return `/admin-ui/brand/${name}`;
}

const manifest = {
  name: brand.name,
  tagline: brand.tagline,
  description: brand.description ?? "",
  slug: brand.slug,
  // Duas artes do lockup, por tema. Hoje o mesmo arquivo nos dois; quando a
  // versão para fundo escuro existir, é só trocar `logo.dark` no brand.json.
  logo: Object.fromEntries(
    Object.entries(brand.logo ?? {}).map(([theme, p]) => [theme, publish(p)]),
  ),
  mark: publish(brand.mark),
  icon: publish(brand.icon),
  favicon: publish(brand.favicon),
  icons: Object.fromEntries(
    Object.entries(brand.icons ?? {}).map(([size, p]) => [size, publish(p)]),
  ),
  // Bandeiras do seletor de idioma: um ícone diz o idioma sem depender de o
  // leitor saber o que "PT-BR" quer dizer.
  flags: Object.fromEntries(
    Object.entries(brand.flags ?? {}).map(([locale, p]) => [locale, publish(p)]),
  ),
};

writeFileSync(join(srcDir, "brand.json"), JSON.stringify(manifest, null, 2));
console.log(`build-brand: ${manifest.name} (${manifest.slug}) embutida em src/brand/`);
