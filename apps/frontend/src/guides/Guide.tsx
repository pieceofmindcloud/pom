/**
 * Renders one guide.
 *
 * Every block type gets one look, decided here and nowhere else: that is the
 * whole reason the guides are data. A writer adding a page picks from the
 * existing blocks and gets the spacing, the colour and the dark-theme
 * behaviour for free, and cannot invent a fourth kind of callout.
 *
 * Measure over width: the prose column is capped so lines stay readable on a
 * wide screen, while tables and diagrams are allowed to use the full width -
 * they are scanned, not read.
 */
import { ArrowRight, CircleCheck, ChevronRight, Info, TriangleAlert } from "lucide-react";
import { useI18n } from "../i18n/provider";
import type { Block, Guide as GuideType } from "./content";
import { Diagram } from "./diagrams";
import { ScrollX } from "../components/ui";

function Callout({
  tone,
  title,
  text,
}: {
  tone: "note" | "warn";
  title: string;
  text: string;
}) {
  const warn = tone === "warn";
  const Icon = warn ? TriangleAlert : Info;
  return (
    <div
      className={
        warn
          ? "my-5 flex gap-3 rounded-xl border border-amber/30 bg-amber/[0.07] p-4"
          : "my-5 flex gap-3 rounded-xl border border-line bg-raised p-4"
      }
    >
      <Icon className={warn ? "mt-0.5 h-4 w-4 shrink-0 text-amber" : "mt-0.5 h-4 w-4 shrink-0 text-iris"} />
      <div className="min-w-0">
        <p className="text-[0.86rem] font-semibold text-fg">{title}</p>
        <p className="mt-1 text-[0.86rem] leading-relaxed text-muted">{text}</p>
      </div>
    </div>
  );
}

