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
 * The set is the curated "living documentation": every AGENTS.md in the repo
 * (the root one, the per-folder ones from the documentation pass), the
 * subsystem READMEs under `docs/subsystems/`, plus `README.md` and
 * `apps/frontend/README.md`. `docs/superpowers/` (historical design specs) is
 * deliberately left out.
 */
import { mkdirSync, readFileSync, rmSync, writeFileSync, readdirSync, statSync } from "node:fs";
import { localeOf, parseGuide } from "./guides-md.mjs";
import { createHash } from "node:crypto";
import { basename, dirname, extname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url)); // apps/frontend/scripts
const repo = join(here, "..", "..", "..");
const out = join(here, "..", "src", "docs");

// `docs` is the output directory (`src/docs`): the frontend walk must not
// re-collect the files it embeds itself, or the copied AGENTS.md files
// become duplicates inside the list.
const SKIP_DIRS = new Set(["node_modules", "dist", "target", ".git", "venv", "docs"]);

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

/**
 * A folder's `AGENTS.md` is not a document *named* AGENTS.md - it is that
 * folder's overview. The reader picks a subject, never a file name, so the
 * manifest carries the role (`kind`) and the folder it covers (`scope`)
 * instead of leaving the screen to parse paths.
 */
function kindOf(p) {
  return p.endsWith("AGENTS.md") ? "overview" : "doc";
}

/** Folder an overview covers, with trailing slash; "" for the repository root. */
function scopeOf(p) {
  const dir = dirname(p);
  return dir === "." ? "" : dir + "/";
}

/**
 * Short human label for the navigation. Titles written for agents carry
 * prefixes that only repeat the tree ("Subsystem: ", "AGENTS.md - "); the
 * reader already knows which section they clicked.
 */
