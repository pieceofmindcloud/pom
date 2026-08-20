import type { TransferJob } from "./types";

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

/** Por quanto tempo o desfecho de uma transferência fica no sino. */
export const RECENTLY_FINISHED_MS = 5 * 60 * 1000;

/**
 * Terminou agora há pouco — o que o sino ainda precisa mostrar.
 *
 * O sino listava só o que estava em andamento, então "concluída", "falhou" e
 * "recusada" sumiam da tela no instante em que aconteciam: quem recebia via a
 * barra andar e depois nada, e quem enviava não via desfecho nenhum. O
 * resultado só sobrevivia na tela de Transferências, que foi exatamente o
 * relato de campo.
 *
 * `finished_at` ausente é um nó de versão anterior, que não carimba a hora:
 * ali não dá para saber se acabou agora ou ontem, e inventar "agora" encheria
 * o sino de transferências velhas a cada carregamento da página.
 */
export function isRecentlyFinished(job: TransferJob, now: number = Date.now()) {
  if (!isFinishedTransfer(job.state)) return false;
  if (!job.finished_at) return false;
  const at = Date.parse(job.finished_at);
  if (Number.isNaN(at)) return false;
  return now - at < RECENTLY_FINISHED_MS;
}

/** O que o sino mostra: o que está andando, mais o que acabou de terminar. */
export function isWorthNotifying(job: TransferJob, now: number = Date.now()) {
  return isActiveTransfer(job.state) || isRecentlyFinished(job, now);
}
