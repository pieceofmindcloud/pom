/**
 * The product mark and wordmark.
 *
 * Both come from `brand.json` (via `lib/brand`), never from literals here: the
 * name, the tagline and the image file are one edit at the repository root, and
 * the binary reads the same file. What used to be a hand-drawn placeholder is
 * now the real artwork, served from `/brand/` as a static asset rather than
 * inlined - the mark is a detailed raster and every caller shows it small.
 *
 * The mark is a square that fills its box and every caller sizes it through
 * `size`, so the sidebar, the setup screen and the browser tab all follow the
 * same source.
 */
import clsx from "clsx";
import { brand } from "../lib/brand";
import { useTheme } from "../lib/theme";

export function LogoMark({
  size = 32,
  className,
}: {
  size?: number;
  className?: string;
}) {
  return (
    <img
      src={brand.icon ?? brand.mark ?? ""}
      alt=""
      aria-hidden="true"
      width={size}
      height={size}
      // The artwork is a full-colour illustration with its own white glow: at
      // 32px, loose on a dark panel, it read as a smudge. The icon files carry
      // the rounded plate baked in (see `assets/brand/`), which is what makes
      // it read as a mark at every size and on both themes.
      // A hairline follows the baked-in plate radius: on the light theme the
      // plate is nearly the panel colour and the mark lost its edge without it.
      className={clsx(
        "shrink-0 select-none rounded-[22%] object-contain ring-1 ring-inset ring-line",
        className,
      )}
      style={{ width: size, height: size }}
      draggable={false}
    />
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
            {brand.name}
          </span>
          <span
            className="font-mono uppercase tracking-[0.22em] text-faint"
            style={{ fontSize: size * 0.26, marginTop: size * 0.1 }}
          >
            {brand.tagline}
          </span>
        </span>
      )}
    </span>
  );
}

/**
 * The full lockup - mark, wordmark and tagline in one drawing, as the brand was
 * designed. This is what the sidebar header and the setup screen show; the
 * icon-plus-text `Logo` above stays for places too small for the tagline.
 *
 * **Two arts, one per theme** (`brand.logo.light` / `brand.logo.dark`), so the
 * lockup needs no plate behind it: each theme gets art drawn for its own
 * ground. The dark one is derived from the light one by
 * `scripts/derive-dark-logo.py` until a hand-drawn version exists - replacing
 * the file is the whole change.
 *
 * `variant` is only about how it fills its box:
 * - `"card"` - padded, for the setup screen, where the lockup floats mid-page;
 * - `"band"` - fills its container edge to edge, for the sidebar header, which
 *   *is* the band.
 */
export function LogoFull({
  width = 260,
  variant = "card",
  className,
}: {
  width?: number;
  variant?: "card" | "band";
  className?: string;
}) {
  const { theme } = useTheme();
  const src = brand.logo?.[theme] ?? brand.logo?.light;
  if (!src) return <Logo size={48} className={className} />;
  const band = variant === "band";
  return (
    <span
      className={clsx(
        "inline-flex items-center justify-center",
        band ? "h-full w-full px-4 py-4" : "px-2 py-1",
        className,
      )}
    >
      <img
        src={src}
        alt={`${brand.name} — ${brand.tagline}`}
        width={width}
        className="h-auto max-w-full select-none object-contain"
        style={{ width }}
        draggable={false}
      />
    </span>
  );
}
