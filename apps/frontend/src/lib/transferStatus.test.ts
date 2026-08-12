import { describe, expect, it } from "vitest";
import { canCancel, isActiveTransfer, isFinishedTransfer } from "./transferStatus";

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
