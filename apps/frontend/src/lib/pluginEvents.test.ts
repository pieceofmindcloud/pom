// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  forwardNodeEvent,
  installPluginCommandExecutor,
  listenForPluginCommands,
  PLUGIN_EVENT_DOM,
  PLUGIN_EVENTS_PROTOCOL,
  PLUGIN_REQUEST_DOM,
  publishPluginEvent,
  resetPluginEventsForTest,
  runPluginCommand,
  scopedPluginApi,
  subscribePluginEvents,
  type PluginEvent,
} from "./pluginEvents";

afterEach(() => {
  resetPluginEventsForTest();
  document.body.innerHTML = "";
});

function executor(overrides: Partial<Parameters<typeof installPluginCommandExecutor>[0] & object> = {}) {
  const value = {
    isPlugin: (code: string) => code === "base",
    notify: vi.fn(async () => {}),
    confirm: vi.fn(async () => "accepted" as const),
    ...overrides,
  };
  installPluginCommandExecutor(value);
  return value;
}

describe("pom-plugin-events/v1", () => {
  it("delivers one envelope through the SDK, the DOM event and same-origin plugin iframes", () => {
    const frame = document.createElement("iframe");
    frame.dataset.pomPlugin = "base";
    document.body.appendChild(frame);
    const posted = vi.spyOn(frame.contentWindow as Window, "postMessage");
    const sdk: PluginEvent[] = [];
    const dom: PluginEvent[] = [];
    subscribePluginEvents("theme.changed", (event) => sdk.push(event));
    window.addEventListener(PLUGIN_EVENT_DOM, (event) => dom.push((event as CustomEvent).detail));

    const event = publishPluginEvent("theme.changed", { theme: "light" });

    expect(event).toMatchObject({ protocol: PLUGIN_EVENTS_PROTOCOL, type: "theme.changed", target: "*", source: "pom", payload: { theme: "light" } });
    expect(sdk).toEqual([event]);
    expect(dom).toEqual([event]);
    expect(posted).toHaveBeenCalledWith(event, window.location.origin);
  });

  it("replays the last global event and filters events addressed to another plugin", () => {
    publishPluginEvent("locale.changed", { locale: "pt-BR" });
    const seen: string[] = [];
    const api = scopedPluginApi("base");
    api.events.subscribe("*", (event) => seen.push(`${event.type}:${event.target}`), { replay: true });
    publishPluginEvent("notification.response", { request_id: "x" }, "other");
    publishPluginEvent("notification.response", { request_id: "y" }, "base");
    expect(seen).toEqual(["locale.changed:*", "notification.response:base"]);
  });

  it("answers confirmations and notifications with notification.response for the calling plugin", async () => {
    const host = executor({ confirm: vi.fn(async () => "cancelled" as const) });
    const responses: PluginEvent[] = [];
    subscribePluginEvents("notification.response", (event) => responses.push(event), { plugin: "base" });
    const api = scopedPluginApi("base");

    await expect(api.notifications.confirm({ title: "Continuar?" })).resolves.toMatchObject({ kind: "confirm", action: "cancelled" });
    await expect(api.notifications.notify({ title: "Pronto", scope: "network", level: "success" })).resolves.toMatchObject({ kind: "notify", action: "delivered" });
    expect(host.notify).toHaveBeenCalledWith("base", { title: "Pronto", body: undefined, level: "success", scope: "network" });
    expect(responses.map((event) => event.payload.action)).toEqual(["cancelled", "delivered"]);
  });

  it("refuses unknown plugins, bad protocols and invalid requests without calling the host", async () => {
    const host = executor();
    await expect(runPluginCommand({ protocol: "other", type: "notification.notify", plugin_code: "base", payload: { title: "x" } }))
      .resolves.toMatchObject({ action: "failed", error: "protocol_unsupported" });
    await expect(runPluginCommand({ protocol: PLUGIN_EVENTS_PROTOCOL, type: "notification.notify", plugin_code: "ghost", payload: { title: "x" } }))
      .resolves.toMatchObject({ action: "failed", error: "plugin_unknown" });
    await expect(runPluginCommand({ protocol: PLUGIN_EVENTS_PROTOCOL, type: "notification.notify", plugin_code: "base", payload: { title: "" } }))
      .resolves.toMatchObject({ action: "failed", error: "request_invalid" });
    await expect(runPluginCommand({ protocol: PLUGIN_EVENTS_PROTOCOL, type: "notification.notify", plugin_code: "base", payload: { title: "x", level: "loud" } }))
      .resolves.toMatchObject({ action: "failed", error: "request_invalid" });
    expect(host.notify).not.toHaveBeenCalled();
  });

  it("accepts commands from code without the SDK through the DOM request event", async () => {
    const host = executor();
    const stop = listenForPluginCommands();
    const answered = new Promise<PluginEvent>((resolve) => {
      window.addEventListener(PLUGIN_EVENT_DOM, (event) => {
        const detail = (event as CustomEvent).detail as PluginEvent;
        if (detail.type === "notification.response") resolve(detail);
      });
    });
    window.dispatchEvent(new CustomEvent(PLUGIN_REQUEST_DOM, {
      detail: { protocol: PLUGIN_EVENTS_PROTOCOL, id: "req-1", type: "notification.confirm", plugin_code: "base", payload: { title: "Aceitar?" } },
    }));
    const response = await answered;
    expect(response.target).toBe("base");
    expect(response.payload).toMatchObject({ request_id: "req-1", kind: "confirm", action: "accepted" });
    expect(host.confirm).toHaveBeenCalled();
    stop();
  });

  it("forwards node envelopes unchanged and ignores unknown or malformed ones", () => {
    const seen: PluginEvent[] = [];
    subscribePluginEvents("*", (event) => seen.push(event));
    const envelope = {
      protocol: PLUGIN_EVENTS_PROTOCOL,
      id: "node-1",
      type: "model.serving",
      target: "*",
      source: "pom",
      at: "2026-09-28T12:00:00Z",
      payload: { model: "qwen", deploy_ids: ["d1"], node_ids: ["n1"] },
    };
    expect(forwardNodeEvent(envelope)?.id).toBe("node-1");
    expect(forwardNodeEvent({ ...envelope, id: "x", type: "theme.changed" })).toBeNull();
    expect(forwardNodeEvent({ ...envelope, id: "y", protocol: "other" })).toBeNull();
    expect(forwardNodeEvent(null)).toBeNull();
    expect(seen.map((event) => `${event.type}:${event.id}`)).toEqual(["model.serving:node-1"]);
  });

  it("defaults confirmations to the notification bell and accepts an explicit dialog", async () => {
    const host = executor();
    const api = scopedPluginApi("base");
    await api.notifications.confirm({ title: "Bell?" });
    await api.notifications.confirm({ title: "Dialog?", display: "dialog" });
    expect(host.confirm.mock.calls.map(([, request]) => (request as { display?: string }).display)).toEqual(["notification", "dialog"]);
    await expect(api.notifications.confirm({ title: "Bad", display: "popup" as never }))
      .resolves.toMatchObject({ action: "failed", error: "request_invalid" });
  });
});
