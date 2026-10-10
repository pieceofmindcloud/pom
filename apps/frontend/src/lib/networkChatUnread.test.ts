// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import {
  countUnreadNetworkChatMessages,
  markLobbyRead,
  markNetworkChatRead,
  type NetworkChatReadMarker,
} from "./networkChatUnread";
import { storageKey } from "./brand";
import type { NetworkChatEvent } from "./types";

function message(
  messageId: string,
  senderNodeId: string,
  sentAt: string,
): NetworkChatEvent {
  return {
    messageId,
    roomId: "network",
    senderNodeId,
    recipientNodeId: null,
    kind: "text",
    text: messageId,
    fileName: null,
    mimeType: null,
    fileData: [],
    sentAt,
  };
}

describe("network chat unread state", () => {
  it("counts remote messages when no read marker exists yet", () => {
    const messages = [
      message("old", "peer", "2026-09-11T12:00:00.000Z"),
      message("mine", "local", "2026-09-11T12:01:00.000Z"),
    ];

    expect(countUnreadNetworkChatMessages(messages, "local", null)).toBe(1);
  });

  it("counts only incoming messages after the read marker", () => {
    const messages = [
      message("old", "peer", "2026-09-11T12:00:00.000Z"),
      message("mine", "local", "2026-09-11T12:01:00.000Z"),
      message("new-1", "peer", "2026-09-11T12:02:00.000Z"),
      message("new-2", "peer-2", "2026-09-11T12:03:00.000Z"),
    ];
    const marker: NetworkChatReadMarker = {
      messageId: "old",
      sentAt: "2026-09-11T12:00:00.000Z",
    };

    expect(countUnreadNetworkChatMessages(messages, "local", marker)).toBe(2);
  });

  it("does not count a stale marker's history as new", () => {
    const messages = [
      message("new-1", "peer", "2026-09-11T12:02:00.000Z"),
      message("new-2", "peer", "2026-09-11T12:03:00.000Z"),
    ];
    const marker: NetworkChatReadMarker = {
      messageId: "rotated-out",
      sentAt: "2026-09-11T12:01:00.000Z",
    };

    expect(countUnreadNetworkChatMessages(messages, "local", marker)).toBe(2);
  });

  it("keeps the lobby's read marker apart from the members' room", () => {
    markNetworkChatRead([message("n1", "peer", "2026-09-11T12:00:00.000Z")]);
    markLobbyRead([message("l1", "guest", "2026-09-11T12:05:00.000Z")]);
    expect(JSON.parse(localStorage.getItem(storageKey("network-chat.read"))!).messageId).toBe("n1");
    expect(JSON.parse(localStorage.getItem(storageKey("network-chat.lobby.read"))!).messageId).toBe("l1");
  });
});
