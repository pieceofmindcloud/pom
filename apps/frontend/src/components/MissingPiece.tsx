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
 * The Chat's screen when the network serves no model: three pieces of a
 * puzzle fly in and lock together, and the fourth slot stays empty, pulsing.
 * The words under it say what the missing piece is.
 */
export function MissingPiece({ title, detail, children }: { title: string; detail: string; children?: React.ReactNode }) {
  const still = useReducedMotion() ?? false;
  const { theme } = useTheme();
  const glow = useId().replace(/:/g, "");
  return (
    <motion.div
      className="flex flex-col items-center text-center"
      data-testid="chat-missing-piece"
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      transition={{ duration: 0.4 }}
    >
      <motion.img
        className="pom-chat-hero-mark !mb-2 !w-[min(150px,40vw)]"
        src={theme === "dark" ? markOnDark : markOnLight}
        alt="POM"
        width={815}
        height={320}
        initial={still ? false : { opacity: 0, y: 40 }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ duration: 0.9, ease: [0.16, 1, 0.3, 1] }}
      />
      <svg viewBox={`0 0 ${SIZE} ${SIZE}`} width={SIZE} height={SIZE} className="max-w-[60vw] overflow-visible" role="img" aria-label={title}>
        <defs>
          <filter id={`glow-${glow}`} x="-30%" y="-30%" width="160%" height="160%">
            <feGaussianBlur stdDeviation="4" result="blur" />
            <feMerge><feMergeNode in="blur" /><feMergeNode in="SourceGraphic" /></feMerge>
          </filter>
        </defs>
        {/* The hole: where the model goes. */}
        <motion.path
          d={piecePath(MISSING)}
          fill="rgb(var(--iris) / 0.06)"
          stroke="rgb(var(--muted))"
          strokeWidth={1.6}
          strokeDasharray="5 5"
          data-testid="missing-piece-slot"
          initial={still ? false : { opacity: 0 }}
          animate={still ? { opacity: 0.9 } : { opacity: [0.35, 0.95, 0.35] }}
          transition={still ? undefined : { delay: 1.9, duration: 2.2, repeat: Infinity, ease: "easeInOut" }}
        />
        <motion.text
          x={PAD + CELL * 1.5}
          y={PAD + CELL * 1.5 + 9}
          textAnchor="middle"
          fontSize={26}
          fontWeight={700}
          fill="rgb(var(--muted))"
          initial={still ? false : { opacity: 0, scale: 0.6 }}
          animate={{ opacity: 0.8, scale: 1 }}
          transition={{ delay: 2.1, duration: 0.5 }}
          style={{ transformOrigin: `${PAD + CELL * 1.5}px ${PAD + CELL * 1.5}px` }}
        >
          ?
        </motion.text>
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
            transition={{ delay: 0.35 + index * 0.35, duration: 0.9, ease: [0.16, 1, 0.3, 1] }}
            style={{ transformBox: "fill-box", transformOrigin: "center" }}
          />
        ))}
      </svg>
      <motion.div
        initial={still ? false : { opacity: 0, y: 8 }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ delay: 1.8, duration: 0.5 }}
        className="mt-4 max-w-md"
      >
        <p className="font-display text-lg font-semibold text-fg">{title}</p>
        <p className="mt-1.5 text-sm leading-relaxed text-muted">{detail}</p>
        {children}
      </motion.div>
    </motion.div>
  );
}
