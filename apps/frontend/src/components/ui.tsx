/**
 * The small pieces every screen is built from.
 *
 * Kept in one file on purpose: they are all one screenful, and having the
 * button, the badge and the empty state side by side is what keeps their
 * spacing and weight consistent.
 */
import clsx from "clsx";
import { motion } from "framer-motion";
import { AlertTriangle, Info, Loader2, X } from "lucide-react";
import { createPortal } from "react-dom";
import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import type { CSSProperties, ReactNode } from "react";
import { useI18n } from "../i18n/provider";

// ── Surfaces ─────────────────────────────────────────────────────────────────

export function Panel({
  children,
  className,
  ...rest
}: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div className={clsx("panel min-w-0", className)} {...rest}>
      {children}
    </div>
  );
}

export function PanelHeader({
  title,
  subtitle,
  eyebrow,
  actions,
  className,
}: {
  title: ReactNode;
  subtitle?: ReactNode;
  eyebrow?: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={clsx(
        "flex flex-wrap items-start justify-between gap-3 border-b border-line px-5 py-4",
        className,
      )}
    >
      <div className="min-w-0">
        {eyebrow && <div className="eyebrow mb-1.5">{eyebrow}</div>}
        <h2 className="font-display text-[0.98rem] font-semibold tracking-tight text-fg">
          {title}
        </h2>
        {subtitle && (
          <p className="mt-1 max-w-prose text-sm leading-relaxed text-muted">
            {subtitle}
          </p>
        )}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </div>
  );
}

/**
 * The compact header bar shared by every full-bleed table screen (the
 * contract `Deployments`/`PublicNetworks`/`DeploymentLogs` already use):
 * a fixed, edge-to-edge bar above a body that scrolls by itself, instead of
 * the padded, centred column `PageHeader` sits in.
 */
export function FullBleedHeader({
  title,
  subtitle,
  actions,
  className,
}: {
  title: string;
  subtitle?: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <header className={clsx("shrink-0 border-b border-line bg-surface/75", className)}>
      <div className="flex min-w-0 flex-wrap items-center justify-between gap-3 px-3 py-3 sm:px-4">
        <div className="min-w-0">
          <h1 className="truncate font-display text-lg font-semibold tracking-tight text-fg">
            {title}
          </h1>
          {subtitle && <p className="mt-0.5 truncate text-[0.7rem] text-muted">{subtitle}</p>}
        </div>
        {actions && <div className="flex shrink-0 flex-wrap items-center gap-2">{actions}</div>}
      </div>
    </header>
  );
}

/** The filter/search strip under a `FullBleedHeader`, same bar `PublicNetworks` uses. */
export function FullBleedToolbar({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={clsx(
        "flex shrink-0 flex-wrap items-end justify-between gap-3 border-b border-line bg-raised/60 px-4 py-3 sm:px-5",
        className,
      )}
    >
      {children}
    </div>
  );
}

export function PageHeader({
  title,
  subtitle,
  actions,
  className,
}: {
  title: string;
  subtitle?: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <header className={clsx("mb-6 flex flex-wrap items-end justify-between gap-4", className)}>
      <div className="min-w-0">
        <h1 className="font-display text-2xl font-bold tracking-tight text-fg">
          {title}
        </h1>
        {subtitle && (
          <p className="mt-1.5 max-w-2xl text-sm leading-relaxed text-muted">
            {subtitle}
          </p>
        )}
      </div>
      {actions && <div className="flex flex-wrap items-center gap-2">{actions}</div>}
    </header>
  );
}

// ── Controls ─────────────────────────────────────────────────────────────────

type ButtonProps = React.ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: "primary" | "ghost" | "outline" | "danger";
  size?: "sm" | "md";
  loading?: boolean;
  icon?: ReactNode;
};

