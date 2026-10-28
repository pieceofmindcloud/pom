/**
 * Reads one guide written in markdown (`docs/guides/**`, `docs/human/**`) into
 * the block list `src/guides/Guide.tsx` renders.
 *
 * A guide is a file per language: `<name>.md` is English, `<name>.pt-BR.md` is
 * Brazilian Portuguese. The front matter carries the navigation (`id`, `label`,
 * `summary`, `title`, `order`); the body is plain markdown plus fenced
 * directives for the blocks markdown has no word for:
 *
 *   :::lead            the opening sentence, set larger
 *   :::note Title      an information callout
 *   :::warn Title      a warning callout
 *   :::steps           numbered timeline; each item opens with `### Title`
 *   :::cards           peer concepts; each item opens with `### Title`
 *   :::faq             questions that open on demand; each opens with `### Question`
 *   :::stats           a list of `- **value** what it measures`
 *   :::checklist Title a list of `- item`
 *   :::diagram <id>    an animated diagram from `src/guides/diagrams.tsx`; the body is the caption
 *   :::deep <doc>      a link into the reference; the body is the link text
 *   :::video <url>     an embedded YouTube video; the body is the caption
 *
 * Outside a directive: `## Heading`, paragraphs, GFM tables, fenced code and a
 * standalone image line `![caption](relative/path.webp)`, whose path is
 * resolved against the guide's own folder.
 *
 * No dependency: the build runs it with Node's standard library only, and so
 * can the website's copy of the guides.
 */
import { dirname, join, normalize } from "node:path";

const DIRECTIVES = new Set([
  "lead",
  "note",
  "warn",
  "steps",
  "cards",
  "faq",
  "stats",
  "checklist",
  "diagram",
  "deep",
  "video",
]);

/** The video id of a YouTube watch, short or embed address. */
function youtubeId(url) {
  const m = url.match(/(?:youtube\.com\/(?:watch\?(?:.*&)?v=|embed\/|shorts\/)|youtu\.be\/)([A-Za-z0-9_-]{11})/);
  return m ? m[1] : null;
}

/** `key: value` lines between two `---`; values are kept as strings except `order`. */
function frontMatter(text) {
  if (!text.startsWith("---\n")) return { meta: {}, body: text };
  const end = text.indexOf("\n---", 4);
  if (end === -1) throw new Error("front matter is not closed");
  const meta = {};
  for (const line of text.slice(4, end).split("\n")) {
    const m = line.match(/^([A-Za-z_]+):\s*(.*)$/);
    if (!m) continue;
    let value = m[2].trim();
    if (/^".*"$/.test(value)) value = value.slice(1, -1);
    meta[m[1]] = m[1] === "order" ? Number(value) : value;
  }
  return { meta, body: text.slice(end + 4).replace(/^\n/, "") };
}

/** Joins wrapped lines into one string, the way markdown joins a paragraph. */
const join1 = (lines) => lines.map((l) => l.trim()).filter(Boolean).join(" ");

