import { motion, useReducedMotion } from "framer-motion";
import { useId } from "react";
import markOnDark from "../assets/pom-mark-dark.png";
import markOnLight from "../assets/pom-mark-light.png";
import { useTheme } from "../lib/theme";

/** -1 a blank (hole), 0 a straight border, 1 a tab sticking out. */
type Edge = -1 | 0 | 1;

interface Piece {
  col: number;
  row: number;
  /** Clockwise: top, right, bottom, left. */
  edges: [Edge, Edge, Edge, Edge];
  /** Where the piece flies in from, in cells, and its spin on the way. */
  from: { x: number; y: number; rotate: number };
  color: string;
}

const CELL = 64;
const PAD = 22;
const SIZE = CELL * 2 + PAD * 2;

/*
 * A 2x2 puzzle. Neighbours agree on their shared edge (one tab, one blank),
 * so the three pieces lock together and leave the bottom-right hole in the
 * exact shape of the piece that is missing.
 */
const PIECES: Piece[] = [
  { col: 0, row: 0, edges: [0, 1, -1, 0], from: { x: -1.6, y: -1.2, rotate: -28 }, color: "#4f9bff" },
  { col: 1, row: 0, edges: [0, 0, 1, -1], from: { x: 1.7, y: -1.1, rotate: 24 }, color: "#35c9f0" },
  { col: 0, row: 1, edges: [1, -1, 0, 0], from: { x: -1.5, y: 1.4, rotate: 18 }, color: "#2fdcc8" },
];
const MISSING: Piece = { col: 1, row: 1, edges: [-1, 0, 0, 1], from: { x: 0, y: 0, rotate: 0 }, color: "" };

/** The outline of one piece, its edges walked clockwise with knobs where they have one. */
export function piecePath(piece: Pick<Piece, "col" | "row" | "edges">): string {
  const x = PAD + piece.col * CELL;
  const y = PAD + piece.row * CELL;
  const corners: [number, number][] = [[x, y], [x + CELL, y], [x + CELL, y + CELL], [x, y + CELL]];
  let path = `M${x} ${y}`;
  for (let side = 0; side < 4; side += 1) {
    const [x0, y0] = corners[side];
    const [x1, y1] = corners[(side + 1) % 4];
    const dx = (x1 - x0) / CELL;
    const dy = (y1 - y0) / CELL;
    // Outward normal of a clockwise edge in screen coordinates.
    const nx = dy;
    const ny = -dx;
    const h = piece.edges[side] * 0.24;
    const at = (t: number, height: number) => `${(x0 + dx * CELL * t + nx * CELL * height).toFixed(1)} ${(y0 + dy * CELL * t + ny * CELL * height).toFixed(1)}`;
    if (h === 0) {
      path += ` L${at(1, 0)}`;
      continue;
    }
    path += ` L${at(0.38, 0)}`;
    path += ` C${at(0.32, h * 0.55)} ${at(0.3, h)} ${at(0.5, h)}`;
    path += ` C${at(0.7, h)} ${at(0.68, h * 0.55)} ${at(0.62, 0)}`;
    path += ` L${at(1, 0)}`;
  }
  return `${path} Z`;
}

/**
 * Three pieces of a puzzle fly in and lock together around the fourth.
 * `missing` (the Chat with no model served) leaves its slot empty with a "?";
 * `paused` (a plugin whose process is stopped) has the fourth piece in place
 * but grey, dashed and marked with a pause sign, breathing slowly; `loading`
 * (a plugin screen still opening, or a plugin installing) has the fourth piece
 * hovering over its slot, about to drop, and assembles faster because the host
 * opens the screen after a short wait; `failed` (the plugin's install or start
 * failed) has it sitting in its slot, rose and still; `done` (the plugin
 * finished installing) drops it into its slot. The words under it say what that
 * piece is.
 */
const TIMING = {
  /** The slow set: the piece is not coming, so there is no hurry. */
  still_waiting: { piece: 0.35, gap: 0.35, duration: 0.9, slot: 1.9, text: 1.8, breathe: 2.2 },
  /** The loading set: the host reveals the screen within a couple of seconds. */
  arriving: { piece: 0.1, gap: 0.18, duration: 0.7, slot: 0.45, text: 0.55, breathe: 1.6 },
};

