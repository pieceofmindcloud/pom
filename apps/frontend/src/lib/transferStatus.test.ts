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
  it("allows cancelling only what is still in progress", () => {
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
describe("what the bell still needs to show", () => {
  const now = Date.parse("2026-08-20T12:00:00Z");

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

  it("keeps in the bell the transfer that just finished", () => {
    const completed = job({
      state: "done",
      finished_at: "2026-08-20T11:59:30Z",
    });
    expect(isRecentlyFinished(completed, now)).toBe(true);
    expect(isWorthNotifying(completed, now)).toBe(true);
  });

  it("also keeps the refusal the other side sent back", () => {
    const refused = job({
      direction: "sending",
      state: "cancelled",
      detail: "recusado por ksia",
      finished_at: "2026-08-20T11:59:59Z",
    });
    expect(isWorthNotifying(refused, now)).toBe(true);
  });

  it("drops the one that finished long ago, so the bell does not become a history", () => {
    const old = job({
      state: "done",
      finished_at: new Date(now - RECENTLY_FINISHED_MS - 1000).toISOString(),
    });
    expect(isRecentlyFinished(old, now)).toBe(false);
    expect(isWorthNotifying(old, now)).toBe(false);
  });

  /**
   * Um nó de versão anterior não carimba a hora. Ali não dá para saber se
   * acabou agora ou ontem, e supor "agora" encheria o sino de transferências
   * velhas a cada carregamento da página.
   */
  it("invents no 'now' for a node that does not stamp the time", () => {
    expect(isWorthNotifying(job({ state: "done" }), now)).toBe(false);
    expect(
      isWorthNotifying(job({ state: "done", finished_at: null }), now),
    ).toBe(false);
    expect(
      isWorthNotifying(job({ state: "done", finished_at: "não é data" }), now),
    ).toBe(false);
  });

  it("what is still in progress stays in the bell, with or without a stamp", () => {
    expect(isWorthNotifying(job({ state: "running" }), now)).toBe(true);
    expect(isWorthNotifying(job({ state: "verifying" }), now)).toBe(true);
  });
});
