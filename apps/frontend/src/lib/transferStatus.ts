import type { TransferJob } from "./types";

export type TransferState =
  | "running"
  | "verifying"
  | "done"
  | "failed"
  | "cancelled";

export function isFinishedTransfer(state: TransferJob["state"]) {
  return state === "done" || state === "failed" || state === "cancelled";
}

export function isActiveTransfer(state: TransferJob["state"]) {
  return !isFinishedTransfer(state);
}

/**
 * Só uma transferência em andamento pode ser cancelada.
 *
 * Oferecer o botão numa que já terminou convidaria o operador a uma ação sem
 * efeito, e num download já verificado sugeriria que dá para desfazer o
 * arquivo que já está no disco — o que a rota não faz.
 */
export function canCancel(state: TransferJob["state"]) {
  return state === "running" || state === "verifying";
}
