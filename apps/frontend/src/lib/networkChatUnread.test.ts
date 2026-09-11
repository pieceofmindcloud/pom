// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import {
  countUnreadNetworkChatMessages,
  type NetworkChatReadMarker,
} from "./networkChatUnread";
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
});
