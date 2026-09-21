import hljs from "highlight.js/lib/core";
import jsonLanguage from "highlight.js/lib/languages/json";
import { useMemo } from "react";

hljs.registerLanguage("json", jsonLanguage);

/** A selectable, read-only code editor surface for structured request data. */
export function CodeViewer({ value, label, className = "" }: { value: string; label: string; className?: string }) {
  const highlighted = useMemo(
    () => hljs.highlight(value, { language: "json", ignoreIllegals: true }).value,
    [value],
  );

  return (
    <pre
      role="textbox"
      aria-label={label}
      aria-readonly="true"
      tabIndex={0}
      className={`min-h-0 w-full min-w-0 flex-1 overflow-auto rounded-lg border border-line bg-ink p-3 font-mono text-[0.72rem] leading-relaxed text-fg outline-none select-text whitespace-pre-wrap break-words focus:border-iris ${className}`}
    >
      <code className="hljs language-json" dangerouslySetInnerHTML={{ __html: highlighted }} />
    </pre>
  );
}
