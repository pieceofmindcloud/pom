// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  addPendingPluginConfirm,
  addPluginNotice,
  answerPendingPluginConfirm,
  dismissPluginNotice,
  markPluginNoticesRead,
  pendingPluginConfirms,
  pluginNotices,
  resetPluginInboxForTest,
} from "./pluginInbox";

afterEach(() => resetPluginInboxForTest());

const notice = (id: string) => ({
  id,
  code: "base",
  title: `Aviso ${id}`,
  level: "info" as const,
  createdAt: "2026-09-28T12:00:00Z",
  source: "network" as const,
});

describe("plugin inbox", () => {
  it("keeps a question until it is answered once and resolves the plugin's promise", () => {
    const resolve = vi.fn();
    addPendingPluginConfirm({ id: "c1", code: "base", request: { title: "Continuar?" }, createdAt: "now" }, resolve);
    expect(pendingPluginConfirms().map((entry) => entry.id)).toEqual(["c1"]);

    answerPendingPluginConfirm("c1", "accepted");
    answerPendingPluginConfirm("c1", "cancelled");

    expect(pendingPluginConfirms()).toEqual([]);
    expect(resolve).toHaveBeenCalledTimes(1);
    expect(resolve).toHaveBeenCalledWith("accepted");
  });

  it("adds notices once, unread, newest first", () => {
    expect(addPluginNotice(notice("a"))).toBe(true);
    expect(addPluginNotice(notice("b"))).toBe(true);
    expect(addPluginNotice(notice("a"))).toBe(false);
    expect(pluginNotices().map((item) => `${item.id}:${item.read}`)).toEqual(["b:false", "a:false"]);
  });

  it("remembers read notices so a reload does not bring them back as new", () => {
    addPluginNotice(notice("a"));
    markPluginNoticesRead();
    dismissPluginNotice("a");
    expect(pluginNotices()).toEqual([]);
    // Same network notice seen again after a reload of the node snapshot.
    expect(addPluginNotice(notice("a"))).toBe(false);
    expect(pluginNotices()[0].read).toBe(true);
  });
});
