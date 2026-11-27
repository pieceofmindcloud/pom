// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { api } from "../lib/api";
import { ChatTemplateDialog } from "./ChatTemplateDialog";

vi.mock("../lib/api", () => ({ api: { get: vi.fn(), post: vi.fn() } }));

const GGUF_TEMPLATE = "{% for m in messages %}<|message_user|>{{ m.content }}{% endfor %}";

function mount(props: Partial<Parameters<typeof ChatTemplateDialog>[0]> = {}) {
  const onApply = vi.fn();
  const onClose = vi.fn();
  render(
    <LocaleProvider initialLocale="en">
      <ChatTemplateDialog
        open
        value={null}
        source={{ path: "/models/inkling.gguf", nodeId: "ksia" }}
        onApply={onApply}
        onClose={onClose}
        {...props}
      />
    </LocaleProvider>,
  );
  return { onApply, onClose };
}

describe("ChatTemplateDialog", () => {
  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("loads the GGUF's template from the machine that holds the file and applies it as the deployment's", async () => {
    vi.mocked(api.post).mockResolvedValue({ chat_template: GGUF_TEMPLATE });
    const { onApply } = mount();

    fireEvent.click(screen.getByRole("button", { name: "Load the GGUF's template" }));

    await waitFor(() =>
      expect((screen.getByRole("textbox", { name: "Chat template (Jinja)" }) as HTMLTextAreaElement).value).toBe(GGUF_TEMPLATE),
    );
    expect(api.post).toHaveBeenCalledWith("/api/ui/model-chat-template", { path: "/models/inkling.gguf", node_id: "ksia" });

    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    expect(onApply).toHaveBeenCalledWith(GGUF_TEMPLATE);
  });

  it("an emptied template goes back to the GGUF's", () => {
    const { onApply } = mount({ value: GGUF_TEMPLATE });

    fireEvent.click(screen.getByRole("button", { name: "Use the GGUF's template" }));
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));

    expect(onApply).toHaveBeenCalledWith(null);
  });

  it("says so when the GGUF has no template", async () => {
    vi.mocked(api.post).mockResolvedValue({ chat_template: null });
    mount();

    fireEvent.click(screen.getByRole("button", { name: "Load the GGUF's template" }));

    expect((await screen.findByRole("alert")).textContent).toBe("This GGUF has no chat template");
  });

  it("only shows the template when read-only", () => {
    mount({ value: GGUF_TEMPLATE, readOnly: true });

    expect((screen.getByRole("textbox", { name: "Chat template (Jinja)" }) as HTMLTextAreaElement).readOnly).toBe(true);
    expect(screen.queryByRole("button", { name: "Apply" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Load the GGUF's template" })).toBeNull();
  });
});