export function Button({
  variant = "outline",
  size = "md",
  loading = false,
  icon,
  className,
  children,
  disabled,
  ...rest
}: ButtonProps) {
  return (
    <button
      className={clsx(
        "inline-flex select-none items-center justify-center gap-2 rounded-lg font-medium",
        "transition-[background-color,border-color,color,transform] duration-150 ease-swift",
        "active:scale-[0.985] disabled:pointer-events-none disabled:opacity-45",
        size === "sm" ? "h-8 px-2.5 text-[0.8rem]" : "h-9 px-3.5 text-sm",
        variant === "primary" &&
          "bg-iris text-white hover:bg-iris/85 shadow-[0_1px_0_rgb(255_255_255/0.12)_inset]",
        variant === "outline" &&
          "border border-line bg-raised text-fg hover:border-line-strong hover:bg-line/40",
        variant === "ghost" && "text-muted hover:bg-line/40 hover:text-fg",
        variant === "danger" &&
          "border border-rose/40 bg-rose/10 text-rose hover:bg-rose/20",
        className,
      )}
      disabled={disabled || loading}
      {...rest}
    >
      {loading ? <Loader2 className="h-4 w-4 animate-spin" /> : icon}
      {children}
    </button>
  );
}

/**
 * The size is chosen here, by `compact`, and not added by the caller.
 *
 * `clsx` concatenates, it does not resolve Tailwind conflicts: when two utilities
 * of the same property land in the same class, the winner is the order of the
 * stylesheet, not the order of the argument. The inspector's fields asked for
 * `h-6 px-1.5 text-[0.62rem]` and rendered `h-9 px-3 text-sm` - the whole panel
 * came out at full size and the classes passed were dead letter. A
 * variant swaps the utilities instead of stacking them, so the caller's request
 * is what shows up.
 */
export function Input({
  className,
  compact,
  ...rest
}: React.InputHTMLAttributes<HTMLInputElement> & { compact?: boolean }) {
  return (
    <input
      className={clsx(
        compact
          ? "h-6 rounded-md px-1.5 text-[0.62rem]"
          : "h-9 rounded-lg px-3 text-sm",
        "w-full min-w-0 border border-line bg-ink text-fg",
        "placeholder:text-faint transition-colors duration-150",
        "hover:border-line-strong focus:border-iris focus:outline-none focus:ring-2 focus:ring-iris/25",
        className,
      )}
      {...rest}
    />
  );
}

/** Same variant as `Input`, and for the same reason: the two live side by side in the
 *  inspector, and one shrinking without the other leaves the column misaligned. */
export function Select({
  className,
  compact,
  children,
  ...rest
}: React.SelectHTMLAttributes<HTMLSelectElement> & { compact?: boolean }) {
  return (
    <select
      className={clsx(
        compact
          ? "h-6 rounded-md px-1 text-[0.62rem]"
          : "h-9 rounded-lg px-2.5 text-sm",
        "w-full min-w-0 border border-line bg-ink text-fg",
        "transition-colors duration-150 hover:border-line-strong",
        "focus:border-iris focus:outline-none focus:ring-2 focus:ring-iris/25",
        className,
      )}
      {...rest}
    >
      {children}
    </select>
  );
}

export function Field({
  label,
  hint,
  tip,
  children,
  className,
}: {
  label: string;
  /** Visible helper text under the control. */
  hint?: ReactNode;
  /** Hover/focus/tap explanation shown as an `[i]` next to the label - the
   *  same text a `hint` would carry, kept out of the layout until asked for. */
  tip?: string;
  children: ReactNode;
  className?: string;
}) {
  // The `[i]` trigger is a <button>, and a <button> inside a <label> is
  // invalid HTML that breaks the wrapped control's accessible name. So the
  // <label> keeps wrapping only the text and the control (name + implicit
  // association unchanged), and the tip sits outside it, pinned to the label
  // row.
  return (
    <div className={clsx("relative min-w-0", className)}>
      <label className="block min-w-0">
        <span
          className={clsx(
            "mb-1.5 block text-[0.8rem] font-medium text-muted",
            tip && "pr-5",
          )}
        >
          {label}
        </span>
        {children}
        {hint && <span className="mt-1.5 block text-xs text-faint">{hint}</span>}
      </label>
      {tip && (
        <span className="absolute right-0 top-0">
          <InfoTip label={label} message={tip} />
        </span>
      )}
    </div>
  );
}

// ── Signals ──────────────────────────────────────────────────────────────────

export type Tone = "neutral" | "iris" | "aqua" | "amber" | "rose" | "sky" | "lime";

