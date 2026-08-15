/**
 * Embeeds the repository's living documentation into the UI build.
 *
 * The panel is read from disk at runtime (`config::ui_dist_dir()`), so the
 * only content that survives on another machine is what the bundle carries.
 * This script copies the folder AGENTS.md files, the subsystem READMEs and the
 * root README into `src/docs/` (gitignored) at build time and writes
 * `manifest.json` with the navigation metadata. `npm run build` (and `npm run
 * dev`) runs it first; the Documentation screen loads the markdown through
 * `import.meta.glob` and the metadata through the manifest.
 *
 * The set is the curated "documentação viva": every AGENTS.md in the repo
 * (the root one, the per-folder ones from the documentation pass), the
 * subsystem READMEs under `docs/subsystems/`, plus `README.md` and
 * `apps/frontend/README.md`. `docs/superpowers/` (historical design specs) and
 * `old/` are deliberately left out.
 */
import { mkdirSync, readFileSync, writeFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url)); // apps/frontend/scripts
const repo = join(here, "..", "..", "..");
const out = join(here, "..", "src", "docs");

// `docs` é o diretório de saída (`src/docs`): a caminhada do frontend não pode
// re-coletar os arquivos que ela própria embute, ou os AGENTS.md copiados
// viram duplicatas dentro da lista.
const SKIP_DIRS = new Set(["node_modules", "dist", "target", ".git", "old", "venv", "docs"]);

/** Walks a subtree collecting every file whose name is `name`. */
function collect(root, dir, name, acc) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const e of entries) {
    if (e.isDirectory()) {
      if (SKIP_DIRS.has(e.name)) continue;
      collect(root, join(dir, e.name), name, acc);
    } else if (e.isFile() && e.name === name) {
      acc.push(relative(root, join(dir, e.name)));
    }
  }
}

const wanted = new Set();
const push = (p) => wanted.add(p);

push("AGENTS.md");
push("README.md");
push("docs/AGENTS.md");
push("scripts/AGENTS.md");

const subsystem = [];
for (const f of readdirSync(join(repo, "docs", "subsystems"))) {
  if (f.endsWith(".md")) subsystem.push(join("docs", "subsystems", f));
}
subsystem.sort();
for (const s of subsystem) push(s);

const crates = [];
collect(repo, join(repo, "crates"), "AGENTS.md", crates);
crates.sort();
for (const c of crates) push(c);

const frontend = [];
collect(repo, join(repo, "apps", "frontend"), "AGENTS.md", frontend);
frontend.push("apps/frontend/README.md");
frontend.sort();
for (const f of frontend) push(f);

/** Section id for a repo-relative path; `null` means "not part of the docs". */
function sectionOf(p) {
  if (p === "AGENTS.md" || p === "README.md") return "root";
  if (p === "docs/AGENTS.md") return "docs";
  if (p.startsWith("docs/subsystems/")) return "subsystems";
  if (p.startsWith("crates/")) return "crates";
  if (p.startsWith("apps/frontend/")) return "frontend";
  if (p === "scripts/AGENTS.md") return "scripts";
  return null;
}

/** Where the doc lives inside `src/docs`, with the repo path kept as the id. */
function destId(p) {
  if (p.startsWith("apps/frontend/")) return p.replace(/^apps\/frontend\//, "frontend/");
  return p;
}

/** First `# ` heading as the title; falls back to the file name. */
function titleOf(p) {
  const text = readFileSync(join(repo, p), "utf8");
  const first = text.split("\n").find((l) => l.startsWith("# ") && l.length > 2);
  if (first) return first.replace(/^#\s+/, "").trim();
  return p.split("/").pop() ?? p;
}

const entries = [];
for (const p of [...wanted].sort()) {
  const section = sectionOf(p);
  if (!section) continue;
  const id = destId(p);
  const body = readFileSync(join(repo, p), "utf8");
  const dest = join(out, id);
  mkdirSync(dirname(dest), { recursive: true });
  writeFileSync(dest, body);
  entries.push({ id, path: p, title: titleOf(p), section });
}

const SUBSYSTEMS_ORDER = [
  "rede-p2p.md",
  "fatiamento.md",
  "implantacao.md",
  "inferencia.md",
  "transferencia.md",
  "api-http.md",
  "web-ui.md",
  "build-execucao.md",
];
function subsystemsOrder(p) {
  const file = p.split("/").pop();
  const i = SUBSYSTEMS_ORDER.indexOf(file);
  return i === -1 ? SUBSYSTEMS_ORDER.length : i;
}

const SECTION_ORDER = ["root", "docs", "subsystems", "crates", "frontend", "scripts"];
entries.sort((a, b) => {
  const sa = SECTION_ORDER.indexOf(a.section);
  const sb = SECTION_ORDER.indexOf(b.section);
  if (sa !== sb) return sa - sb;
  // subsistemas seguem a ordem de leitura do mapa em docs/AGENTS.md, não a
  // alfabética: rede primeiro, build/execução por último.
  if (a.section === "subsystems") return subsystemsOrder(a.path) - subsystemsOrder(b.path);
  // raiz: AGENTS.md primeiro; o resto por caminho.
  if (a.section === "root" && a.path !== b.path) return a.path === "AGENTS.md" ? -1 : 1;
  return a.path.localeCompare(b.path);
});

entries.forEach((e, order) => (e.order = order));

const manifest = {
  generatedBy: "apps/frontend/scripts/build-docs.mjs",
  sections: SECTION_ORDER,
  entries,
};
writeFileSync(join(out, "manifest.json"), JSON.stringify(manifest, null, 2));

console.log(`build-docs: ${entries.length} docs embutidas em src/docs/`);
