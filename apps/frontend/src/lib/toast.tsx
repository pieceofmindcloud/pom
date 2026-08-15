/**
 * Confirmation that something happened.
 *
 * Every action in this panel reaches across a network to another machine, so
 * "it worked" and "it did not" have to be said out loud — a button that quietly
 * returns to rest is indistinguishable from one that did nothing.
 */
import { AnimatePresence, motion } from "framer-motion";
import { AlertTriangle, CheckCircle2, Info, X, XCircle } from "lucide-react";
import { createContext, useCallback, useContext, useMemo, useState } from "react";
import { useI18n } from "../i18n/provider";

/**
 * `warning` is the level this panel kept needing and did not have: the action
 * did work, so calling it an error lies, but the cluster came out of it worse
 * than it went in — a released slice nobody picked up is the case that named it.
 */
type Kind = "success" | "error" | "warning" | "info";

interface Toast {
  id: number;
  kind: Kind;
  title: string;
  detail?: string;
}

const ToastContext = createContext<{
  push: (kind: Kind, title: string, detail?: string) => void;
}>({ push: () => {} });

let nextId = 1;

export function ToastProvider({ children }: { children: React.ReactNode }) {
  const { t } = useI18n();
  const [toasts, setToasts] = useState<Toast[]>([]);

  const dismiss = useCallback(
    (id: number) => setToasts((t) => t.filter((x) => x.id !== id)),
    [],
  );

  const push = useCallback(
    (kind: Kind, title: string, detail?: string) => {
      const id = nextId++;
      setToasts((t) => [...t.slice(-3), { id, kind, title, detail }]);
      // Errors stay long enough to be read and copied; the rest step aside.
      // Bad news stays on screen long enough to be read, not just glimpsed.
      const linger = kind === "error" || kind === "warning";
      window.setTimeout(() => dismiss(id), linger ? 9000 : 4200);
    },
    [dismiss],
  );

  const value = useMemo(() => ({ push }), [push]);

  return (
    <ToastContext.Provider value={value}>
      {children}
      <div className="pointer-events-none fixed bottom-5 right-5 z-[60] flex w-[min(24rem,calc(100vw-2.5rem))] flex-col gap-2">
        <AnimatePresence initial={false}>
          {toasts.map((toast) => (
            <motion.div
              key={toast.id}
              layout
              initial={{ opacity: 0, y: 14, scale: 0.97 }}
              animate={{ opacity: 1, y: 0, scale: 1 }}
              exit={{ opacity: 0, x: 24, scale: 0.97 }}
              transition={{ duration: 0.22, ease: [0.22, 1, 0.36, 1] }}
              className="pointer-events-auto flex items-start gap-3 rounded-xl border border-line bg-surface px-3.5 py-3 shadow-lift"
            >
              {toast.kind === "success" && (
                <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0 text-aqua" />
              )}
              {toast.kind === "error" && (
                <XCircle className="mt-0.5 h-4 w-4 shrink-0 text-rose" />
              )}
              {toast.kind === "warning" && (
                <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-amber" />
              )}
              {toast.kind === "info" && <Info className="mt-0.5 h-4 w-4 shrink-0 text-sky" />}
              <div className="min-w-0 flex-1">
                <p className="text-sm font-medium text-fg">{toast.title}</p>
                {toast.detail && (
                  <p className="break-path mt-0.5 text-xs leading-relaxed text-muted">
                    {toast.detail}
                  </p>
                )}
              </div>
              <button
                onClick={() => dismiss(toast.id)}
                className="-m-1 rounded p-1 text-faint transition-colors hover:text-fg"
                aria-label={t("common.dismiss")}
              >
                <X className="h-3.5 w-3.5" />
              </button>
            </motion.div>
          ))}
        </AnimatePresence>
      </div>
    </ToastContext.Provider>
  );
}

export function useToast() {
  const { push } = useContext(ToastContext);
  return useMemo(
    () => ({
      success: (title: string, detail?: string) => push("success", title, detail),
      error: (title: string, detail?: string) => push("error", title, detail),
      warning: (title: string, detail?: string) => push("warning", title, detail),
      info: (title: string, detail?: string) => push("info", title, detail),
      /** Report whatever the node said went wrong, verbatim. */
      fromError: (title: string, e: unknown) =>
        push("error", title, e instanceof Error ? e.message : String(e)),
    }),
    [push],
  );
}
