import { useEffect, useRef, useSyncExternalStore } from "react";
import { useNetworkChat, useNode } from "./queries";
import { storageKey } from "./brand";
import type { NetworkChatEvent } from "./types";

export interface NetworkChatReadMarker {
  messageId: string;
  sentAt: string;
}

const STORAGE = storageKey("network-chat.read");
const listeners = new Set<() => void>();

function loadMarker(): NetworkChatReadMarker | null {
  try {
    const raw = localStorage.getItem(STORAGE);
    if (!raw) return null;
    const value: unknown = JSON.parse(raw);
    if (
      typeof value !== "object" ||
      value === null ||
      typeof (value as NetworkChatReadMarker).messageId !== "string" ||
      typeof (value as NetworkChatReadMarker).sentAt !== "string"
    ) {
      return null;
    }
    return value as NetworkChatReadMarker;
  } catch {
    return null;
  }
}

let marker = loadMarker();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function getMarker(): NetworkChatReadMarker | null {
  return marker;
}

function notifyMarkerChanged() {
  for (const listener of listeners) listener();
}

export function markNetworkChatRead(messages: NetworkChatEvent[]) {
  const latest = messages[messages.length - 1];
  if (!latest || marker?.messageId === latest.messageId) return;
  marker = { messageId: latest.messageId, sentAt: latest.sentAt };
  try {
    localStorage.setItem(STORAGE, JSON.stringify(marker));
  } catch {
    // The in-memory marker still makes this tab behave correctly.
  }
  notifyMarkerChanged();
}

export function countUnreadNetworkChatMessages(
  messages: NetworkChatEvent[],
  localNodeId: string,
  readMarker: NetworkChatReadMarker | null,
): number {
  const markerIndex = readMarker
    ? messages.findIndex((message) => message.messageId === readMarker.messageId)
    : -1;
  const candidates = readMarker && markerIndex >= 0
    ? messages.slice(markerIndex + 1)
    : readMarker
      ? messages.filter((message) => message.sentAt > readMarker.sentAt)
      : messages;
  return candidates.filter(
    (message) => message.senderNodeId !== localNodeId && message.kind !== "typing" && message.kind !== "typing_stopped",
  ).length;
}

export function useNetworkChatUnread(active: boolean) {
  const query = useNetworkChat();
  const node = useNode();
  const readMarker = useSyncExternalStore(subscribe, getMarker, () => null);
  const initialized = useRef(false);
  const messages = query.data?.messages ?? [];
  const latestMessageId = messages[messages.length - 1]?.messageId;

  useEffect(() => {
    if (!query.isSuccess || initialized.current) return;
    initialized.current = true;
    if (messages.length > 0 && (active || readMarker === null)) markNetworkChatRead(messages);
  }, [active, latestMessageId, messages, query.isSuccess, readMarker]);

  return {
    ...query,
    snapshot: query.data ?? null,
    localNodeId: node.data?.node_id ?? "",
    unreadCount: active
      ? 0
      : countUnreadNetworkChatMessages(messages, node.data?.node_id ?? "", readMarker),
  };
}