export function MissingPiece({ title, detail, variant = "missing", compact = false, children }: {
  title: string;
  detail?: string;
  variant?: "missing" | "paused" | "loading" | "failed" | "done";
  /** A smaller mark, puzzle and title, for a card inside a screen rather than a whole screen. */
  compact?: boolean;
  children?: React.ReactNode;
}) {
  const still = useReducedMotion() ?? false;
  const paused = variant === "paused";
  const loading = variant === "loading";
  const failed = variant === "failed";
  const done = variant === "done";
  const timing = loading || failed || done ? TIMING.arriving : TIMING.still_waiting;
  const centre = PAD + CELL * 1.5;
  const { theme } = useTheme();
  const glow = useId().replace(/:/g, "");
  return (
    <motion.div
      className="flex flex-col items-center text-center"
      data-testid={
        paused ? "paused-piece" : loading ? "plugin-loading-piece" : failed ? "plugin-loader-failed" : done ? "plugin-loader-done" : "chat-missing-piece"
      }
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      transition={{ duration: 0.4 }}
    >
      <motion.img
        className={compact ? "pom-chat-hero-mark !mb-1 !w-[min(84px,30vw)]" : "pom-chat-hero-mark !mb-2 !w-[min(150px,40vw)]"}
        src={theme === "dark" ? markOnDark : markOnLight}
        alt="POM"
        width={815}
        height={320}
        initial={still ? false : { opacity: 0, y: 40 }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ duration: 0.9, ease: [0.16, 1, 0.3, 1] }}
      />
      <svg viewBox={`0 0 ${SIZE} ${SIZE}`} width={compact ? SIZE * 0.55 : SIZE} height={compact ? SIZE * 0.55 : SIZE} className="max-w-[60vw] overflow-visible" role="img" aria-label={title}>
        <defs>
          <filter id={`glow-${glow}`} x="-30%" y="-30%" width="160%" height="160%">
            <feGaussianBlur stdDeviation="4" result="blur" />
            <feMerge><feMergeNode in="blur" /><feMergeNode in="SourceGraphic" /></feMerge>
          </filter>
        </defs>
        {/* The hole where the model goes, or the stopped piece sitting in it. */}
        <motion.path
          d={piecePath(MISSING)}
          fill={paused ? "rgb(var(--muted) / 0.22)" : "rgb(var(--iris) / 0.06)"}
          stroke="rgb(var(--muted))"
          strokeWidth={1.6}
          strokeDasharray="5 5"
          data-testid="missing-piece-slot"
          data-variant={variant}
          initial={still ? false : { opacity: 0 }}
          animate={still ? { opacity: 0.9 } : { opacity: paused ? [0.55, 0.9, 0.55] : [0.35, 0.95, 0.35] }}
          transition={still ? undefined : { delay: timing.slot, duration: paused ? 3.2 : timing.breathe, repeat: Infinity, ease: "easeInOut" }}
        />
        {paused ? (
          <motion.g
            fill="rgb(var(--muted))"
            initial={still ? false : { opacity: 0, scale: 0.6 }}
            animate={{ opacity: 0.85, scale: 1 }}
            transition={{ delay: 2.1, duration: 0.5 }}
            style={{ transformOrigin: `${centre}px ${centre}px` }}
            data-testid="paused-piece-sign"
          >
            <rect x={centre - 10} y={centre - 12} width={7} height={24} rx={2} />
            <rect x={centre + 3} y={centre - 12} width={7} height={24} rx={2} />
          </motion.g>
        ) : loading ? (
          /* The fourth piece, hovering over its slot: it is on its way. */
          <motion.g
            data-testid="loading-piece"
            fill="rgb(var(--iris))"
            fillOpacity={0.5}
            stroke="rgb(var(--surface))"
            strokeWidth={2}
            filter={`url(#glow-${glow})`}
            initial={still ? false : { opacity: 0, y: -14, rotate: -5 }}
            animate={still ? { opacity: 0.7 } : { opacity: [0.4, 0.8, 0.4], y: [0, -7, 0], rotate: [-4, 4, -4] }}
            transition={still ? undefined : { delay: timing.slot, duration: 1.8, repeat: Infinity, ease: "easeInOut" }}
            style={{ transformBox: "fill-box", transformOrigin: "center" }}
          >
            <path d={piecePath(MISSING)} />
          </motion.g>
        ) : failed || done ? (
          /* The fourth piece in its slot: rose and still when it failed, dropped in place when it is done. */
          <motion.g
            data-testid={failed ? "failed-piece" : "done-piece"}
            fill={failed ? "rgb(var(--rose))" : "rgb(var(--iris))"}
            fillOpacity={0.88}
            stroke="rgb(var(--surface))"
            strokeWidth={2}
            filter={`url(#glow-${glow})`}
            initial={still || failed ? false : { opacity: 0.6, y: -7, rotate: 4 }}
            animate={{ opacity: 1, y: 0, rotate: 0 }}
            transition={{ duration: 0.45, ease: [0.16, 1, 0.3, 1] }}
            style={{ transformBox: "fill-box", transformOrigin: "center" }}
          >
            <path d={piecePath(MISSING)} />
          </motion.g>
        ) : (
          <motion.text
            x={centre}
            y={centre + 9}
            textAnchor="middle"
            fontSize={26}
            fontWeight={700}
            fill="rgb(var(--muted))"
            initial={still ? false : { opacity: 0, scale: 0.6 }}
            animate={{ opacity: 0.8, scale: 1 }}
            transition={{ delay: 2.1, duration: 0.5 }}
            style={{ transformOrigin: `${centre}px ${centre}px` }}
          >
            ?
          </motion.text>
        )}
        {PIECES.map((piece, index) => (
          <motion.path
            key={`${piece.col}-${piece.row}`}
            d={piecePath(piece)}
            fill={piece.color}
            fillOpacity={0.88}
            stroke="rgb(var(--surface))"
            strokeWidth={2}
            filter={`url(#glow-${glow})`}
            data-testid="puzzle-piece"
            initial={still ? false : { x: piece.from.x * CELL, y: piece.from.y * CELL, rotate: piece.from.rotate, opacity: 0 }}
            animate={{ x: 0, y: 0, rotate: 0, opacity: 1 }}
            transition={{ delay: timing.piece + index * timing.gap, duration: timing.duration, ease: [0.16, 1, 0.3, 1] }}
            style={{ transformBox: "fill-box", transformOrigin: "center" }}
          />
        ))}
      </svg>
      <motion.div
        initial={still ? false : { opacity: 0, y: 8 }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ delay: timing.text, duration: 0.5 }}
        className={compact ? "mt-2 max-w-md" : "mt-4 max-w-md"}
      >
        <p className={compact ? "font-display text-base font-semibold text-fg" : "font-display text-lg font-semibold text-fg"}>{title}</p>
        {detail ? (
          <p className={compact ? "mt-1 text-[0.8rem] leading-relaxed text-muted" : "mt-1.5 text-sm leading-relaxed text-muted"}>{detail}</p>
        ) : null}
        {children}
      </motion.div>
    </motion.div>
  );
}
