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

  useEffect(() => {
    if (still) {
      setOpen(true);
      return;
    }
    const timer = window.setTimeout(() => {
      if (open) {
        setOpen(false);
      } else {
        setPhrase((current) => (current + 1) % POM_PHRASES.length);
        setOpen(true);
      }
    }, open ? OPEN_MS : CLOSED_MS);
    return () => window.clearTimeout(timer);
  }, [open, still]);

  const words = POM_PHRASES[still ? 0 : phrase];
  return (
    <motion.div
      className="pom-chat-hero"
      data-testid="chat-hero"
      initial={{ opacity: 0, y: 12 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0, y: -24, scale: 0.97, transition: { duration: 0.28 } }}
      transition={{ duration: 0.5, ease: [0.22, 1, 0.36, 1] }}
    >
      <img className="pom-chat-hero-mark" src={theme === "dark" ? markOnDark : markOnLight} alt="POM" width={815} height={320} />
      <p className="pom-chat-hero-sub" aria-label={words.join(" ")} data-testid="chat-hero-phrase" data-open={open}>
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
      </p>
      <span className="pom-login-link pom-chat-hero-link" aria-hidden="true"><i /><i /><i /></span>
      {children}
    </motion.div>
  );
}
