import { readFileSync } from "node:fs";
import { fileURLToPath, URL } from "node:url";
import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";

/**
 * `index.html` is the one file the app cannot import from, so the product
 * identity is injected into it at build time from the same `brand.json` the
 * binary embeds. Placeholders (`%BRAND_NAME%`, `%BRAND_SLUG%`, ...) keep the
 * title, the icons and the pre-paint storage keys in one source - renaming the
 * product must never leave the browser tab announcing the old name.
 *
 * `scripts/build-brand.mjs` runs first (npm pre-hooks) and is what copies the
 * images into `public/brand/`.
 */
function brandHtml(): Plugin {
  // O manifesto **gerado** (`src/brand/brand.json`), não o da raiz: é ele que
  // tem as URLs com hash de conteúdo que o navegador precisa para não segurar
  // um ícone velho por um ano.
  const brand = JSON.parse(
    readFileSync(fileURLToPath(new URL("./src/brand/brand.json", import.meta.url)), "utf8"),
  );
  const icons: Record<string, string> = brand.icons ?? {};
  const asset = (p?: string) => p ?? "";
  const values: Record<string, string> = {
    "%BRAND_NAME%": brand.name,
    "%BRAND_TAGLINE%": brand.tagline,
    "%BRAND_SLUG%": brand.slug,
    "%BRAND_FAVICON%": asset(brand.favicon),
    "%BRAND_ICON%": asset(icons["512"] ?? brand.mark),
    "%BRAND_APPLE_ICON%": asset(icons["180"] ?? brand.mark),
  };
  return {
    name: "brand-html",
    transformIndexHtml(html) {
      return Object.entries(values).reduce(
        (out, [token, value]) => out.split(token).join(value),
        html,
      );
    },
  };
}

/**
 * Where the browser's API calls are forwarded.
 *
 * A node serves the API and the UI on the same port, so in production the app
 * is same-origin and every path below is relative. In development the Vite
 * server sits on 5199 and forwards to a real node — point it wherever the node
 * you want to drive is listening:
 *
 *   VITE_API_TARGET=http://192.168.100.138:8090 npm run dev
 *
 * There is no sample backend: every screen reads a real node. A node with
 * nothing configured yet is a legitimate thing to develop against — it answers
 * the same routes with empty lists, which is exactly the state the screens have
 * to handle. Start one with `--no-discovery` so it stays out of the cluster.
 */
const target = process.env.VITE_API_TARGET ?? "http://127.0.0.1:8080";

/** Every path the node owns. Anything else is served by Vite itself. */
const proxied = ["/api", "/v1", "/admin", "/peers", "/health", "/generate"];

export default defineConfig({
  plugins: [react(), brandHtml()],
  /**
   * Pinned, and not left to the default, because the default has a silent
   * fallback that hid a real failure for a while.
   *
   * Vite computes it as `cacheDir ?? (pkgDir ? pkgDir/node_modules/.vite :
   * root/.vite)`. The last branch only happens when Vite could not find this
   * package's `package.json` — the same loss of project context that leaves it
   * running with no config at all: no PostCSS (so `@tailwind` reaches the
   * browser as a literal and the page renders unstyled) and no `server.proxy`
   * (so `/api/*` answers with index.html instead of JSON). Nothing errors; the
   * server keeps replying 200.
   *
   * That state is reachable by editing nothing: a `git rebase` or branch switch
   * rewrites `vite.config.ts` under a live dev server, Vite restarts itself on
   * the spot, and the restart can land while the tree is mid-rewrite. Naming
   * the directory means a stray `apps/frontend/.vite/` can no longer appear, so if the
   * page ever loses its styling again the cure is the same as ever — restart
   * the dev server — and the cache location is at least not a second variable.
   */
  cacheDir: "node_modules/.vite",
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  server: {
    port: 5199,
    strictPort: true,
    proxy: Object.fromEntries(
      proxied.map((path) => [
        path,
        { target, changeOrigin: true, ws: false, timeout: 600_000 },
      ]),
    ),
  },
  build: { outDir: "dist", sourcemap: false },
});
