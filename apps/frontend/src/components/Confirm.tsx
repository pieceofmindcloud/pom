/**
 * A stop before an action that reaches other machines.
 *
 * Leaving the network and unloading a model both interrupt whatever the cluster
 * is serving, and neither is undone by clicking again — so they are asked about
 * by name, with the consequence spelled out.
 *
 * `Modal` is the shell - veil, panel, header and footer -, separate from `Confirm`
 * because not everything that appears this way is a question: the report of an
 * undeploy uses the same window and has no "cancel". Having both things
 * in one function would lead to a "cancel" button that cancels nothing.
 */
import clsx from "clsx";
import { AnimatePresence, motion } from "framer-motion";
import { useEffect, useId } from "react";
import { createPortal } from "react-dom";
import { Button } from "./ui";
import { formatMessage, getActiveLocale } from "../i18n/locales";

/**
 * Who is on top.
 *
 * One window may open another - the "Leave the network" confirmation is born from inside
 * the card that the details window shows -, and all of them listen for Esc on
 * `document`. Without this stack one Esc would close both at once, and the operator
 * would lose the screen they were reading for having given up on what they opened from there.
 */
const openModals: string[] = [];

export function Modal({
  open,
  title,
  children,
  footer,
  onClose,
  size = "md",
  titleHidden = false,
}: {
  open: boolean;
  title: string;
  children: React.ReactNode;
  footer: React.ReactNode;
  onClose: () => void;
  /**
   * `lg` for content that is a whole panel, and not a paragraph; `full`
   * takes almost the whole window, for whoever asked to see everything.
   */
  size?: "md" | "lg" | "xl" | "full";
  /**
   * The title is still the window's name for screen readers, but disappears
   * from view when the content already opens with its own header - otherwise the name
   * appears twice, one on top of the other.
   */
  titleHidden?: boolean;
}) {
  const id = useId();

  useEffect(() => {
    if (!open) return;
    openModals.push(id);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && openModals[openModals.length - 1] === id) onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("keydown", onKey);
      const at = openModals.lastIndexOf(id);
      if (at >= 0) openModals.splice(at, 1);
    };
  }, [open, onClose, id]);

  // Goes to `body`, not to where the button is.
  //
  // `position: fixed` stops applying against the window and starts applying against the
  // nearest ancestor that has `transform`, `filter` **or**
  // `backdrop-filter`. The top bar and the Dashboard header have
  // `backdrop-blur`, so a confirmation opened from there was centered inside
  // a strip about 76 px tall: measured in the browser, the window was born at
  // `y = -66` in a viewport of 2029 px, with the title off screen. It was like this
  // since before this button existed, and it is a trap for whoever comes next - the
  // portal solves it for everyone at once.
  return createPortal(
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
            onClick={onClose}
          />
          <motion.div
            role="dialog"
            aria-modal="true"
            aria-label={title}
            initial={{ opacity: 0, y: 10, scale: 0.98 }}
            animate={{ opacity: 1, y: 0, scale: 1 }}
            exit={{ opacity: 0, y: 6, scale: 0.98 }}
            transition={{ duration: 0.2, ease: [0.22, 1, 0.36, 1] }}
            className={
              "relative flex w-full max-h-[calc(100vh-2rem)] flex-col overflow-hidden rounded-xl border border-line bg-surface shadow-lift " +
              (size === "full" ? "h-[calc(100vh-2rem)] max-w-[min(96vw,90rem)]" : size === "xl" ? "max-w-4xl" : size === "lg" ? "max-w-2xl" : "max-w-md")
            }
          >
            <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
              <h2 className={titleHidden ? "sr-only" : "font-display text-base font-semibold text-fg"}>{title}</h2>
              <div className={clsx("text-sm leading-relaxed text-muted", !titleHidden && "mt-2")}>{children}</div>
            </div>
            <div className="flex justify-end gap-2 border-t border-line bg-raised px-5 py-3">
              {footer}
            </div>
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>,
    document.body,
  );
}

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
  const cancelLabel = formatMessage(getActiveLocale(), "common.cancel");

  return (
    <Modal
      open={open}
      title={title}
      onClose={onCancel}
      footer={
        <>
          <Button variant="ghost" onClick={onCancel} disabled={busy}>
            {cancelLabel}
          </Button>
          <Button variant={tone} loading={busy} onClick={onConfirm}>
            {confirmLabel}
          </Button>
        </>
      }
    >
      {description}
    </Modal>
  );
}
