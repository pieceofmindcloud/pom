import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useLocation, useNavigate } from "react-router-dom";
import { useI18n } from "../i18n/provider";
import { useOptionalChatSessionController } from "../lib/chatStore";
import { currentGuideStep, GUIDE_MODEL_REPO, isWizardStep, useGuide, useGuideActions, type GuideStepId, type WizardStepId } from "../lib/guide";
import { useCluster, useLocalCatalog, useModelDownloads } from "../lib/queries";

interface StepTarget {
  selector: string;
  /** Message key (`guide.<hint>`) shown while this is what the balloon points at. */
  hint?: string;
}

interface StepPlan {
  /** Screen where the step is carried out. */
  path: string;
  /** Menu section that holds the link, when it sits inside one. */
  section?: string;
  /**
   * What to point at once the operator is on the screen, the furthest point
   * of the step first: the first one on screen wins, so the balloon follows
   * the operator through a step that takes several clicks.
   */
  targets: StepTarget[];
}

const REPO = GUIDE_MODEL_REPO.toLowerCase();
/** What a file of the guide's model is called, whatever its quantization. */
const MODEL_FILE = /qwen2\.5-0\.5b-instruct/i;

/** The steps that get a hint: the names are asked by the wizard before them. */
type HintStepId = Exclude<GuideStepId, WizardStepId>;

const PLAN: Record<HintStepId, StepPlan> = {
  download_model: {
    path: "/modelos/remotos",
    section: "nav.models",
    targets: [
      { selector: '[data-guide="model-download-confirm"]', hint: "downloadConfirm" },
      { selector: '[data-guide="model-file"][data-guide-quantization="q4_k_m"]', hint: "downloadFile" },
      { selector: '[data-guide="model-file"]', hint: "downloadFile" },
      { selector: `[data-guide-repo="${REPO}"]`, hint: "downloadOpen" },
      { selector: '[data-guide="model-search"]' },
    ],
  },
  deploy_model: {
    path: "/implantar",
    section: "nav.deployments",
    targets: [
      { selector: '[data-guide="deploy-running"]', hint: "deployWait" },
      { selector: '[data-guide="deploy-run"]', hint: "deployRun" },
      { selector: '[data-guide="deploy-whole"]', hint: "deployWhole" },
      { selector: '[data-guide="deploy-peer"]', hint: "deployPeer" },
    ],
  },
  chat: { path: "/chat", targets: [{ selector: '[data-guide="chat-input"]' }] },
  join: { path: "/redes-publicas", targets: [{ selector: '[data-guide="network-join"]' }] },
};

