import { describe, expect, it } from "vitest";
import { messageKeys } from "../i18n/locales";
import { WAITING_ROTATE_MS, WAITING_SET_SIZE, waitingMessageKey, waitingSet, type WaitingSet } from "./chatWaiting";

describe("chat waiting line", () => {
  it("picks the set from the state of the deployment and the length of the wait", () => {
    expect(waitingSet(true, 0)).toBe("wake");
    expect(waitingSet(true, 60_000)).toBe("wake");
    expect(waitingSet(false, 0)).toBe("pieces");
    expect(waitingSet(false, 6_000)).toBe("assemble");
    expect(waitingSet(false, 20_000)).toBe("long");
  });

  it("opens a wake with the line that says the engines are starting", () => {
    expect(waitingMessageKey({ asleep: true, elapsedMs: 0, seed: 4 })).toBe("chat.wait.wake.1");
  });

  it("rotates in order and never repeats a line back to back", () => {
    const keys = Array.from({ length: 12 }, (_, step) =>
      waitingMessageKey({ asleep: true, elapsedMs: step * WAITING_ROTATE_MS, seed: 0 }));
    for (let index = 1; index < keys.length; index += 1) expect(keys[index]).not.toBe(keys[index - 1]);
    expect(new Set(keys).size).toBe(WAITING_SET_SIZE.wake);
  });

  it("starts each ordinary wait on a different line", () => {
    expect(waitingMessageKey({ asleep: false, elapsedMs: 0, seed: 0 }))
      .not.toBe(waitingMessageKey({ asleep: false, elapsedMs: 0, seed: 1 }));
  });

  it("has every message of every set in both languages", () => {
    for (const locale of ["en", "pt-BR"] as const) {
      const keys = new Set(messageKeys(locale));
      for (const set of Object.keys(WAITING_SET_SIZE) as WaitingSet[]) {
        for (let n = 1; n <= WAITING_SET_SIZE[set]; n += 1) expect(keys.has(`chat.wait.${set}.${n}`)).toBe(true);
        expect(keys.has(`chat.wait.${set}.${WAITING_SET_SIZE[set] + 1}`)).toBe(false);
      }
    }
  });
});
