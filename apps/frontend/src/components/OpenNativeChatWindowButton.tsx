import { ExternalLink, Power } from "lucide-react";
import { useI18n } from "../i18n/provider";

type WebKitWindow = Window & {
  webkit?: {
    messageHandlers?: {
      pomOpenWindow?: { postMessage: (message: string) => void };
      pomQuit?: { postMessage: (message: string) => void };
    };
  };
};

function nativeWindowHandler() {
  if (typeof window === "undefined") return undefined;
  return (window as WebKitWindow).webkit?.messageHandlers?.pomOpenWindow;
}

const NATIVE_BUTTON =
  "flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-line bg-raised text-muted transition-colors hover:border-line-strong hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris";

/**
 * Quits the macOS menu-bar app. It runs as an accessory, with no Dock icon or
 * menu bar of its own, so without this the app opened from the menu bar had
 * no way to be closed. Only the macOS WebKit bridge exposes `pomQuit`; on
 * Windows and Linux closing the window already quits, and nothing renders.
 */
export function QuitNativeAppButton() {
  const { t } = useI18n();
  if (typeof window === "undefined") return null;
  const handler = (window as WebKitWindow).webkit?.messageHandlers?.pomQuit;
  if (!handler) return null;
  return (
    <button
      type="button"
      onClick={() => handler.postMessage("quit")}
      aria-label={t("chat.quitApp")}
      title={t("chat.quitApp")}
      className={`${NATIVE_BUTTON} hover:text-rose`}
    >
      <Power className="h-3.5 w-3.5" />
    </button>
  );
}

export function OpenNativeChatWindowButton() {
  const { t } = useI18n();
  const handler = nativeWindowHandler();
  if (!handler) return null;

  return (
    <button
      type="button"
      onClick={() => handler.postMessage("open")}
      aria-label={t("chat.openLargerWindow")}
      title={t("chat.openLargerWindow")}
      className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-line bg-raised text-muted transition-colors hover:border-line-strong hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-iris"
    >
      <ExternalLink className="h-3.5 w-3.5" />
    </button>
  );
}
