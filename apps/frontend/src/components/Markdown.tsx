/**
 * Assistant answers, rendered.
 *
 * The model writes markdown, so the chat reads it: fenced code is highlighted,
 * tables scroll inside themselves rather than widening the thread, and every
 * code block has a copy button — the answers here are mostly commands the
 * operator is about to run.
 */
import { Check, Copy } from "lucide-react";
import { memo, useState } from "react";
import ReactMarkdown from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";
import { copyText } from "../lib/clipboard";

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      onClick={() => {
        void copyText(text).then((ok) => {
          if (!ok) return;
          setCopied(true);
          window.setTimeout(() => setCopied(false), 1600);
        });
      }}
      className="absolute right-2 top-2 flex h-7 items-center gap-1.5 rounded-md border border-line bg-surface px-2 text-[0.7rem] text-muted opacity-0 transition-opacity duration-150 hover:text-fg focus-visible:opacity-100 group-hover:opacity-100"
    >
      {copied ? <Check className="h-3 w-3 text-aqua" /> : <Copy className="h-3 w-3" />}
      {copied ? "copiado" : "copiar"}
    </button>
  );
}

/**
 * `compact` is the small print of a side panel: text, headings and the gaps
 * between blocks shrink together, so a network's description written with a
 * `#` title does not shout over the panel it sits in.
 */
const COMPACT =
  "min-w-0 text-[0.72rem] leading-[1.55] text-fg [&_h1]:mb-1 [&_h1]:mt-2.5 [&_h1]:text-[0.8rem] [&_h2]:mb-1 [&_h2]:mt-2.5 [&_h2]:text-[0.76rem] [&_h3]:mb-1 [&_h3]:mt-2 [&_h3]:text-[0.74rem] [&_p]:my-1.5 [&_ul]:my-1.5 [&_ol]:my-1.5 [&_ul]:pl-4 [&_ol]:pl-4";

export const Markdown = memo(function Markdown({ children, compact = false }: { children: string; compact?: boolean }) {
  return (
    <div className={compact ? COMPACT : "min-w-0 text-[0.9rem] leading-[1.7] text-fg"}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[[rehypeHighlight, { detect: true, ignoreMissing: true }]]}
        components={{
          p: ({ children }) => <p className="my-3 first:mt-0 last:mb-0">{children}</p>,
          h1: ({ children }) => (
            <h1 className="mb-2 mt-5 font-display text-lg font-semibold first:mt-0">
              {children}
            </h1>
          ),
          h2: ({ children }) => (
            <h2 className="mb-2 mt-5 font-display text-base font-semibold first:mt-0">
              {children}
            </h2>
          ),
          h3: ({ children }) => (
            <h3 className="mb-1.5 mt-4 font-display text-[0.95rem] font-semibold first:mt-0">
              {children}
            </h3>
          ),
          ul: ({ children }) => (
            <ul className="my-3 list-disc space-y-1 pl-5 marker:text-faint">{children}</ul>
          ),
          ol: ({ children }) => (
            <ol className="my-3 list-decimal space-y-1 pl-5 marker:text-faint">
              {children}
            </ol>
          ),
          a: ({ children, href }) => (
            <a
              href={href}
              target="_blank"
              rel="noreferrer"
              className="text-iris underline decoration-iris/40 underline-offset-2 hover:decoration-iris"
            >
              {children}
            </a>
          ),
          blockquote: ({ children }) => (
            <blockquote className="my-3 border-l-2 border-iris/40 pl-3 text-muted">
              {children}
            </blockquote>
          ),
          hr: () => <hr className="my-5 border-line" />,
          table: ({ children }) => (
            <div className="my-4 max-w-full overflow-x-auto rounded-lg border border-line">
              <table className="w-full border-collapse text-[0.82rem]">{children}</table>
            </div>
          ),
          th: ({ children }) => (
            <th className="border-b border-line bg-raised px-3 py-2 text-left font-medium text-muted">
              {children}
            </th>
          ),
          td: ({ children }) => (
            <td className="border-b border-line px-3 py-2 align-top last:border-0">
              {children}
            </td>
          ),
          code: ({ className, children, ...rest }) => {
            const text = String(children).replace(/\n$/, "");
            // Fenced blocks arrive with a language class; anything else is
            // inline and stays in the run of text.
            const fenced = /language-/.test(className ?? "") || text.includes("\n");
            if (!fenced) {
              return (
                <code
                  className="rounded border border-line bg-raised px-1 py-0.5 font-mono text-[0.82em] text-fg"
                  {...rest}
                >
                  {children}
                </code>
              );
            }
            return (
              <span className="group relative my-3 block">
                <CopyButton text={text} />
                <span className="block max-w-full overflow-x-auto rounded-lg border border-line bg-ink p-3.5">
                  <code
                    className={`${className ?? ""} block whitespace-pre font-mono text-[0.8rem] leading-relaxed`}
                    {...rest}
                  >
                    {children}
                  </code>
                </span>
              </span>
            );
          },
          pre: ({ children }) => <>{children}</>,
        }}
      >
        {children}
      </ReactMarkdown>
    </div>
  );
});
