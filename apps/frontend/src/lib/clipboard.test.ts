// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { copyText } from "./clipboard";

function setSecure(value: boolean) {
  Object.defineProperty(window, "isSecureContext", { configurable: true, value });
}

function setClipboard(value: unknown) {
  Object.defineProperty(navigator, "clipboard", { configurable: true, value });
}

afterEach(() => {
  setSecure(false);
  setClipboard(undefined);
  Reflect.deleteProperty(document, "execCommand");
  vi.restoreAllMocks();
});

describe("copyText", () => {
  it("uses the browser clipboard API in a secure context", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    setSecure(true);
    setClipboard({ writeText });

    await expect(copyText("abc")).resolves.toBe(true);
    expect(writeText).toHaveBeenCalledWith("abc");
  });

  it("falls back to execCommand over plain HTTP, where navigator.clipboard is missing", async () => {
    const execCommand = vi.fn().mockReturnValue(true);
    Object.defineProperty(document, "execCommand", { configurable: true, value: execCommand });
    setSecure(false);

    await expect(copyText("chave-123")).resolves.toBe(true);
    expect(execCommand).toHaveBeenCalledWith("copy");
    // The fallback clipboard is removed from the DOM after the copy.
    expect(document.querySelector("textarea")).toBeNull();
  });

  it("falls back when the clipboard API refuses", async () => {
    const execCommand = vi.fn().mockReturnValue(true);
    Object.defineProperty(document, "execCommand", { configurable: true, value: execCommand });
    setSecure(true);
    setClipboard({ writeText: vi.fn().mockRejectedValue(new Error("denied")) });

    await expect(copyText("x")).resolves.toBe(true);
    expect(execCommand).toHaveBeenCalledWith("copy");
  });

  it("resolves false when neither path copies", async () => {
    const execCommand = vi.fn().mockReturnValue(false);
    Object.defineProperty(document, "execCommand", { configurable: true, value: execCommand });
    setSecure(false);

    await expect(copyText("x")).resolves.toBe(false);
  });
});