/** The message keys of each step: `guide.<name>Title`, `Go` and `Here`. */
const MESSAGES: Record<HintStepId, string> = {
  download_model: "download",
  deploy_model: "deploy",
  chat: "chat",
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
function findTarget(plan: StepPlan, onScreen: boolean): { element: HTMLElement; hint?: string } | null {
  if (onScreen) {
    for (const target of plan.targets) {
      const element = document.querySelector(target.selector);
      if (visible(element)) return { element, hint: target.hint };
    }
    return null;
  }
  const link = document.querySelector(`[data-guide-nav="${plan.path}"]`);
  if (visible(link)) return { element: link };
  if (!plan.section) return null;
  const section = document.querySelector(`[data-guide-section="${plan.section}"]`);
  return visible(section) ? { element: section } : null;
}

interface Anchor {
  left: number;
  top: number;
  width: number;
  height: number;
  hint?: string;
}

function sameAnchor(a: Anchor | null, b: Anchor | null): boolean {
  if (!a || !b) return a === b;
  return a.left === b.left && a.top === b.top && a.width === b.width && a.height === b.height && a.hint === b.hint;
}

/**
 * Steps that end by themselves: nobody presses "save" after a download, a
 * deployment or an answer, so each one watches the node for its outcome.
 * Mounted only while its step is the current one.
 */
function DownloadWatcher({ onDone, onRunning }: { onDone: () => void; onRunning: (running: boolean) => void }) {
  const catalog = useLocalCatalog();
  const downloads = useModelDownloads();
  const running = downloads.data.some((job) => job.state === "running" && job.repo_id?.toLowerCase() === REPO);
  useEffect(() => {
    onRunning(running);
    return () => onRunning(false);
  }, [running]);
  const present =
    (catalog.data?.models ?? []).some((model) => MODEL_FILE.test(model.name) || MODEL_FILE.test(model.path)) ||
    downloads.data.some((job) => job.state === "done" && job.repo_id?.toLowerCase() === REPO);
  useEffect(() => {
    if (present) onDone();
  }, [present]);
  return null;
}

function DeployWatcher({ onDone }: { onDone: () => void }) {
  const cluster = useCluster();
  const serving = Boolean(cluster.data?.active_model);
  useEffect(() => {
    if (serving) onDone();
  }, [serving]);
  return null;
}

function ChatWatcher({ onDone }: { onDone: () => void }) {
  const answered = Boolean(useOptionalChatSessionController()?.lastStreamResult);
  useEffect(() => {
    if (answered) onDone();
  }, [answered]);
  return null;
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
  const navigate = useNavigate();
  const guide = useGuide(enabled);
  const actions = useGuideActions();
  const current = enabled ? currentGuideStep(guide.data) : null;
  // While the wizard is on screen there is nothing to point at.
  const step = isWizardStep(current) ? null : current;
  const plan = step ? PLAN[step] : null;
  const onScreen = plan ? location.pathname === plan.path : false;
  const [anchor, setAnchor] = useState<Anchor | null>(null);
  const balloon = useRef<HTMLDivElement>(null);
  const [balloonHeight, setBalloonHeight] = useState(0);
  // While the model downloads there is nothing left to click: the balloon
  // stops pointing and says to wait.
  const [downloading, setDownloading] = useState(false);
  const waiting = step === "download_model" && downloading;

  // The target moves with scrolling, the menu opening and the screen
  // loading, and none of those tells us: follow it by looking again.
  useEffect(() => {
    if (!plan || waiting) {
      setAnchor(null);
      return;
    }
    const measure = () => {
      const target = findTarget(plan, onScreen);
      const rect = target?.element.getBoundingClientRect();
      const next = rect
        ? { left: Math.round(rect.left), top: Math.round(rect.top), width: Math.round(rect.width), height: Math.round(rect.height), hint: target?.hint }
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
  }, [plan, onScreen, waiting]);

  // The model to download is a search away: fill the search in once, when
  // the operator arrives, and leave it alone afterwards.
  const searched = useRef(false);
  useEffect(() => {
    if (step !== "download_model" || !onScreen || searched.current) return;
    searched.current = true;
    if (!new URLSearchParams(location.search).get("q")) {
      navigate(`${location.pathname}?q=${encodeURIComponent(GUIDE_MODEL_REPO)}`, { replace: true });
    }
  }, [step, onScreen]);

  useLayoutEffect(() => {
    const height = balloon.current?.offsetHeight ?? 0;
    if (height !== balloonHeight) setBalloonHeight(height);
  });

  if (!step || !plan || !guide.data) return null;

  const hinted = guide.data.steps.filter((id) => !isWizardStep(id));
  const index = hinted.indexOf(step);
  const total = hinted.length;
  const last = index === total - 1;
  const body = t(
    waiting ? "guide.downloadWait" : onScreen && anchor?.hint ? `guide.${anchor.hint}` : `guide.${MESSAGES[step]}${onScreen ? "Here" : "Go"}`,
    { model: GUIDE_MODEL_REPO },
  );
  const complete = () => void actions.complete(step);

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
      {step === "download_model" && <DownloadWatcher onDone={complete} onRunning={setDownloading} />}
      {step === "deploy_model" && <DeployWatcher onDone={complete} />}
      {step === "chat" && <ChatWatcher onDone={complete} />}
      {anchor && (
        <div
          aria-hidden
          className="tour-ring pointer-events-none fixed z-[71] rounded-lg"
          style={{ left: anchor.left - 4, top: anchor.top - 4, width: anchor.width + 8, height: anchor.height + 8 }}
        />
      )}
      <div
        ref={balloon}
        role="status"
        data-testid="first-run-guide"
        data-guide-step={step}
        className={`fixed z-[72] rounded-xl border border-iris/60 bg-raised p-3.5 text-fg shadow-2xl ${anchor ? "tour-float" : ""}`}
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
            onClick={complete}
          >
            {t(last ? "guide.finish" : "guide.skip")}
          </button>
        </div>
      </div>
    </>,
    document.body,
  );
}
