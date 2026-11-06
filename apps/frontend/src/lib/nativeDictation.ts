/**
 * Dictation through the macOS app, in the shape of the browser's Web Speech.
 *
 * The app's WebKit is not a dependable place for Web Speech, so the native side
 * (`crates/node/src/macos_status.m`) listens with the system's own recognizer and
 * sends its text back. The app offers the `pomDictation` bridge only when its
 * bundle declares the microphone and speech permissions.
 */
type DictationBridge = { postMessage: (message: unknown) => void };

interface NativeEvent {
  type: "result" | "final" | "error" | "end";
  text?: string;
  code?: string;
  message?: string;
}

type NativeWindow = Window & {
  webkit?: { messageHandlers?: { pomDictation?: DictationBridge } };
  __pomDictationEvent?: (event: NativeEvent) => void;
};

function bridge(): DictationBridge | undefined {
  return (window as NativeWindow).webkit?.messageHandlers?.pomDictation;
}

export function nativeDictationAvailable(): boolean {
  return bridge() != null;
}

let active: NativeDictation | null = null;

export class NativeDictation {
  lang = "";
  continuous = true;
  interimResults = true;
  onresult:
    | ((event: {
        resultIndex: number;
        results: ArrayLike<{ isFinal: boolean; [index: number]: { transcript: string } }>;
      }) => void)
    | null = null;
  onerror: ((event: { error?: string; message?: string }) => void) | null = null;
  onend: (() => void) | null = null;

  start() {
    active = this;
    (window as NativeWindow).__pomDictationEvent = (event) => active?.deliver(event);
    bridge()?.postMessage({ action: "start", locale: this.lang });
  }

  /** Ends here at once: a word arriving later must not refill a draft already sent. */
  stop() {
    if (active === this) active = null;
    bridge()?.postMessage({ action: "stop" });
  }

  private deliver(event: NativeEvent) {
    if (event.type === "error") {
      this.onerror?.({ error: event.code, message: event.message });
    } else if (event.type === "end") {
      if (active === this) active = null;
      this.onend?.();
    } else {
      const result = { 0: { transcript: event.text ?? "" }, isFinal: event.type === "final" };
      this.onresult?.({ resultIndex: 0, results: [result] });
    }
  }
}
