import { useEffect, useRef, useSyncExternalStore } from "react";
import { useNetworkChat, useNode } from "./queries";
import { storageKey } from "./brand";
import type { NetworkChatEvent } from "./types";

export interface NetworkChatReadMarker {
  messageId: string;
  sentAt: string;
}

/**
 * One read marker per room, each kept in this browser: the members' room and
 * the lobby, where machines that are not members ask to join.
 */
type Room = "network" | "lobby";
const STORAGE: Record<Room, string> = {
  network: storageKey("network-chat.read"),
  lobby: storageKey("network-chat.lobby.read"),
};
const listeners = new Set<() => void>();

function loadMarker(room: Room): NetworkChatReadMarker | null {
  try {
    const raw = localStorage.getItem(STORAGE[room]);
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

const markers: Record<Room, NetworkChatReadMarker | null> = {
  network: loadMarker("network"),
  lobby: loadMarker("lobby"),
};

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function notifyMarkerChanged() {
  for (const listener of listeners) listener();
}

function markRead(room: Room, messages: NetworkChatEvent[]) {
  const latest = messages[messages.length - 1];
  if (!latest || markers[room]?.messageId === latest.messageId) return;
  markers[room] = { messageId: latest.messageId, sentAt: latest.sentAt };
  try {
    localStorage.setItem(STORAGE[room], JSON.stringify(markers[room]));
  } catch {
    // The in-memory marker still makes this tab behave correctly.
  }
  notifyMarkerChanged();
}

export function markNetworkChatRead(messages: NetworkChatEvent[]) {
  markRead("network", messages);
}

/** The lobby was read: called while the lobby room is open on its screen. */
export function markLobbyRead(messages: NetworkChatEvent[]) {
  markRead("lobby", messages);
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
  const readMarker = useSyncExternalStore(subscribe, () => markers.network, () => null);
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

/**
 * New lobby messages for an administrator: what machines that are not members
 * wrote since the lobby was last open here. The lobby is read only while the
 * network has it on and this machine holds an admin grant; otherwise nothing
 * is asked and the count is zero. The first answer marks what is already
 * there as read, like the members' room.
 */
export function useLobbyUnread(enabled: boolean) {
  const query = useNetworkChat("lobby", enabled);
  const node = useNode();
  const readMarker = useSyncExternalStore(subscribe, () => markers.lobby, () => null);
  const initialized = useRef(false);
  const messages = enabled ? query.data?.messages ?? [] : [];

  useEffect(() => {
    if (!enabled || !query.isSuccess || initialized.current) return;
    initialized.current = true;
    if (messages.length > 0 && readMarker === null) markLobbyRead(messages);
  }, [enabled, messages, query.isSuccess, readMarker]);

  return enabled ? countUnreadNetworkChatMessages(messages, node.data?.node_id ?? "", readMarker) : 0;
}
