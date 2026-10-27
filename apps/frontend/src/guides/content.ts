/**
 * The human half of the documentation: types and loader.
 *
 * The markdown under `src/docs/` is written for agents: exhaustive, file- and
 * function-level, and true to the code down to the constant. It is the wrong
 * thing to hand a person who has just opened the panel and wants to know what
 * this cluster does. These guides are the other half - a small, ordered set of
 * explained pages with diagrams, written to be read once, front to back.
 *
 * The text lives in markdown under `docs/guides/<group>/` and, for the "how
 * it works inside" pages, `docs/human/<folder>/HUMAN.md` - one file per
 * language. `scripts/build-docs.mjs` parses them (`scripts/guides-md.mjs`)
 * into `src/docs/guides.json` and copies their images to
 * `src/docs/guide-images/`; this module only reads that output. A guide is
 * still data (`Block[]`), so the renderer owns every spacing and colour
 * decision and the two locales cannot drift into different layouts. Every guide ends with
 * `deep` links into the markdown reference, because the reference is where the
 * detail lives and duplicating it here would guarantee it goes stale.
 *
 * The rule that keeps this honest: **nothing here may state a number or a
 * behaviour that the markdown does not.** When they disagree, the markdown
 * (which tracks the code) wins and this file is corrected.
 */
import type { Locale } from "../i18n/locales";
import generated from "../docs/guides.json";

export type Block =
  /** Opening sentence, set larger than the body. One per guide. */
  | { kind: "lead"; text: string }
  | { kind: "p"; text: string }
  | { kind: "h"; text: string }
  /** Numbered sequence where the order is the point. */
  | { kind: "steps"; items: { title: string; text: string }[] }
  /** Peer concepts, no order implied. */
  | { kind: "cards"; items: { title: string; text: string }[] }
  | { kind: "note"; title: string; text: string }
  | { kind: "warn"; title: string; text: string }
  | { kind: "table"; head: string[]; rows: string[][] }
  | { kind: "code"; code: string }
  | { kind: "diagram"; id: DiagramId; caption: string }
  /** A YouTube video, played in place (privacy-enhanced embed). */
  | { kind: "video"; youtube: string; caption: string }
  /** A screenshot or a crop of one; `src` is resolved to a bundled URL. */
  | { kind: "image"; src: string; caption: string }
  /** Two to four headline numbers, each with what it measures. */
  | { kind: "stats"; items: { value: string; label: string }[] }
  /** Things to confirm, in any order; rendered with check marks. */
  | { kind: "checklist"; title: string; items: string[] }
  /** Questions a reader asks next; each answer opens on demand. */
  | { kind: "faq"; items: { q: string; a: string }[] }
  /** "The full detail is over there" - a link into the markdown reference. */
  | { kind: "deep"; text: string; doc: string };

export type DiagramId =
  | "topology"
  | "request"
  | "slicing"
  | "lifecycle"
  | "setup"
  | "profiles"
  | "network"
  | "api"
  | "plugins";

/** Where a guide sits in the Documentation sidebar: the folder it lives in. */
export type GuideGroup = "concepts" | "screens" | "settings" | "human";

export type Guide = {
  id: string;
  group: GuideGroup;
  /** Screen guides: the panel route the guide explains. */
  route?: string;
  /** Human pages: the AGENTS.md they explain, opened from the reference tab. */
  reference?: string;
  /** Sidebar label. Short - it sits in a narrow column. */
  label: string;
  /** One line under the title, and the sidebar subtitle. */
  summary: string;
  title: string;
  blocks: Block[];
};

// Images are published under `public/guide-images/` by the build; the JSON
// carries their path relative to the app's base.
function withImages(guide: Guide): Guide {
  return {
    ...guide,
    blocks: guide.blocks.map((block) =>
      block.kind === "image" ? { ...block, src: `${import.meta.env.BASE_URL}${block.src}` } : block,
    ),
  };
}

const GUIDES: Record<Locale, Guide[]> = {
  en: (generated.locales.en as unknown as Guide[]).map(withImages),
  "pt-BR": (generated.locales["pt-BR"] as unknown as Guide[]).map(withImages),
};

/** The groups in sidebar order. */
export const GUIDE_GROUPS = generated.groups as GuideGroup[];

export function guidesFor(locale: Locale): Guide[] {
  return GUIDES[locale] ?? GUIDES.en;
}
