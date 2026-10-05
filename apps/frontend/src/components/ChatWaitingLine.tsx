import { AnimatePresence, motion } from "framer-motion";
import { useEffect, useRef, useState } from "react";
import { useI18n } from "../i18n/provider";
import { WAITING_ROTATE_MS, waitingMessageKey } from "../lib/chatWaiting";

/**
 * What the Chat shows between sending a message and the first token: three
 * pulsing dots and a line that changes as the wait goes on. `asleep` is the
 * deployment unloaded for idleness and loading again, which is the long wait
 * worth explaining. See `lib/chatWaiting.ts` for the message sets.
 */
export function ChatWaitingLine({ asleep }: { asleep: boolean }) {
  const { t } = useI18n();
  const started = useRef(Date.now());
  // One random start per wait, so consecutive waits open differently.
  const seed = useRef(Math.floor(Math.random() * 1000));
  const [elapsedMs, setElapsedMs] = useState(0);

  useEffect(() => {
    const timer = setInterval(() => setElapsedMs(Date.now() - started.current), WAITING_ROTATE_MS / 7);
    return () => clearInterval(timer);
  }, []);

  const key = waitingMessageKey({ asleep, elapsedMs, seed: seed.current });
  return (
    <div
      className="flex items-center gap-2 text-sm text-muted"
      role="status"
      aria-label={t("chat.through")}
      data-testid="chat-waiting"
      data-waiting={asleep ? "wake" : "thinking"}
    >
      <span className="flex gap-1" aria-hidden="true">
        {[0, 1, 2].map((i) => (
          <span
            key={i}
            className="h-1.5 w-1.5 rounded-full bg-iris"
            style={{
              animation: "pulse-soft 1.2s ease-in-out infinite",
              animationDelay: `${i * 0.16}s`,
            }}
          />
        ))}
      </span>
      <AnimatePresence mode="wait" initial={false}>
        <motion.span
          key={key}
          initial={{ opacity: 0, y: 3 }}
          animate={{ opacity: 1, y: 0 }}
          exit={{ opacity: 0, y: -3 }}
          transition={{ duration: 0.25 }}
        >
          {t(key)}
        </motion.span>
      </AnimatePresence>
    </div>
  );
}
