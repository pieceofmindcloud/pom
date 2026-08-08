/**
 * A stop before an action that reaches other machines.
 *
 * Leaving the network and unloading a model both interrupt whatever the cluster
 * is serving, and neither is undone by clicking again — so they are asked about
 * by name, with the consequence spelled out.
 */
import { AnimatePresence, motion } from "framer-motion";
import { useEffect } from "react";
import { Button } from "./ui";

export function Confirm({
  open,
  title,
  description,
  confirmLabel,
  tone = "primary",
  busy = false,
  onConfirm,
  onCancel,
}: {
  open: boolean;
  title: string;
  description: React.ReactNode;
  confirmLabel: string;
  tone?: "primary" | "danger";
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onCancel();
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, onCancel]);

  return (
    <AnimatePresence>
      {open && (
        <motion.div
          className="fixed inset-0 z-[70] flex items-center justify-center p-4"
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.15 }}
        >
          <div
            className="absolute inset-0 bg-black/55 backdrop-blur-[2px]"
            onClick={onCancel}
          />
          <motion.div
            role="dialog"
            aria-modal="true"
            aria-label={title}
            initial={{ opacity: 0, y: 10, scale: 0.98 }}
            animate={{ opacity: 1, y: 0, scale: 1 }}
            exit={{ opacity: 0, y: 6, scale: 0.98 }}
            transition={{ duration: 0.2, ease: [0.22, 1, 0.36, 1] }}
            className="relative w-full max-w-md overflow-hidden rounded-xl border border-line bg-surface shadow-lift"
          >
            <div className="px-5 py-4">
              <h2 className="font-display text-base font-semibold text-fg">{title}</h2>
              <div className="mt-2 text-sm leading-relaxed text-muted">{description}</div>
            </div>
            <div className="flex justify-end gap-2 border-t border-line bg-raised px-5 py-3">
              <Button variant="ghost" onClick={onCancel} disabled={busy}>
                Cancelar
              </Button>
              <Button variant={tone} loading={busy} onClick={onConfirm}>
                {confirmLabel}
              </Button>
            </div>
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
