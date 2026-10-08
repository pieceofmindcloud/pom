import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useLocation } from "react-router-dom";
import { useI18n } from "../i18n/provider";
import { currentGuideStep, useGuide, useGuideActions, type GuideStepId } from "../lib/guide";

interface StepPlan {
  /** Screen where the step is carried out. */
  path: string;
  /** Menu section that holds the link, when it sits inside one. */
  section?: string;
  /** What to point at once the operator is on the screen. */
  target: string;
}

const PLAN: Record<GuideStepId, StepPlan> = {
  network_name: { path: "/config/rede", section: "nav.network", target: '[data-guide="network-name"]' },
  machine_name: { path: "/config/minha-maquina", section: "nav.network", target: '[data-guide="machine-name"]' },
  join: { path: "/redes-publicas", target: '[data-guide="network-join"]' },
};

/** The message keys of each step: `guide.<name>Title`, `Go` and `Here`. */
const MESSAGES: Record<GuideStepId, string> = {
  network_name: "network",
  machine_name: "machine",
  join: "join",
};

const BALLOON_WIDTH = 288;
const GAP = 14;
const EDGE = 12;

function visible(element: Element | null): element is HTMLElement {
  if (!(element instanceof HTMLElement)) return false;
  const rect = element.getBoundingClientRect();
  return rect.width > 0 && rect.height > 0;
}

/**
 * What the balloon points at: the field on the step's own screen, or the menu
 * entry that leads there - the link itself, or the section that hides it.
 */
function findTarget(plan: StepPlan, onScreen: boolean): HTMLElement | null {
  if (onScreen) {
    const field = document.querySelector(plan.target);
    return visible(field) ? field : null;
  }
  const link = document.querySelector(`[data-guide-nav="${plan.path}"]`);
  if (visible(link)) return link;
  if (!plan.section) return null;
  const section = document.querySelector(`[data-guide-section="${plan.section}"]`);
  return visible(section) ? section : null;
}

interface Anchor {
  left: number;
  top: number;
  width: number;
  height: number;
}

function sameAnchor(a: Anchor | null, b: Anchor | null): boolean {
  if (!a || !b) return a === b;
  return a.left === b.left && a.top === b.top && a.width === b.width && a.height === b.height;
}

/**
 * The hints of a machine's first run. One balloon at a time floats beside the
 * thing to click or fill, and moves on when the step is done: first to the
 * menu entry, then to the field on the screen it opens. With nothing to point
 * at (a narrow screen with the menu closed, a list that is still empty) the
 * same text sits in the corner instead.
 */
