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
 * `npm run mock` starts a read-only sample backend on 5198 for working on the
 * interface without a cluster.
 */
const target = process.env.VITE_API_TARGET ?? "http://127.0.0.1:8080";

/** Every path the node owns. Anything else is served by Vite itself. */
const proxied = ["/api", "/v1", "/admin", "/peers", "/health", "/generate"];

export default defineConfig({
  plugins: [react()],
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