const toneClass: Record<Tone, string> = {
  neutral: "border-line bg-raised text-muted",
  iris: "border-iris/35 bg-iris/12 text-iris",
  aqua: "border-aqua/35 bg-aqua/12 text-aqua",
  amber: "border-amber/35 bg-amber/12 text-amber",
  rose: "border-rose/35 bg-rose/12 text-rose",
  sky: "border-sky/35 bg-sky/12 text-sky",
  lime: "border-lime/35 bg-lime/12 text-lime",
};

type TooltipPosition = { top: number; left: number };

function clamp(value: number, min: number, max: number) {
  return Math.min(Math.max(value, min), Math.max(min, max));
}

/**
 * The state and positioning of a portal bubble, shared by
 * `AlertTooltip` (amber warning) and `InfoTip` (neutral tip). Fixed portal so as not to
 * be clipped by the inspector's scroll; the maximum width plus
 * `overflow-wrap:anywhere` keep space-less ids inside the viewport. Opens
 * on hover and on focus, pins on click, closes on Escape or on a click outside.
 */
function usePositionedPopover() {
  const [open, setOpen] = useState(false);
  const [pinned, setPinned] = useState(false);
  const [position, setPosition] = useState<TooltipPosition>({ top: 8, left: 8 });
  const triggerRef = useRef<HTMLButtonElement>(null);
  const tooltipRef = useRef<HTMLDivElement>(null);

  const updatePosition = useCallback(() => {
    const trigger = triggerRef.current;
    if (!trigger) return;
    const rect = trigger.getBoundingClientRect();
    const tooltip = tooltipRef.current;
    const width = tooltip?.offsetWidth || Math.min(320, window.innerWidth - 16);
    const height = tooltip?.offsetHeight || 160;
    const left = clamp(rect.right - width, 8, window.innerWidth - width - 8);
    const below = rect.bottom + 8;
    const preferredTop =
      below + height <= window.innerHeight - 8 ? below : rect.top - height - 8;
    setPosition({ top: clamp(preferredTop, 8, window.innerHeight - height - 8), left });
  }, []);

  useLayoutEffect(() => {
    if (!open) return;
    updatePosition();
    window.addEventListener("resize", updatePosition);
    window.addEventListener("scroll", updatePosition, true);
    return () => {
      window.removeEventListener("resize", updatePosition);
      window.removeEventListener("scroll", updatePosition, true);
    };
  }, [open, updatePosition]);

  useEffect(() => {
    if (!open) return;
    const close = (event: PointerEvent) => {
      const target = event.target as Node;
      if (triggerRef.current?.contains(target) || tooltipRef.current?.contains(target)) return;
      setPinned(false);
      setOpen(false);
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setPinned(false);
      setOpen(false);
      triggerRef.current?.focus();
    };
    document.addEventListener("pointerdown", close);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("pointerdown", close);
      document.removeEventListener("keydown", escape);
    };
  }, [open]);

  return { open, setOpen, pinned, setPinned, position, triggerRef, tooltipRef };
}

/**
 * Compact warnings that remain a full reading surface.
 */
export function AlertTooltip({
  label,
  message,
  /** Inspector density: same size as the `[i]`, without the warning's frame. */
  compact = false,
}: {
  label: string;
  message: string;
  compact?: boolean;
}) {
  const { t } = useI18n();
  const { open, setOpen, pinned, setPinned, position, triggerRef, tooltipRef } =
    usePositionedPopover();
  const tooltipId = `alert-tooltip-${useId().replaceAll(":", "")}`;
  const closeIfUnpinned = () => {
    if (!pinned) setOpen(false);
  };

  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        aria-label={t("common.viewWarningFor", { label })}
        aria-expanded={open}
        aria-controls={open ? tooltipId : undefined}
        aria-describedby={open ? tooltipId : undefined}
        title={t("common.warningFor", { label })}
        onMouseEnter={() => setOpen(true)}
        onMouseLeave={closeIfUnpinned}
        onFocus={() => setOpen(true)}
        onBlur={closeIfUnpinned}
        onClick={() => {
          const next = !pinned;
          setPinned(next);
          setOpen(next);
        }}
        className={clsx(
          "flex shrink-0 items-center justify-center text-amber transition-colors focus-visible:ring-2 focus-visible:ring-amber/60",
          compact
            ? "h-4 w-4 rounded-full hover:text-fg"
            : "mt-0.5 h-6 w-6 rounded-md border border-amber/35 bg-amber/10 hover:bg-amber/20",
        )}
      >
        <AlertTriangle aria-hidden="true" className="h-3.5 w-3.5" />
      </button>
      {open &&
        createPortal(
          <div
            ref={tooltipRef}
            id={tooltipId}
            role="tooltip"
            className="fixed z-[80] max-h-[min(18rem,calc(100vh-1rem))] w-[min(20rem,calc(100vw-1rem))] max-w-[calc(100vw-1rem)] overflow-y-auto break-words whitespace-normal rounded-lg border border-amber/40 bg-ink px-3 py-2.5 text-[0.68rem] leading-relaxed text-fg shadow-lift [overflow-wrap:anywhere]"
            style={{ top: position.top, left: position.left }}
          >
            {message}
          </div>,
          document.body,
        )}
    </>
  );
}

