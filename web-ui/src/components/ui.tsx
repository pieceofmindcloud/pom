/**
 * The small pieces every screen is built from.
 *
 * Kept in one file on purpose: they are all one screenful, and having the
 * button, the badge and the empty state side by side is what keeps their
 * spacing and weight consistent.
 */
import clsx from "clsx";
import { motion } from "framer-motion";
import { AlertTriangle, Loader2 } from "lucide-react";
import type { ReactNode } from "react";
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

export function PageHeader({
  title,
  subtitle,
  actions,
}: {
  title: string;
  subtitle?: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <header className="mb-6 flex flex-wrap items-end justify-between gap-4">
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

export function Input({
  className,
  ...rest
}: React.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={clsx(
        "h-9 w-full min-w-0 rounded-lg border border-line bg-ink px-3 text-sm text-fg",
        "placeholder:text-faint transition-colors duration-150",
        "hover:border-line-strong focus:border-iris focus:outline-none focus:ring-2 focus:ring-iris/25",
        className,
      )}
      {...rest}
    />
  );
}

export function Select({
  className,
  children,
  ...rest
}: React.SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select
      className={clsx(
        "h-9 w-full min-w-0 rounded-lg border border-line bg-ink px-2.5 text-sm text-fg",
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
  children,
  className,
}: {
  label: string;
  hint?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <label className={clsx("block min-w-0", className)}>
      <span className="mb-1.5 block text-[0.8rem] font-medium text-muted">{label}</span>
      {children}
      {hint && <span className="mt-1.5 block text-xs text-faint">{hint}</span>}
    </label>
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

export function Badge({
  tone = "neutral",
  mono = false,
  className,
  children,
}: {
  tone?: Tone;
  mono?: boolean;
  className?: string;
  children: ReactNode;
}) {
  return (
    <span
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
}: {
  label: string;
  value: number | null;
  detail?: ReactNode;
  tone?: Tone;
}) {
  return (
    <div className="min-w-0">
      <div className="mb-1.5 flex items-baseline justify-between gap-2">
        <span className="truncate text-[0.8rem] text-muted">{label}</span>
        <span className="shrink-0 font-mono text-[0.78rem] tabular-nums text-fg">
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

export function KeyValue({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="min-w-0">
      <dt className="eyebrow mb-1">{label}</dt>
      <dd className="break-path text-sm text-fg">{children}</dd>
    </div>
  );
}
