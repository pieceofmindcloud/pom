/**
 * The small pieces every screen is built from.
 *
 * Kept in one file on purpose: they are all one screenful, and having the
 * button, the badge and the empty state side by side is what keeps their
 * spacing and weight consistent.
 */
import clsx from "clsx";
import { motion } from "framer-motion";
import { AlertTriangle, Info, Loader2 } from "lucide-react";
import { createPortal } from "react-dom";
import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import { useI18n } from "../i18n/provider";
import { localizeServerText } from "../i18n/locales";

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

/**
 * O tamanho é escolhido aqui, por `compact`, e não acrescentado pelo chamador.
 *
 * `clsx` concatena, não resolve conflito de Tailwind: quando duas utilitárias
 * da mesma propriedade caem na mesma classe, quem vence é a ordem da folha de
 * estilo, não a ordem do argumento. Os campos do inspetor pediam
 * `h-6 px-1.5 text-[0.62rem]` e renderizavam `h-9 px-3 text-sm` — o painel
 * inteiro saía em tamanho cheio e as classes passadas eram letra morta. Uma
 * variante troca as utilitárias em vez de empilhá-las, então o pedido do
 * chamador é o que aparece.
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

/** Mesma variante do `Input`, e pelo mesmo motivo: os dois vivem lado a lado no
 *  inspetor, e um encolher sem o outro deixa a coluna desalinhada. */
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
 * O estado e o posicionamento de um balão de portal, compartilhados por
 * `AlertTooltip` (aviso âmbar) e `InfoTip` (dica neutra). Portal fixo para não
 * ser cortado pelo scroll do inspetor; a largura máxima mais
 * `overflow-wrap:anywhere` seguram ids sem espaços dentro da viewport. Abre
 * por hover e por foco, fixa no clique, fecha no Escape ou num clique fora.
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
 * Avisos compactos que continuam sendo uma superfície de leitura completa.
 */
export function AlertTooltip({
  label,
  message,
  /** Densidade do inspetor: mesmo tamanho do `[i]`, sem a moldura do aviso. */
  compact = false,
}: {
  label: string;
  message: string;
  compact?: boolean;
}) {
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
        aria-label={`Ver aviso para ${label}`}
        aria-expanded={open}
        aria-controls={open ? tooltipId : undefined}
        aria-describedby={open ? tooltipId : undefined}
        title={`Aviso para ${label}`}
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
 * Uma dica neutra ao lado de um controle: o `[i]` abre no hover, no foco ou
 * no toque, e o texto explica o que o parâmetro faz e como ele afeta a
 * inferência. Mesma mecânica de portal do `AlertTooltip`, sem o peso visual
 * de um aviso - a informação fica escondida até ser pedida para não poluir o
 * painel.
 */
export function InfoTip({ label, message }: { label: string; message: string }) {
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
        aria-label={`O que é ${label}`}
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
  /** Densidade do card de máquina, onde cabem oito medidores numa tela. */
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
  const { t, locale } = useI18n();
  const message = error instanceof Error ? error.message : String(error);
  return (
    <div className="flex items-start gap-3 rounded-xl border border-rose/30 bg-rose/[0.07] px-4 py-3.5">
      <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-rose" />
      <div className="min-w-0 flex-1">
        <p className="text-sm font-medium text-fg">{title ?? t("common.loadFailed")}</p>
        <p className="break-path mt-1 text-sm text-muted">{localizeServerText(locale, message)}</p>
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
  /** Densidade do card de máquina. */
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