function BlockView({
  block,
  onOpenDoc,
}: {
  block: Block;
  onOpenDoc: (docId: string) => void;
}) {
  switch (block.kind) {
    case "lead":
      return (
        <p className="mb-6 max-w-[68ch] text-[1.02rem] leading-relaxed text-fg">{block.text}</p>
      );
    case "p":
      return <p className="my-4 max-w-[68ch] text-[0.92rem] leading-[1.75] text-muted">{block.text}</p>;
    case "h":
      return (
        <h3 className="mb-3 mt-9 font-display text-[1.05rem] font-semibold tracking-tight text-fg">
          {block.text}
        </h3>
      );
    case "steps":
      // A timeline: the numbered dots sit on one rail, so the order reads at a
      // glance before any text is read.
      return (
        <ol className="relative my-6 space-y-4 before:absolute before:bottom-3 before:left-3 before:top-3 before:w-px before:bg-gradient-to-b before:from-iris/50 before:via-iris/20 before:to-transparent">
          {block.items.map((item, i) => (
            <li key={item.title} className="relative flex gap-4">
              <span className="relative z-10 mt-0.5 flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-iris/50 bg-surface font-mono text-[0.7rem] font-semibold text-iris shadow-[0_0_0_4px_rgb(var(--iris)/0.08)]">
                {i + 1}
              </span>
              <div className="min-w-0 max-w-[64ch] rounded-lg pb-1">
                <p className="text-[0.9rem] font-semibold text-fg">{item.title}</p>
                <p className="mt-0.5 text-[0.86rem] leading-relaxed text-muted">{item.text}</p>
              </div>
            </li>
          ))}
        </ol>
      );
    case "cards":
      return (
        <div className="my-5 grid gap-3 sm:grid-cols-2">
          {block.items.map((item) => (
            <div
              key={item.title}
              className="group relative overflow-hidden rounded-xl border border-line bg-raised p-4 transition-colors duration-200 hover:border-iris/40"
            >
              <span className="absolute inset-x-0 top-0 h-px bg-gradient-to-r from-transparent via-iris/50 to-transparent opacity-0 transition-opacity duration-200 group-hover:opacity-100" />
              <p className="text-[0.88rem] font-semibold text-fg">{item.title}</p>
              <p className="mt-1.5 text-[0.83rem] leading-relaxed text-muted">{item.text}</p>
            </div>
          ))}
        </div>
      );
    case "stats":
      return (
        <div className="my-6 grid grid-cols-2 gap-3 sm:grid-cols-4">
          {block.items.map((item) => (
            <div key={item.label} className="rounded-xl border border-line bg-raised px-4 py-3">
              <p className="font-display text-[1.35rem] font-bold tracking-tight text-iris">{item.value}</p>
              <p className="mt-0.5 text-[0.76rem] leading-snug text-muted">{item.label}</p>
            </div>
          ))}
        </div>
      );
    case "checklist":
      return (
        <div className="my-6 rounded-xl border border-aqua/30 bg-aqua/[0.06] p-4">
          <p className="mb-2.5 text-[0.86rem] font-semibold text-fg">{block.title}</p>
          <ul className="space-y-2">
            {block.items.map((item) => (
              <li key={item} className="flex gap-2.5 text-[0.85rem] leading-relaxed text-muted">
                <CircleCheck className="mt-0.5 h-4 w-4 shrink-0 text-aqua" />
                <span className="min-w-0">{item}</span>
              </li>
            ))}
          </ul>
        </div>
      );
    case "faq":
      return (
        <div className="my-6 divide-y divide-line overflow-hidden rounded-xl border border-line bg-raised">
          {block.items.map((item) => (
            <details key={item.q} className="group">
              <summary className="flex cursor-pointer list-none items-center gap-2.5 px-4 py-3 text-[0.87rem] font-medium text-fg transition-colors hover:bg-line/30 [&::-webkit-details-marker]:hidden">
                <ChevronRight className="h-4 w-4 shrink-0 text-faint transition-transform duration-200 group-open:rotate-90 group-open:text-iris" />
                {item.q}
              </summary>
              <p className="guide-enter px-4 pb-4 pl-11 text-[0.85rem] leading-relaxed text-muted">{item.a}</p>
            </details>
          ))}
        </div>
      );
    case "note":
    case "warn":
      return <Callout tone={block.kind} title={block.title} text={block.text} />;
    case "table":
      return (
        <ScrollX className="my-5">
          <table className="w-full min-w-[34rem] border-collapse text-left text-[0.84rem]">
            <thead>
              <tr className="border-b border-line">
                {block.head.map((h) => (
                  <th key={h} className="px-3 py-2 font-medium text-faint">
                    {h}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {block.rows.map((row) => (
                <tr key={row.join("|")} className="border-b border-line/60 last:border-0">
                  {row.map((cell, i) => (
                    <td
                      key={cell}
                      className={
                        i === 0
                          ? "px-3 py-2.5 align-top font-medium text-fg"
                          : "px-3 py-2.5 align-top leading-relaxed text-muted"
                      }
                    >
                      {cell}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </ScrollX>
      );
    case "code":
      return (
        <pre className="my-5 overflow-x-auto rounded-xl border border-line bg-ink p-4 font-mono text-[0.78rem] leading-relaxed text-muted">
          {block.code}
        </pre>
      );
    case "diagram":
      return (
        <figure className="my-6 overflow-hidden rounded-xl border border-line bg-gradient-to-b from-raised to-surface p-4">
          <Diagram id={block.id} />
          <figcaption className="mt-3 border-t border-line pt-3 text-[0.78rem] leading-relaxed text-faint">
            {block.caption}
          </figcaption>
        </figure>
      );
    case "deep":
      return (
        <button
          type="button"
          onClick={() => onOpenDoc(block.doc)}
          className="group mt-2 flex w-full max-w-[68ch] items-center justify-between gap-3 rounded-xl border border-line bg-raised px-4 py-3 text-left transition-colors hover:border-line-strong hover:bg-line/30"
        >
          <span className="min-w-0">
            <span className="block text-[0.86rem] font-medium text-fg">{block.text}</span>
            <span className="mt-0.5 block truncate font-mono text-[0.7rem] text-faint">
              {block.doc}
            </span>
          </span>
          <ArrowRight className="h-4 w-4 shrink-0 text-faint transition-transform duration-150 group-hover:translate-x-0.5 group-hover:text-iris" />
        </button>
      );
  }
}

export function GuideView({
  guide,
  onOpenDoc,
}: {
  guide: GuideType;
  onOpenDoc: (docId: string) => void;
}) {
  const { t } = useI18n();
  // The deep links are a block type, but they read as a closing section rather
  // than as another paragraph, so they get one heading and sit together.
  const body = guide.blocks.filter((b) => b.kind !== "deep");
  const deeper = guide.blocks.filter((b) => b.kind === "deep");

  return (
    <article className="min-w-0">
      <header className="mb-6">
        <h2 className="font-display text-[1.5rem] font-bold tracking-tight text-fg">
          {guide.title}
        </h2>
        <p className="mt-1 text-[0.88rem] text-muted">{guide.summary}</p>
      </header>
      {/* Keyed by guide so switching pages replays the entrance. */}
      <div key={guide.id} className="guide-enter">
        {body.map((block, i) => (
          <BlockView key={`${block.kind}-${i}`} block={block} onOpenDoc={onOpenDoc} />
        ))}
      </div>
      {deeper.length > 0 && (
        <section className="mt-10 border-t border-line pt-6">
          <p className="eyebrow mb-3">{t("docs.deeper")}</p>
          <div className="flex flex-col gap-2">
            {deeper.map((block, i) => (
              <BlockView key={`deep-${i}`} block={block} onOpenDoc={onOpenDoc} />
            ))}
          </div>
        </section>
      )}
    </article>
  );
}
