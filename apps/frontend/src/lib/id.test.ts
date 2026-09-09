import { describe, expect, it, vi } from "vitest";
import { randomId } from "./id";

describe("randomId", () => {
  it("works when randomUUID is unavailable outside a secure context", () => {
    vi.stubGlobal("crypto", {
      getRandomValues: (bytes: Uint8Array) => {
        bytes.fill(0xab);
        return bytes;
      },
    });

    const value = randomId();

    expect(value).toMatch(/^[0-9a-f]{32}$/);
    expect(value[12]).toBe("4");
    expect("89ab").toContain(value[16]);
  });
});
