// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";

import { NativeDictation, nativeDictationAvailable } from "./nativeDictation";

function bridge() {
  const postMessage = vi.fn();
  Object.defineProperty(window, "webkit", {
    configurable: true,
    value: { messageHandlers: { pomDictation: { postMessage } } },
  });
  return postMessage;
}

type Delivered = { type: string; text?: string; code?: string; message?: string };
const deliver = (event: Delivered) =>
  (window as unknown as { __pomDictationEvent: (event: Delivered) => void }).__pomDictationEvent(event);

afterEach(() => {
  Reflect.deleteProperty(window, "webkit");
  Reflect.deleteProperty(window, "__pomDictationEvent");
});

describe("NativeDictation", () => {
  it("is only available inside the app that offers the bridge", () => {
    expect(nativeDictationAvailable()).toBe(false);
    bridge();
    expect(nativeDictationAvailable()).toBe(true);
  });

  it("asks the app to start in the chosen language and delivers its text as results", () => {
    const postMessage = bridge();
    const dictation = new NativeDictation();
    const onresult = vi.fn();
    dictation.lang = "pt-BR";
    dictation.onresult = onresult;
    dictation.start();
    expect(postMessage).toHaveBeenCalledWith({ action: "start", locale: "pt-BR" });

    deliver({ type: "result", text: "Olá" });
    deliver({ type: "final", text: "Olá, rede" });
    const [partial] = onresult.mock.calls[0];
    expect(partial.results[0][0].transcript).toBe("Olá");
    expect(partial.results[0].isFinal).toBe(false);
    const [final] = onresult.mock.calls[1];
    expect(final.results[0][0].transcript).toBe("Olá, rede");
    expect(final.results[0].isFinal).toBe(true);
  });

  it("reports the app's failure and its end", () => {
    bridge();
    const dictation = new NativeDictation();
    const onerror = vi.fn();
    const onend = vi.fn();
    dictation.onerror = onerror;
    dictation.onend = onend;
    dictation.start();
    deliver({ type: "error", code: "denied", message: "no" });
    deliver({ type: "end" });
    expect(onerror).toHaveBeenCalledWith({ error: "denied", message: "no" });
    expect(onend).toHaveBeenCalledTimes(1);
  });

  it("delivers nothing after stop, so a late word cannot refill a sent draft", () => {
    const postMessage = bridge();
    const dictation = new NativeDictation();
    const onresult = vi.fn();
    const onend = vi.fn();
    dictation.onresult = onresult;
    dictation.onend = onend;
    dictation.start();
    dictation.stop();
    expect(postMessage).toHaveBeenLastCalledWith({ action: "stop" });
    deliver({ type: "final", text: "late" });
    deliver({ type: "end" });
    expect(onresult).not.toHaveBeenCalled();
    expect(onend).not.toHaveBeenCalled();
  });

  it("gives the bridge to the newest dictation", () => {
    bridge();
    const first = new NativeDictation();
    const second = new NativeDictation();
    const firstResult = vi.fn();
    const secondResult = vi.fn();
    first.onresult = firstResult;
    second.onresult = secondResult;
    first.start();
    second.start();
    deliver({ type: "result", text: "hi" });
    expect(firstResult).not.toHaveBeenCalled();
    expect(secondResult).toHaveBeenCalledTimes(1);
  });
});
