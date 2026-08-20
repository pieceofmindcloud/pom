import { describe, expect, it } from "vitest";
import {
  canCancel,
  isActiveTransfer,
  isFinishedTransfer,
  isRecentlyFinished,
  isWorthNotifying,
  RECENTLY_FINISHED_MS,
} from "./transferStatus";
import type { TransferJob } from "./types";

describe("transfer status", () => {
  it("moves a verified download out of the active queue", () => {
    expect(isActiveTransfer("done")).toBe(false);
    expect(isFinishedTransfer("done")).toBe(true);
  });

  it("keeps an MD5 verification visibly in progress", () => {
    expect(isActiveTransfer("verifying")).toBe(true);
    expect(isFinishedTransfer("verifying")).toBe(false);
  });

  it("treats a failed transfer as finished", () => {
    expect(isActiveTransfer("failed")).toBe(false);
    expect(isFinishedTransfer("failed")).toBe(true);
  });

  /// Cancelada é uma transferência terminada, não uma em andamento: ela não
  /// pode ficar na fila ativa esperando um progresso que nunca vem.
  it("treats a cancelled transfer as finished", () => {
    expect(isActiveTransfer("cancelled")).toBe(false);
    expect(isFinishedTransfer("cancelled")).toBe(true);
  });
});

describe("canCancel", () => {
  it("permite cancelar só o que ainda está andando", () => {
    expect(canCancel("running")).toBe(true);
    expect(canCancel("verifying")).toBe(true);
    expect(canCancel("done")).toBe(false);
    expect(canCancel("failed")).toBe(false);
    expect(canCancel("cancelled")).toBe(false);
  });
});

/**
 * O sino listava só o que estava em andamento, e o desfecho sumia da tela no
 * instante em que acontecia: quem recebia via a barra andar e depois nada,
 * quem enviava não via aviso de aceite nem de recusa. O resultado só
 * sobrevivia na tela de Transferências — que foi exatamente o relato.
 */
describe("o que o sino ainda precisa mostrar", () => {
  const agora = Date.parse("2026-08-20T12:00:00Z");

  const job = (over: Partial<TransferJob>): TransferJob => ({
    id: "j",
    name: "f.gguf",
    direction: "receiving",
    peer_host: "atena",
    path: "/m/f.gguf",
    state: "running",
    bytes_done: 0,
    bytes_total: 10,
    bytes_per_sec: 0,
    eta_secs: null,
    detail: "",
    ...over,
  });

  it("mantém no sino a transferência que acabou de terminar", () => {
    const concluida = job({
      state: "done",
      finished_at: "2026-08-20T11:59:30Z",
    });
    expect(isRecentlyFinished(concluida, agora)).toBe(true);
    expect(isWorthNotifying(concluida, agora)).toBe(true);
  });

  it("mantém também a recusa que o outro lado mandou de volta", () => {
    const recusada = job({
      direction: "sending",
      state: "cancelled",
      detail: "recusado por ksia",
      finished_at: "2026-08-20T11:59:59Z",
    });
    expect(isWorthNotifying(recusada, agora)).toBe(true);
  });

  it("solta a que terminou faz tempo, para o sino não virar histórico", () => {
    const velha = job({
      state: "done",
      finished_at: new Date(agora - RECENTLY_FINISHED_MS - 1000).toISOString(),
    });
    expect(isRecentlyFinished(velha, agora)).toBe(false);
    expect(isWorthNotifying(velha, agora)).toBe(false);
  });

  /**
   * Um nó de versão anterior não carimba a hora. Ali não dá para saber se
   * acabou agora ou ontem, e supor "agora" encheria o sino de transferências
   * velhas a cada carregamento da página.
   */
  it("não inventa 'agora' para um nó que não carimba a hora", () => {
    expect(isWorthNotifying(job({ state: "done" }), agora)).toBe(false);
    expect(
      isWorthNotifying(job({ state: "done", finished_at: null }), agora),
    ).toBe(false);
    expect(
      isWorthNotifying(job({ state: "done", finished_at: "não é data" }), agora),
    ).toBe(false);
  });

  it("o que ainda anda continua no sino, com ou sem carimbo", () => {
    expect(isWorthNotifying(job({ state: "running" }), agora)).toBe(true);
    expect(isWorthNotifying(job({ state: "verifying" }), agora)).toBe(true);
  });
});
