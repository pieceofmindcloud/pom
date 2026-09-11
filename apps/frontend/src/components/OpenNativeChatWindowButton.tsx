import { ExternalLink } from "lucide-react";
import { useI18n } from "../i18n/provider";

type WebKitWindow = Window & {
  webkit?: {
    messageHandlers?: {
      pomOpenWindow?: { postMessage: (message: string) => void };
    };
  };
};

function nativeWindowHandler() {
  if (typeof window === "undefined") return undefined;
  return (window as WebKitWindow).webkit?.messageHandlers?.pomOpenWindow;
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