/** Splits a directive body into `### Title` items. */
function titledItems(lines, directive) {
  const items = [];
  for (const line of lines) {
    const h = line.match(/^###\s+(.*)$/);
    if (h) items.push({ title: h[1].trim(), lines: [] });
    else if (items.length) items[items.length - 1].lines.push(line);
    else if (line.trim()) throw new Error(`:::${directive} text before the first ### item`);
  }
  return items.map((i) => ({ title: i.title, text: join1(i.lines) }));
}

/** `- item` lines (a wrapped line continues the previous item). */
function listItems(lines) {
  const items = [];
  for (const line of lines) {
    if (/^\s*-\s+/.test(line)) items.push(line.replace(/^\s*-\s+/, "").trim());
    else if (line.trim() && items.length) items[items.length - 1] += " " + line.trim();
  }
  return items;
}

function tableRow(line) {
  return line
    .trim()
    .replace(/^\|/, "")
    .replace(/\|$/, "")
    .split(/(?<!\\)\|/)
    .map((c) => c.trim().replace(/\\\|/g, "|"));
}

function directiveBlock(name, arg, lines, ctx) {
  switch (name) {
    case "lead":
      return { kind: "lead", text: join1(lines) };
    case "note":
    case "warn":
      return { kind: name, title: arg, text: join1(lines) };
    case "steps":
    case "cards":
      return { kind: name, items: titledItems(lines, name) };
    case "faq":
      return { kind: "faq", items: titledItems(lines, name).map((i) => ({ q: i.title, a: i.text })) };
    case "stats":
      return {
        kind: "stats",
        items: listItems(lines).map((item) => {
          const m = item.match(/^\*\*(.+?)\*\*\s*(.*)$/);
          if (!m) throw new Error(`${ctx}: a :::stats item must start with **value**`);
          return { value: m[1], label: m[2] };
        }),
      };
    case "checklist":
      return { kind: "checklist", title: arg, items: listItems(lines) };
    case "diagram":
      return { kind: "diagram", id: arg, caption: join1(lines) };
    case "deep":
      return { kind: "deep", doc: arg, text: join1(lines) };
    case "video": {
      const id = youtubeId(arg);
      if (!id) throw new Error(`${ctx}: :::video takes a YouTube address`);
      return { kind: "video", youtube: id, caption: join1(lines) };
    }
  }
  throw new Error(`${ctx}: unknown directive :::${name}`);
}

/**
 * Parses one guide. `file` is the repo-relative path, used for messages and to
 * resolve image paths; images come back as repo-relative paths in `images`.
 */
export function parseGuide(text, file) {
  const { meta, body } = frontMatter(text.replace(/\r\n/g, "\n"));
  const lines = body.split("\n");
  const blocks = [];
  const images = [];
  let para = [];
  const flush = () => {
    if (para.length) blocks.push({ kind: "p", text: join1(para) });
    para = [];
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const ctx = `${file}:${i + 1}`;
    const open = line.match(/^:::([a-z]+)\s*(.*)$/);
    if (open) {
      flush();
      const [, name, arg] = open;
      if (!DIRECTIVES.has(name)) throw new Error(`${ctx}: unknown directive :::${name}`);
      const inner = [];
      i++;
      while (i < lines.length && lines[i].trim() !== ":::") inner.push(lines[i++]);
      if (i >= lines.length) throw new Error(`${ctx}: :::${name} is not closed`);
      blocks.push(directiveBlock(name, arg.trim(), inner, ctx));
      continue;
    }
    if (line.startsWith("```")) {
      flush();
      const code = [];
      i++;
      while (i < lines.length && !lines[i].startsWith("```")) code.push(lines[i++]);
      blocks.push({ kind: "code", code: code.join("\n") });
      continue;
    }
    const heading = line.match(/^##\s+(.*)$/);
    if (heading) {
      flush();
      blocks.push({ kind: "h", text: heading[1].trim() });
      continue;
    }
    const image = line.match(/^!\[(.*)\]\((.+)\)\s*$/);
    if (image) {
      flush();
      const src = normalize(join(dirname(file), image[2])).replace(/\\/g, "/");
      images.push(src);
      blocks.push({ kind: "image", src, caption: image[1].trim() });
      continue;
    }
    if (line.trim().startsWith("|") && lines[i + 1]?.trim().match(/^\|?\s*:?-{3,}/)) {
      flush();
      const head = tableRow(line);
      const rows = [];
      i += 2;
      while (i < lines.length && lines[i].trim().startsWith("|")) rows.push(tableRow(lines[i++]));
      i--;
      blocks.push({ kind: "table", head, rows });
      continue;
    }
    if (!line.trim()) {
      flush();
      continue;
    }
    if (line.startsWith("# ")) throw new Error(`${ctx}: the title lives in the front matter, not in a # heading`);
    para.push(line);
  }
  flush();
  for (const key of ["id", "label", "summary", "title"]) {
    if (!meta[key]) throw new Error(`${file}: front matter is missing "${key}"`);
  }
  return {
    guide: {
      id: meta.id,
      label: meta.label,
      summary: meta.summary,
      title: meta.title,
      ...(meta.route ? { route: meta.route } : {}),
      ...(meta.reference ? { reference: meta.reference } : {}),
      blocks,
    },
    order: Number.isFinite(meta.order) ? meta.order : 1000,
    images,
  };
}

/** `name.pt-BR.md` -> "pt-BR"; `name.md` -> "en". */
export function localeOf(file) {
  return file.endsWith(".pt-BR.md") ? "pt-BR" : "en";
}
