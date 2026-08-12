import { describe, expect, it } from "vitest";
import { isActiveTransfer, isFinishedTransfer } from "./transferStatus";

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
});