/**
 * A neutral tip beside a control: the `[i]` opens on hover, on focus or
 * on touch, and the text explains what the parameter does and how it affects
 * inference. Same portal mechanics as `AlertTooltip`, without the visual weight
 * of a warning - the information stays hidden until asked for so as not to pollute the
 * panel.
 */
export function InfoTip({ label, message }: { label: string; message: string }) {
  const { t } = useI18n();
  const { open, setOpen, pinned, setPinned, position, triggerRef, tooltipRef } =
    usePositionedPopover();
  const tooltipId = `info-tip-${useId().replaceAll(":", "")}`;
  const closeIfUnpinned = () => {
    if (!pinned) setOpen(false);
  };

  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        aria-label={t("common.whatIs", { label })}
        aria-expanded={open}
        aria-controls={open ? tooltipId : undefined}
        aria-describedby={open ? tooltipId : undefined}
        onMouseEnter={() => setOpen(true)}
        onMouseLeave={closeIfUnpinned}
        onFocus={() => setOpen(true)}
        onBlur={closeIfUnpinned}
        onClick={() => {
          const next = !pinned;
          setPinned(next);
          setOpen(next);
        }}
        className="inline-flex h-4 w-4 shrink-0 items-center justify-center rounded-full text-faint transition-colors hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris/50"
      >
        <Info aria-hidden="true" className="h-3.5 w-3.5" />
      </button>
      {open &&
        createPortal(
          <div
            ref={tooltipRef}
            id={tooltipId}
            role="tooltip"
            className="fixed z-[80] max-h-[min(18rem,calc(100vh-1rem))] w-[min(20rem,calc(100vw-1rem))] max-w-[calc(100vw-1rem)] overflow-y-auto break-words whitespace-normal rounded-lg border border-line-strong bg-ink px-3 py-2.5 text-[0.68rem] leading-relaxed text-fg shadow-lift [overflow-wrap:anywhere]"
            style={{ top: position.top, left: position.left }}
          >
            {message}
          </div>,
          document.body,
        )}
    </>
  );
}

/**
 * An icon that explains itself: the same popover as `InfoTip`, opened by
 * hover, focus or touch, but the trigger is the icon being explained. A click
 * on it never reaches a clickable row underneath.
 */
export function IconTip({ label, message, children, className }: { label: string; message: string; children: ReactNode; className?: string }) {
  const { open, setOpen, pinned, setPinned, position, triggerRef, tooltipRef } =
    usePositionedPopover();
  const tooltipId = `icon-tip-${useId().replaceAll(":", "")}`;
  const closeIfUnpinned = () => {
    if (!pinned) setOpen(false);
  };
  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        aria-label={label}
        aria-expanded={open}
        aria-describedby={open ? tooltipId : undefined}
        onMouseEnter={() => setOpen(true)}
        onMouseLeave={closeIfUnpinned}
        onFocus={() => setOpen(true)}
        onBlur={closeIfUnpinned}
        onClick={(event) => {
          event.stopPropagation();
          const next = !pinned;
          setPinned(next);
          setOpen(next);
        }}
        className={clsx("inline-flex shrink-0 items-center justify-center rounded-md transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris/50", className)}
      >
        {children}
      </button>
      {open &&
        createPortal(
          <div
            ref={tooltipRef}
            id={tooltipId}
            role="tooltip"
            className="fixed z-[80] w-[min(18rem,calc(100vw-1rem))] max-w-[calc(100vw-1rem)] break-words whitespace-normal rounded-lg border border-line-strong bg-ink px-3 py-2 text-[0.68rem] leading-relaxed text-fg shadow-lift"
            style={{ top: position.top, left: position.left }}
          >
            <p className="font-semibold">{label}</p>
            <p className="mt-0.5 text-muted">{message}</p>
          </div>,
          document.body,
        )}
    </>
  );
}

