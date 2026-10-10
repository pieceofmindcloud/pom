import { motion, useReducedMotion } from "framer-motion";
import { useEffect, useState } from "react";
import markOnDark from "../assets/pom-mark-dark.png";
import markOnLight from "../assets/pom-mark-light.png";
import { useTheme } from "../lib/theme";

/**
 * What the three letters stand for, in turn. Each phrase keeps the initials
 * P, O and M, so the line opens from "P O M" into the words and closes back.
 */
export const POM_PHRASES: string[][] = [
  ["PIECE", "OF", "MIND"],
  ["PLACE", "OF", "MIND"],
  ["POWER", "OF", "MACHINES"],
  ["PLATFORM", "OF", "MIND"],
];

/**
 * The entrance: a pause on the empty screen, then the mark rises from below,
 * then the line and the nodes follow it. Seconds.
 */
const MARK_DELAY = 0.7;
const MARK_RISE = 1.1;
const AFTER_MARK = MARK_DELAY + MARK_RISE * 0.7;

/** How long the line rests as "P O M" and as the open phrase, in ms. */
const CLOSED_MS = 1400;
const OPEN_MS = 2600;

/**
 * The Chat's opening screen above an empty conversation: the POM mark
 * breathing, the line that spells what POM stands for, and the three linked
 * nodes of the access gate and the wizard. Under reduced motion it holds the
 * first phrase, open, with nothing moving.
 */
export function ChatHero({ children }: { children?: React.ReactNode }) {
  const { theme } = useTheme();
  const still = useReducedMotion() ?? false;
  const [phrase, setPhrase] = useState(0);
  const [open, setOpen] = useState(still);
  // The phrases start once the line is on screen.
  const [entered, setEntered] = useState(still);

  useEffect(() => {
    if (still || entered) return;
    const timer = window.setTimeout(() => setEntered(true), AFTER_MARK * 1000);
    return () => window.clearTimeout(timer);
  }, [entered, still]);

  useEffect(() => {
    if (still) {
      setOpen(true);
      return;
    }
    if (!entered) return;
    const timer = window.setTimeout(() => {
      if (open) {
        setOpen(false);
      } else {
        setPhrase((current) => (current + 1) % POM_PHRASES.length);
        setOpen(true);
      }
    }, open ? OPEN_MS : CLOSED_MS);
    return () => window.clearTimeout(timer);
  }, [entered, open, still]);

  const words = POM_PHRASES[still ? 0 : phrase];
  return (
    <motion.div
      className="pom-chat-hero"
      data-testid="chat-hero"
      exit={{ opacity: 0, y: -24, scale: 0.97, transition: { duration: 0.28 } }}
    >
      <motion.img
        className="pom-chat-hero-mark"
        src={theme === "dark" ? markOnDark : markOnLight}
        alt="POM"
        width={815}
        height={320}
        data-testid="chat-hero-mark"
        initial={still ? false : { opacity: 0, y: 90 }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ delay: MARK_DELAY, duration: MARK_RISE, ease: [0.16, 1, 0.3, 1] }}
      />
      <motion.p
        initial={still ? false : { opacity: 0, y: 10 }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ delay: AFTER_MARK, duration: 0.6, ease: [0.22, 1, 0.36, 1] }}
        className="pom-chat-hero-sub" aria-label={words.join(" ")} data-testid="chat-hero-phrase" data-open={open}>
        {words.map((word, index) => (
          <span key={`${phrase}-${index}`} className="pom-chat-hero-word" aria-hidden="true">
            <span className="pom-chat-hero-initial">{word[0]}</span>
            <motion.span
              className="pom-chat-hero-rest"
              initial={false}
              animate={open ? { maxWidth: "12ch", opacity: 1 } : { maxWidth: "0ch", opacity: 0 }}
              transition={{ duration: still ? 0 : 0.55, ease: [0.22, 1, 0.36, 1], delay: open ? index * 0.12 : (words.length - 1 - index) * 0.08 }}
            >
              {word.slice(1)}
            </motion.span>
          </span>
        ))}
      </motion.p>
      <motion.span
        initial={still ? false : { opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: AFTER_MARK + 0.3, duration: 0.6 }}
        className="pom-login-link pom-chat-hero-link"
        aria-hidden="true"
      ><i /><i /><i /></motion.span>
      {children}
    </motion.div>
  );
}
