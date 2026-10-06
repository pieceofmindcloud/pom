/**
 * Quão rápida é a fatia de uma máquina, comparada com as outras do mesmo
 * modelo.
 *
 * A comparação é relativa, e não contra um número fixo, porque o tok/s de uma
 * fatia depende de quantas camadas ela carrega e de qual modelo é: 40 tok/s é
 * ótimo para metade de um 70B e péssimo para um quarto de um 8B. O que o
 * operador precisa ver é quem segura o token na pipeline - o tempo de um token
 * é a soma das fatias, e a mais lenta é a que mais custa.
 *
 * Com uma fatia só medida não há com quem comparar: o tom é `solo`, neutro,
 * em vez de um verde que afirmaria o que ninguém mediu.
 */
export type SpeedTone = "fast" | "medium" | "slow" | "solo" | "unknown";

/** A partir de quanto da fatia mais rápida uma fatia ainda é "rápida". */
export const FAST_RATIO = 0.75;
/** Abaixo disso, a fatia é "lenta": segura o token. */
export const MEDIUM_RATIO = 0.4;

function measured(value: number | null | undefined): value is number {
  return typeof value === "number" && Number.isFinite(value) && value > 0;
}

export type SpeedRating = { tone: SpeedTone; ratio: number | null };

export function rateSpeed(value: number | null | undefined, peers: Array<number | null | undefined>): SpeedRating {
  if (!measured(value)) return { tone: "unknown", ratio: null };
  const values = peers.filter(measured);
  if (values.length < 2) return { tone: "solo", ratio: null };
  const best = Math.max(value, ...values);
  const ratio = value / best;
  const tone = ratio >= FAST_RATIO ? "fast" : ratio >= MEDIUM_RATIO ? "medium" : "slow";
  return { tone, ratio };
}