function labelOf(p, title) {
  if (kindOf(p) === "overview") {
    const scope = scopeOf(p);
    if (scope === "") return "";
    const parts = scope.split("/").filter(Boolean);
    return parts[parts.length - 1] + "/";
  }
  let label = title
    .replace(/^(Subsystem|Subsistema):\s*/i, "")
    .replace(/^AGENTS\.md\s*-\s*/i, "")
    .trim();
  // A subject, not a sentence: everything after the first ":", " - ", " — " or
  // "(" is the explanation, and the reader gets it from the page itself.
  const cut = label.search(/(:| - | — | \()/);
  if (cut > 0) {
    const head = label.slice(0, cut).trim();
    const tail = label.slice(cut).replace(/^(:| - | — )/, "").trim();
    // "CODAI-SERVER — interface web": the head is a product code, the tail is
    // the subject. Anything shouted in caps loses to what follows it.
    label = head === head.toUpperCase() && tail && !tail.startsWith("(") ? tail : head;
  }
  const first = label.split(/\s/)[0] ?? "";
  if (!/[-.]/.test(first)) label = label.charAt(0).toUpperCase() + label.slice(1);
  return label;
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
  const title = titleOf(p);
  entries.push({
    id,
    path: p,
    title,
    label: labelOf(p, title),
    kind: kindOf(p),
    scope: scopeOf(p),
    section,
  });
}

const SUBSYSTEMS_ORDER = [
  "operations.md",
  "p2p-network.md",
  "slicing.md",
  "deployment.md",
  "inference.md",
  "file-transfer.md",
  "http-api.md",
  "web-ui.md",
  "build-and-run.md",
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
  // subsystems follow the reading order of the map in docs/AGENTS.md, not the
  // alphabetical one: network first, build/run last.
  if (a.section === "subsystems") return subsystemsOrder(a.path) - subsystemsOrder(b.path);
  // root: AGENTS.md first; the rest by path.
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

// ── The guides written for humans ───────────────────────────────────────────
//
// `docs/guides/<group>/<id>[.pt-BR].md` and `docs/human/<folder>/HUMAN[.pt-BR].md`
// become `src/docs/guides.json` (the block lists `src/guides/Guide.tsx` renders,
// per locale) and the images they show are copied to `public/guide-images/`
// with a content hash in the name: the node serves every asset but
// `index.html` as immutable for a year, and a fixed name would pin an old
// screenshot. A public file also reaches a `vite build --watch` started before
// the image existed, which an `import.meta.glob` does not. The group is the
// folder under `docs/guides/`; every `HUMAN.md` belongs to "human".
const GROUPS = ["concepts", "screens", "settings", "human"];

function collectMd(dir, acc) {
  let list;
  try {
    list = readdirSync(dir, { withFileTypes: true });
  } catch {
    return acc;
  }
  for (const e of list) {
    const full = join(dir, e.name);
    if (e.isDirectory()) collectMd(full, acc);
    else if (e.isFile() && e.name.endsWith(".md")) acc.push(relative(repo, full));
  }
  return acc;
}

const guideFiles = [
  ...collectMd(join(repo, "docs", "guides"), []),
  ...collectMd(join(repo, "docs", "human"), []).filter((p) => /\/HUMAN(\.pt-BR)?\.md$/.test(p)),
].sort();

const parsed = { en: new Map(), "pt-BR": new Map() };
const imageOut = join(here, "..", "public", "guide-images");
rmSync(imageOut, { recursive: true, force: true });
rmSync(join(out, "guide-images"), { recursive: true, force: true });
const published = new Map();
function publish(image) {
  if (published.has(image)) return published.get(image);
  const bytes = readFileSync(join(repo, image));
  const hash = createHash("sha256").update(bytes).digest("hex").slice(0, 8);
  const ext = extname(image);
  const rel = image.replace(/^docs\/(guides|human)\//, "$1/");
  const name = join(dirname(rel), `${basename(rel, ext)}.${hash}${ext}`).replace(/\\/g, "/");
  mkdirSync(dirname(join(imageOut, name)), { recursive: true });
  writeFileSync(join(imageOut, name), bytes);
  published.set(image, `guide-images/${name}`);
  return published.get(image);
}
for (const file of guideFiles) {
  const locale = localeOf(file);
  const human = file.startsWith("docs/human/");
  const group = human ? "human" : file.split("/")[2];
  if (!GROUPS.includes(group)) throw new Error(`${file}: unknown guide group "${group}" (one of ${GROUPS.join(", ")})`);
  const { guide, order, images } = parseGuide(readFileSync(join(repo, file), "utf8"), file);
  if (parsed[locale].has(guide.id)) throw new Error(`${file}: duplicate guide id "${guide.id}"`);
  for (const block of guide.blocks) {
    if (block.kind === "image") block.src = publish(block.src);
  }
  parsed[locale].set(guide.id, { ...guide, group, order, source: file });
}

// A "go deeper" link must land on a document the reference tab embeds.
const embedded = new Set(entries.map((e) => e.id));
for (const map of Object.values(parsed)) {
  for (const guide of map.values()) {
    for (const block of guide.blocks) {
      if (block.kind === "deep" && !embedded.has(destId(block.doc))) {
        throw new Error(`${guide.source}: :::deep ${block.doc} is not an embedded reference document`);
      }
    }
  }
}

// A guide written only in English still shows in Portuguese, in English.
for (const [id, guide] of parsed.en) if (!parsed["pt-BR"].has(id)) parsed["pt-BR"].set(id, guide);
const byOrder = (a, b) => GROUPS.indexOf(a.group) - GROUPS.indexOf(b.group) || a.order - b.order || a.id.localeCompare(b.id);
const guides = {
  groups: GROUPS,
  locales: Object.fromEntries(
    Object.entries(parsed).map(([locale, map]) => [locale, [...map.values()].sort(byOrder)]),
  ),
};
writeFileSync(join(out, "guides.json"), JSON.stringify(guides, null, 2));

console.log(
  `build-docs: ${entries.length} docs embutidas em src/docs/, ${parsed.en.size} guias (${guideFiles.length} arquivos)`,
);
