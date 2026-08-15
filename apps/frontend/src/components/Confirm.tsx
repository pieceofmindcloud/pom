/**
 * A stop before an action that reaches other machines.
 *
 * Leaving the network and unloading a model both interrupt whatever the cluster
 * is serving, and neither is undone by clicking again — so they are asked about
 * by name, with the consequence spelled out.
 *
 * `Modal` é a casca — véu, painel, cabeçalho e rodapé —, separada de `Confirm`
 * porque nem tudo que aparece assim é uma pergunta: o relatório de uma
 * desimplantação usa a mesma janela e não tem "cancelar". Ter as duas coisas
 * numa função só levaria a um botão "cancelar" que não cancela nada.
 */
import { AnimatePresence, motion } from "framer-motion";
import { useEffect, useId } from "react";
import { createPortal } from "react-dom";
import { Button } from "./ui";
import { formatMessage, getActiveLocale } from "../i18n/locales";

/**
 * Quem está por cima.
 *
 * Uma janela pode abrir outra - a confirmação de "Sair da rede" nasce de dentro
 * do card que a janela de detalhes mostra -, e todas escutam o Esc no
 * `document`. Sem esta pilha um Esc fecharia as duas de uma vez, e o operador
 * perderia a tela que estava lendo por ter desistido do que abriu de lá.
 */
const openModals: string[] = [];

export function Modal({
  open,
  title,
  children,
  footer,
  onClose,
  size = "md",
}: {
  open: boolean;
  title: string;
  children: React.ReactNode;
  footer: React.ReactNode;
  onClose: () => void;
  /** `lg` para conteúdo que é um painel inteiro, e não um parágrafo. */
  size?: "md" | "lg";
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

  // Vai para o `body`, não para onde o botão está.
  //
  // `position: fixed` deixa de valer contra a janela e passa a valer contra o
  // ancestral mais próximo que tenha `transform`, `filter` **ou**
  // `backdrop-filter`. A barra do topo e o cabeçalho do Painel têm
  // `backdrop-blur`, então uma confirmação aberta de lá era centralizada dentro
  // de uma faixa de ~76 px de altura: medido no navegador, a janela nascia em
  // `y = -66` numa viewport de 2029 px, com o título fora da tela. Era assim
  // desde antes deste botão existir, e é armadilha de quem vier depois - o
  // portal resolve para todo mundo de uma vez.
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
              (size === "lg" ? "max-w-2xl" : "max-w-md")
            }
          >
            <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
              <h2 className="font-display text-base font-semibold text-fg">{title}</h2>
              <div className="mt-2 text-sm leading-relaxed text-muted">{children}</div>
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
