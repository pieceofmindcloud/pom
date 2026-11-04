/**
 * Publishes what the Chat needs to read a PDF on the page: the pdf.js worker and
 * its data, and the OCR worker, engine and languages.
 *
 * They are loaded at run time by URL, so they are not part of the Vite graph,
 * and they come from this node because a scanned contract must not depend on,
 * or be announced to, a CDN. `webapp.rs` serves everything but `index.html` as
 * immutable for a year, so the folder carries the versions: a new version is a
 * new address. Both destinations are gitignored, like the brand assets.
 */
import { cpSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url)); // apps/frontend/scripts
const frontend = join(here, "..");
const modules = join(frontend, "node_modules");
const version = (name) => JSON.parse(readFileSync(join(modules, name, "package.json"), "utf8")).version;

const key = `pdf-${version("pdfjs-dist")}-ocr-${version("tesseract.js")}`;
const published = join(frontend, "public", "document-reader");
const destination = join(published, key);
rmSync(published, { recursive: true, force: true });

const pdfjs = join(modules, "pdfjs-dist");
mkdirSync(join(destination, "pdf"), { recursive: true });
cpSync(join(pdfjs, "legacy/build/pdf.worker.min.mjs"), join(destination, "pdf/pdf.worker.min.mjs"));
for (const data of ["cmaps", "standard_fonts", "wasm"]) {
  cpSync(join(pdfjs, data), join(destination, "pdf", data), { recursive: true });
}

// Only the LSTM engines the reader asks for; the worker picks one by what the
// browser's WebAssembly supports.
mkdirSync(join(destination, "ocr/core"), { recursive: true });
cpSync(join(modules, "tesseract.js/dist/worker.min.js"), join(destination, "ocr/worker.min.js"));
for (const file of readdirSync(join(modules, "tesseract.js-core"))) {
  if (/^tesseract-core(-simd|-relaxedsimd)?-lstm\.wasm\.js$/.test(file)) {
    cpSync(join(modules, "tesseract.js-core", file), join(destination, "ocr/core", file));
  }
}
mkdirSync(join(destination, "ocr/lang"), { recursive: true });
for (const language of ["por", "eng"]) {
  cpSync(
    join(modules, `@tesseract.js-data/${language}/4.0.0/${language}.traineddata.gz`),
    join(destination, `ocr/lang/${language}.traineddata.gz`),
  );
}

const manifest = join(frontend, "src", "brand", "document-reader.json");
mkdirSync(dirname(manifest), { recursive: true });
writeFileSync(manifest, `${JSON.stringify({ base: `document-reader/${key}/` }, null, 2)}\n`);