export function Badge({
  tone = "neutral",
  mono = false,
  className,
  "data-testid": testId,
  children,
}: {
  tone?: Tone;
  mono?: boolean;
  className?: string;
  "data-testid"?: string;
  children: ReactNode;
}) {
  return (
    <span
      data-testid={testId}
      className={clsx(
        "inline-flex items-center gap-1.5 whitespace-nowrap rounded-md border px-2 py-0.5",
        "text-[0.72rem] font-medium leading-5",
        mono && "font-mono tracking-tight",
        toneClass[tone],
        className,
      )}
    >
      {children}
    </span>
  );
}

// ── Tabs ─────────────────────────────────────────────────────────────────────

export type TabItem<T extends string> = {
  value: T;
  label: ReactNode;
  icon?: ReactNode;
  /** After the label, e.g. a count. */
  badge?: ReactNode;
  disabled?: boolean;
  /** Draws a close button beside the tab (a sibling, never nested in the tab button). */
  onClose?: () => void;
  /** Accessible name of the close button. */
  closeLabel?: string;
};

/**
 * A row of tabs drawn as folder tabs over the content below (the POM-JEV look,
 * `--tab-*` in the theme): the chosen tab joins the surface under it, marked by
 * a teal line on top. `fill` spreads the tabs over the whole row.
 */
export function Tabs<T extends string>({
  items,
  value,
  onChange,
  label,
  fill = false,
  className,
  actions,
}: {
  items: TabItem<T>[];
  value: T;
  onChange: (value: T) => void;
  label?: string;
  fill?: boolean;
  className?: string;
  /** Controls drawn at the right end of the tab row, outside the tablist. */
  actions?: ReactNode;
}) {
  if (actions) {
    return (
      <div className={clsx("pom-tabs pom-tabs-with-actions", className)}>
        <Tabs items={items} value={value} onChange={onChange} label={label} fill={fill} className="pom-tabs-inner" />
        <div className="pom-tabs-actions">{actions}</div>
      </div>
    );
  }
  return (
    <div role="tablist" aria-label={label} className={clsx("pom-tabs", fill && "pom-tabs-fill", className)}>
      {items.map((item) => {
        const on = item.value === value;
        const content = (
          <>
            {item.icon && (
              <span className="pom-tab-icon" aria-hidden="true">
                {item.icon}
              </span>
            )}
            {item.label}
            {item.badge}
          </>
        );
        if (item.onClose) {
          return (
            <span key={item.value} className={clsx("pom-tab pom-tab-closable", on && "pom-tab-on")}>
              <button
                type="button"
                role="tab"
                aria-selected={on}
                disabled={item.disabled}
                onClick={() => onChange(item.value)}
                className="pom-tab-main"
              >
                {content}
              </button>
              <button type="button" aria-label={item.closeLabel} title={item.closeLabel} onClick={item.onClose} className="pom-tab-close">
                <X className="h-3 w-3" aria-hidden="true" />
              </button>
            </span>
          );
        }
        return (
          <button
            key={item.value}
            type="button"
            role="tab"
            aria-selected={on}
            disabled={item.disabled}
            onClick={() => onChange(item.value)}
            className={clsx("pom-tab", on && "pom-tab-on")}
          >
            {item.icon && (
              <span className="pom-tab-icon" aria-hidden="true">
                {item.icon}
              </span>
            )}
            {item.label}
            {item.badge}
          </button>
        );
      })}
    </div>
  );
}

/** Space between the end of the text and its next lap, in px. */
const MARQUEE_GAP = 32;
/** Part of each lap the text rests at the start, so it can be read before it moves (keep in step with `@keyframes pom-marquee`). */
const MARQUEE_REST = 0.15;

