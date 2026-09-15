import { Markdown } from "./Markdown";

export function MarkdownField({
  value,
  onChange,
  previewTestId,
  previewLabel,
  ariaLabel,
}: {
  value: string;
  onChange: (value: string) => void;
  previewTestId: string;
  previewLabel: string;
  ariaLabel: string;
}) {
  return (
    <div className="min-w-0 space-y-2">
      <textarea
        value={value}
        onChange={(event) => onChange(event.target.value)}
        aria-label={ariaLabel}
        className="min-h-[12rem] w-full min-w-0 resize-y rounded-lg border border-line bg-ink px-3 py-2 text-sm leading-relaxed text-fg outline-none focus:border-iris focus:ring-2 focus:ring-iris/25"
        spellCheck
      />
      <div className="min-w-0 rounded-lg border border-line bg-raised/45 px-3 py-2">
        <div className="mb-1 text-[0.65rem] font-medium uppercase tracking-[0.1em] text-faint">
          {previewLabel}
        </div>
        <div data-testid={previewTestId} className="min-h-8">
          {value.trim() ? <Markdown>{value}</Markdown> : <span className="text-sm text-faint">-</span>}
        </div>
      </div>
    </div>
  );
}
