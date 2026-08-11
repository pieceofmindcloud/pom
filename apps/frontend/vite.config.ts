import { fileURLToPath, URL } from "node:url";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

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
  plugins: [react()],
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
   * the directory means a stray `web-ui/.vite/` can no longer appear, so if the
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