/**
 * One line of text that never widens its container: when it does not fit, it is
 * clipped and scrolls from right to left in a loop (a second copy follows the
 * first, so the lap closes without a jump), resting a moment at the start of each
 * lap and pausing under the pointer. Text that fits stays still. With reduced
 * motion it stays still too, cut with an ellipsis. `speed` is in px per second.
 */
export function MarqueeText({ children, className, speed = 40 }: { children: ReactNode; className?: string; speed?: number }) {
  const outer = useRef<HTMLSpanElement>(null);
  const copy = useRef<HTMLSpanElement>(null);
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const box = outer.current;
    const text = copy.current;
    if (!box || !text) return;
    const measure = () => setWidth(text.offsetWidth > box.clientWidth + 1 ? text.offsetWidth : 0);
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(box);
    observer.observe(text);
    return () => observer.disconnect();
  }, [children]);
  const moving = width > 0;
  const lap = width + MARQUEE_GAP;
  const style = moving
    ? ({ "--marquee-shift": `-${lap}px`, "--marquee-duration": `${(lap / speed / (1 - MARQUEE_REST)).toFixed(2)}s` } as CSSProperties)
    : undefined;
  return (
    <span
      ref={outer}
      className={clsx("pom-marquee", moving && "pom-marquee-on", className)}
      title={moving && typeof children === "string" ? children : undefined}
    >
      <span className="pom-marquee-track" style={style}>
        <span ref={copy} className="pom-marquee-copy">
          {children}
        </span>
        {moving && (
          <>
            <span className="pom-marquee-gap" style={{ width: MARQUEE_GAP }} aria-hidden="true" />
            <span className="pom-marquee-copy" aria-hidden="true">
              {children}
            </span>
          </>
        )}
      </span>
    </span>
  );
}

export function Dot({ tone = "neutral", pulse = false }: { tone?: Tone; pulse?: boolean }) {
  const color: Record<Tone, string> = {
    neutral: "bg-faint",
    iris: "bg-iris",
    aqua: "bg-aqua",
    amber: "bg-amber",
    rose: "bg-rose",
    sky: "bg-sky",
    lime: "bg-lime",
  };
  return (
    <span className="relative inline-flex h-2 w-2 shrink-0">
      {pulse && (
        <span
          className={clsx("absolute inset-0 rounded-full opacity-60", color[tone])}
          style={{ animation: "ping 1.8s cubic-bezier(0,0,0.2,1) infinite" }}
        />
      )}
      <span className={clsx("relative h-2 w-2 rounded-full", color[tone])} />
    </span>
  );
}

export function Progress({
  value,
  tone = "iris",
  className,
}: {
  /** 0..100, or null for work whose size is not known yet. */
  value: number | null;
  tone?: Tone;
  className?: string;
}) {
  const bar: Record<Tone, string> = {
    neutral: "bg-faint",
    iris: "bg-iris",
    aqua: "bg-aqua",
    amber: "bg-amber",
    rose: "bg-rose",
    sky: "bg-sky",
    lime: "bg-lime",
  };
  return (
    <div
      className={clsx(
        "relative h-1.5 w-full overflow-hidden rounded-full bg-line",
        className,
      )}
      role="progressbar"
      aria-valuenow={value ?? undefined}
      aria-valuemin={0}
      aria-valuemax={100}
    >
      {value == null ? (
        <div
          className={clsx("absolute inset-y-0 w-1/3 rounded-full opacity-70", bar[tone])}
          style={{ animation: "indeterminate 1.4s ease-in-out infinite" }}
        />
      ) : (
        <motion.div
          className={clsx("h-full rounded-full", bar[tone])}
          initial={false}
          animate={{ width: `${Math.max(0, Math.min(100, value))}%` }}
          transition={{ duration: 0.4, ease: [0.22, 1, 0.36, 1] }}
        />
      )}
    </div>
  );
}

