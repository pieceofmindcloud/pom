// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../../i18n/provider";
import type { AcpHandlers } from "../../lib/acp/connection";
import type { ChatAgent } from "../../lib/types";
import { AgentChat } from "./AgentChat";
import { AgentPicker } from "./AgentPicker";

const mocks = vi.hoisted(() => ({
  openAcp: vi.fn(),
  openPlugin: vi.fn(),
}));
vi.mock("../../lib/acp/connection", () => ({ openAcp: mocks.openAcp }));
vi.mock("../../lib/api", () => ({ openPlugin: mocks.openPlugin }));

const goose = (status: string): ChatAgent => ({
  plugin_code: "goose",
  id: "goose",
  name: "Goose",
  version: "1.57.0",
  status,
  proxy_path: "/api/ui/plugins/goose/proxy/acp",
  protocol_version: 1,
  cwd: "/data/workspace",
});

/** A scripted connection; `handlers` is what AgentChat passed to openAcp. */
function fakeConnection(options: { model?: boolean; list?: boolean } = {}) {
  let handlers!: AcpHandlers;
  let closeIt!: () => void;
  const prompt = vi.fn(async () => "end_turn");
  const conn = {
    canLoad: Boolean(options.list),
    canList: Boolean(options.list),
    newSession: vi.fn(async () => ({
      sessionId: "s1",
      model: options.model ? { configId: "model", current: "a", options: [{ value: "a", name: "A" }, { value: "b", name: "B" }] } : null,
    })),
    loadSession: vi.fn(async () => ({ model: null })),
    listSessions: vi.fn(async () => (options.list ? [{ sessionId: "s0", title: "Earlier work", updatedAt: null }] : [])),
    prompt,
    setModel: vi.fn(async () => {}),
    cancel: vi.fn(async () => {}),
    closed: new Promise<void>((resolve) => {
      closeIt = resolve;
    }),
  };
  mocks.openAcp.mockImplementation(async (_path: string, h: AcpHandlers) => {
    handlers = h;
    return conn;
  });
  return { conn, prompt, handlers: () => handlers, close: () => closeIt() };
}

function mount(agent: ChatAgent | null) {
  return render(
    <LocaleProvider initialLocale="en">
      <AgentChat agent={agent} onLeave={() => {}} />
    </LocaleProvider>,
  );
}

beforeEach(() => {
  mocks.openPlugin.mockResolvedValue({ code: "goose", running: true });
});
afterEach(() => {
  cleanup();
  mocks.openAcp.mockReset();
  mocks.openPlugin.mockReset();
});

describe("AgentPicker", () => {
  it("is hidden without agents", () => {
    const { container } = render(
      <LocaleProvider initialLocale="en">
        <AgentPicker agents={[]} value={null} onChange={() => {}} />
      </LocaleProvider>,
    );
    expect(container.innerHTML).toBe("");
  });

  it("offers the model and each agent", () => {
    const onChange = vi.fn();
    render(
      <LocaleProvider initialLocale="en">
        <AgentPicker agents={[goose("ready")]} value={null} onChange={onChange} />
      </LocaleProvider>,
    );
    fireEvent.change(screen.getByLabelText("Talk to"), { target: { value: "goose/goose" } });
    expect(onChange).toHaveBeenCalledWith("goose/goose");
  });
});

describe("AgentChat", () => {
  it("opens a stopped agent's plugin and shows Starting until it is ready", async () => {
    fakeConnection();
    const view = mount(goose("stopped"));
    await waitFor(() => expect(mocks.openPlugin).toHaveBeenCalledWith("goose"));
    expect(screen.getByText("Starting Goose...")).toBeTruthy();
    expect(mocks.openAcp).not.toHaveBeenCalled();
    view.rerender(
      <LocaleProvider initialLocale="en">
        <AgentChat agent={goose("ready")} onLeave={() => {}} />
      </LocaleProvider>,
    );
    await waitFor(() => expect(mocks.openAcp).toHaveBeenCalledWith("/api/ui/plugins/goose/proxy/acp", expect.anything()));
  });

  it("streams text and draws a tool card", async () => {
    const { handlers, conn } = fakeConnection();
    conn.prompt.mockImplementation(async () => {
      handlers().onUpdate({ sessionId: "s1", update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Listing." } } });
      handlers().onUpdate({ sessionId: "s1", update: { sessionUpdate: "tool_call", toolCallId: "t1", title: "ls -la", status: "completed", kind: "execute" } });
      return "end_turn";
    });
    mount(goose("ready"));
    await waitFor(() => expect(conn.newSession).toHaveBeenCalledWith("/data/workspace"));
    fireEvent.change(screen.getByLabelText("Message Goose"), { target: { value: "list files" } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    await waitFor(() => expect(screen.getByText("Listing.")).toBeTruthy());
    expect(screen.getByText("list files")).toBeTruthy();
    expect(screen.getByText("ls -la")).toBeTruthy();
    expect(conn.prompt).toHaveBeenCalledWith("s1", "list files");
  });

  it("Approve sends the chosen optionId", async () => {
    const { handlers, conn } = fakeConnection();
    let answer: Promise<string | null> = Promise.resolve(null);
    conn.prompt.mockImplementation(async () => {
      answer = handlers().onPermission("p1", {
        sessionId: "s1",
        toolCall: { toolCallId: "t1", title: "rm a.txt" },
        options: [
          { optionId: "yes", name: "Allow once", kind: "allow_once" },
          { optionId: "no", name: "Reject", kind: "reject_once" },
        ],
      });
      return "end_turn";
    });
    mount(goose("ready"));
    await waitFor(() => expect(conn.newSession).toHaveBeenCalled());
    fireEvent.change(screen.getByLabelText("Message Goose"), { target: { value: "clean" } });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    fireEvent.click(await screen.findByRole("button", { name: "Allow once" }));
    await expect(answer).resolves.toBe("yes");
  });

  it("socket close mid-turn shows retry", async () => {
    const { close, conn } = fakeConnection();
    mount(goose("ready"));
    await waitFor(() => expect(conn.newSession).toHaveBeenCalled());
    await act(async () => close());
    expect(await screen.findByText("The connection to Goose was lost.")).toBeTruthy();
    fakeConnection();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(mocks.openAcp).toHaveBeenCalledTimes(2));
  });

  it("drops a selected agent that disappears", () => {
    mount(null);
    expect(screen.getByText("This agent is no longer available on this machine.")).toBeTruthy();
  });

  it("session list comes from session/list and reopening calls session/load", async () => {
    const { conn } = fakeConnection({ list: true });
    mount(goose("ready"));
    await waitFor(() => expect(conn.listSessions).toHaveBeenCalledWith("/data/workspace"));
    fireEvent.change(await screen.findByLabelText("Earlier sessions"), { target: { value: "s0" } });
    await waitFor(() => expect(conn.loadSession).toHaveBeenCalledWith("s0", "/data/workspace"));
  });

  it("model picker shows only when the agent offers a model option", async () => {
    const plain = fakeConnection();
    const first = mount(goose("ready"));
    await waitFor(() => expect(plain.conn.newSession).toHaveBeenCalled());
    expect(screen.queryByLabelText("Agent model")).toBeNull();
    first.unmount();
    const rich = fakeConnection({ model: true });
    mount(goose("ready"));
    fireEvent.change(await screen.findByLabelText("Agent model"), { target: { value: "b" } });
    await waitFor(() => expect(rich.conn.setModel).toHaveBeenCalledWith("s1", "model", "b"));
  });
});