export function FirstRunGuide({ enabled = true }: { enabled?: boolean }) {
  const { t } = useI18n();
  const location = useLocation();
  const guide = useGuide(enabled);
  const actions = useGuideActions();
  const step = enabled ? currentGuideStep(guide.data) : null;
  const plan = step ? PLAN[step] : null;
  const onScreen = plan ? location.pathname === plan.path : false;
  const [anchor, setAnchor] = useState<Anchor | null>(null);
  const balloon = useRef<HTMLDivElement>(null);
  const [balloonHeight, setBalloonHeight] = useState(0);

  // The target moves with scrolling, the menu opening and the screen
  // loading, and none of those tells us: follow it by looking again.
  useEffect(() => {
    if (!plan) {
      setAnchor(null);
      return;
    }
    const measure = () => {
      const target = findTarget(plan, onScreen);
      const rect = target?.getBoundingClientRect();
      const next = rect
        ? { left: Math.round(rect.left), top: Math.round(rect.top), width: Math.round(rect.width), height: Math.round(rect.height) }
        : null;
      setAnchor((previous) => (sameAnchor(previous, next) ? previous : next));
    };
    measure();
    const timer = window.setInterval(measure, 200);
    window.addEventListener("resize", measure);
    window.addEventListener("scroll", measure, true);
    return () => {
      window.clearInterval(timer);
      window.removeEventListener("resize", measure);
      window.removeEventListener("scroll", measure, true);
    };
  }, [plan, onScreen]);

  useLayoutEffect(() => {
    const height = balloon.current?.offsetHeight ?? 0;
    if (height !== balloonHeight) setBalloonHeight(height);
  });

  if (!step || !plan || !guide.data) return null;

  const index = guide.data.steps.indexOf(step);
  const total = guide.data.steps.length;
  const last = index === total - 1;
  const body = t(`guide.${MESSAGES[step]}${onScreen ? "Here" : "Go"}`);

  const viewportWidth = window.innerWidth;
  const viewportHeight = window.innerHeight;
  const width = Math.min(BALLOON_WIDTH, viewportWidth - EDGE * 2);
  let style: React.CSSProperties;
  let arrow: { side: "left" | "top" | "bottom"; offset: number } | null = null;
  if (!anchor) {
    style = { right: EDGE, bottom: EDGE, width };
  } else {
    const centerX = anchor.left + anchor.width / 2;
    const centerY = anchor.top + anchor.height / 2;
    const fitsRight = anchor.left + anchor.width + GAP + width + EDGE <= viewportWidth;
    // A menu entry is a narrow thing at the left edge: the balloon goes beside
    // it. Anything on a screen gets it underneath, or above when it would not fit.
    if (!onScreen && fitsRight) {
      const top = Math.max(EDGE, Math.min(centerY - 28, viewportHeight - balloonHeight - EDGE));
      style = { left: anchor.left + anchor.width + GAP, top, width };
      arrow = { side: "left", offset: Math.max(14, Math.min(centerY - top, balloonHeight - 14)) };
    } else {
      const left = Math.max(EDGE, Math.min(centerX - width / 2, viewportWidth - width - EDGE));
      const below = anchor.top + anchor.height + GAP + balloonHeight + EDGE <= viewportHeight;
      const top = below
        ? anchor.top + anchor.height + GAP
        : Math.max(EDGE, anchor.top - GAP - balloonHeight);
      style = { left, top, width };
      arrow = { side: below ? "top" : "bottom", offset: Math.max(14, Math.min(centerX - left, width - 14)) };
    }
  }

  return createPortal(
    <>
      {anchor && (
        <div
          aria-hidden
          className="tour-ring pointer-events-none fixed z-[70] rounded-lg"
          style={{ left: anchor.left - 4, top: anchor.top - 4, width: anchor.width + 8, height: anchor.height + 8 }}
        />
      )}
      <div
        ref={balloon}
        role="status"
        data-testid="first-run-guide"
        data-guide-step={step}
        className={`fixed z-[71] rounded-xl border border-iris/60 bg-raised p-3.5 text-fg shadow-2xl ${anchor ? "tour-float" : ""}`}
        style={style}
      >
        {arrow && (
          <span
            aria-hidden
            className="absolute h-3 w-3 rotate-45 border-iris/60 bg-raised"
            style={
              arrow.side === "left"
                ? { left: -7, top: arrow.offset - 6, borderLeftWidth: 1, borderBottomWidth: 1 }
                : arrow.side === "top"
                  ? { top: -7, left: arrow.offset - 6, borderLeftWidth: 1, borderTopWidth: 1 }
                  : { bottom: -7, left: arrow.offset - 6, borderRightWidth: 1, borderBottomWidth: 1 }
            }
          />
        )}
        <p className="font-mono text-[0.62rem] uppercase tracking-wider text-iris">
          {t("guide.progress", { current: index + 1, total })}
        </p>
        <p className="mt-1 font-display text-sm font-semibold">{t(`guide.${MESSAGES[step]}Title`)}</p>
        <p className="mt-1 text-xs leading-relaxed text-muted">{body}</p>
        <div className="mt-3 flex items-center justify-between gap-3 text-xs">
          <button
            type="button"
            className="text-faint underline-offset-2 hover:text-muted hover:underline"
            onClick={() => void actions.dismiss()}
          >
            {t("guide.hide")}
          </button>
          <button
            type="button"
            className="rounded-md border border-line px-2 py-1 text-muted hover:border-line-strong hover:text-fg"
            data-testid="first-run-guide-skip"
            onClick={() => void actions.complete(step)}
          >
            {t(last ? "guide.finish" : "guide.skip")}
          </button>
        </div>
      </div>
    </>,
    document.body,
  );
}
