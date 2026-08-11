/**
 * CODAI mark — **placeholder**.
 *
 * Swapping in the real logo means replacing the contents of `<LogoMark>` with
 * the brand SVG and nothing else: the mark is a square that fills its box, and
 * every caller sizes it through `size`, so the wordmark, the sidebar, the setup
 * screen and the favicon all follow automatically.
 *
 * The stand-in draws what the product does — three layer bars converging on one
 * output — so the space reads as intentional until the real thing arrives.
 */
import clsx from "clsx";

export function LogoMark({
  size = 32,
  className,
}: {
  size?: number;
  className?: string;
}) {
  return (
    <span
      className={clsx(
        "relative inline-flex shrink-0 items-center justify-center rounded-[28%]",
        "bg-gradient-to-br from-iris/90 to-iris/60 text-white",
        "ring-1 ring-inset ring-white/15",
        className,
      )}
      style={{ width: size, height: size }}
      aria-hidden="true"
      data-logo-placeholder="true"
    >
      <svg viewBox="0 0 32 32" width={size * 0.72} height={size * 0.72} fill="none">
        <path
          d="M7 10.5h7M7 16h11M7 21.5h7"
          stroke="currentColor"
          strokeWidth="2.4"
          strokeLinecap="round"
          opacity="0.9"
        />
        <circle cx="24" cy="16" r="3.1" fill="rgb(var(--aqua))" />
      </svg>
    </span>
  );
}

export function Logo({
  size = 32,
  showWordmark = true,
  className,
}: {
  size?: number;
  showWordmark?: boolean;
  className?: string;
}) {
  return (
    <span className={clsx("inline-flex items-center gap-2.5", className)}>
      <LogoMark size={size} />
      {showWordmark && (
        <span className="flex min-w-0 flex-col leading-none">
          <span
            className="font-display font-bold tracking-tight text-fg"
            style={{ fontSize: size * 0.5 }}
          >
            CODAI
          </span>
          <span
            className="font-mono uppercase tracking-[0.22em] text-faint"
            style={{ fontSize: size * 0.26, marginTop: size * 0.1 }}
          >
            server
          </span>
        </span>
      )}
    </span>
  );
}