/** A labelled bar, the shape RAM / VRAM / disk are all reported in. */
export function Meter({
  label,
  value,
  detail,
  tone = "iris",
  /** Density of the machine card, where eight gauges fit on one screen. */
  compact = false,
}: {
  label: string;
  value: number | null;
  detail?: ReactNode;
  tone?: Tone;
  compact?: boolean;
}) {
  return (
    <div className="min-w-0">
      <div className={clsx("flex items-baseline justify-between gap-2", compact ? "mb-1" : "mb-1.5")}>
        <span className={clsx("truncate text-muted", compact ? "text-[0.68rem]" : "text-[0.8rem]")}>
          {label}
        </span>
        <span
          className={clsx(
            "shrink-0 font-mono tabular-nums text-fg",
            compact ? "text-[0.66rem]" : "text-[0.78rem]",
          )}
        >
          {detail}
        </span>
      </div>
      <Progress value={value} tone={tone} />
    </div>
  );
}

// ── States ───────────────────────────────────────────────────────────────────

export function Empty({
  icon,
  title,
  children,
  action,
}: {
  icon?: ReactNode;
  title: string;
  children?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="flex flex-col items-center justify-center px-6 py-14 text-center">
      {icon && (
        <div className="mb-3.5 flex h-11 w-11 items-center justify-center rounded-xl border border-line bg-raised text-faint">
          {icon}
        </div>
      )}
      <p className="font-display text-[0.95rem] font-semibold text-fg">{title}</p>
      {children && (
        <p className="mt-1.5 max-w-sm text-sm leading-relaxed text-muted">{children}</p>
      )}
      {action && <div className="mt-4">{action}</div>}
    </div>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return (
    <div
      className={clsx(
        "relative overflow-hidden rounded-md bg-line/70",
        "after:absolute after:inset-0 after:animate-shimmer",
        "after:bg-gradient-to-r after:from-transparent after:via-fg/[0.06] after:to-transparent",
        className,
      )}
    />
  );
}

export function ErrorState({
  title,
  error,
  onRetry,
}: {
  title?: string;
  error: unknown;
  onRetry?: () => void;
}) {
  const { t } = useI18n();
  const message = error instanceof Error ? error.message : String(error);
  return (
    <div className="flex items-start gap-3 rounded-xl border border-rose/30 bg-rose/[0.07] px-4 py-3.5">
      <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-rose" />
      <div className="min-w-0 flex-1">
        <p className="text-sm font-medium text-fg">{title ?? t("common.loadFailed")}</p>
        <p className="break-path mt-1 text-sm text-muted">{message}</p>
      </div>
      {onRetry && (
        <Button size="sm" variant="ghost" onClick={onRetry}>
          {t("common.retry")}
        </Button>
      )}
    </div>
  );
}

// ── Data ─────────────────────────────────────────────────────────────────────

export function Mono({
  children,
  className,
  title,
}: {
  children: ReactNode;
  className?: string;
  title?: string;
}) {
  return (
    <span
      className={clsx("font-mono text-[0.8rem] tabular-nums", className)}
      title={title}
    >
      {children}
    </span>
  );
}

/** Wide content scrolls here, never on the page. */
export function ScrollX({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <div className={clsx("min-w-0 max-w-full overflow-x-auto", className)}>
      {children}
    </div>
  );
}

/**
 * The aside under a panel: an icon, then one sentence explaining the screen.
 *
 * It is a component and not four lines of markup because the obvious markup is
 * a trap. A flex container makes a separate flex item out of every inline
 * element in the sentence, so a `<span className="font-mono">` in the middle of
 * a phrase gets pulled out of the text flow and the sentence comes apart into
 * columns. Keeping the prose inside a single child makes that impossible.
 */
export function Footnote({
  icon,
  children,
  className,
}: {
  icon: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <p
      className={clsx(
        "flex items-start gap-2 text-xs leading-relaxed text-faint",
        className,
      )}
    >
      <span className="mt-0.5 flex shrink-0">{icon}</span>
      <span className="min-w-0">{children}</span>
    </p>
  );
}

export function KeyValue({
  label,
  children,
  /** Density of the machine card. */
  compact = false,
}: {
  label: string;
  children: ReactNode;
  compact?: boolean;
}) {
  return (
    <div className="min-w-0">
      <dt className={clsx("eyebrow", compact ? "mb-0.5" : "mb-1")}>{label}</dt>
      <dd className={clsx("break-path text-fg", compact ? "text-[0.72rem]" : "text-sm")}>
        {children}
      </dd>
    </div>
  );
}
