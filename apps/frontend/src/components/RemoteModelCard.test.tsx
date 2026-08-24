// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import type { RemoteModelSummary } from "../lib/types";
import { RemoteModelCard } from "./RemoteModelCard";

const model: RemoteModelSummary = {
  repo_id: "Qwen/Qwen3-GGUF",
  author: "Qwen",
  description: "A compact GGUF model",
  downloads: 1234,
  likes: 56,
  private: false,
  gated: false,
  tags: ["gguf", "text-generation"],
  last_modified: "2026-08-24T10:00:00Z",
  has_gguf: true,
  installed: false,
  downloading: false,
};

afterEach(cleanup);

function renderCard(onOpen = vi.fn()) {
  return render(
    <LocaleProvider initialLocale="en">
      <RemoteModelCard model={model} onOpen={onOpen} />
    </LocaleProvider>,
  );
}

describe("RemoteModelCard", () => {
  it("shows comparable metadata and GGUF priority", () => {
    renderCard();
    expect(screen.getByText("Qwen/Qwen3-GGUF")).toBeTruthy();
    expect(screen.getByText("Qwen")).toBeTruthy();
    expect(screen.getAllByText("GGUF").length).toBeGreaterThan(0);
    expect(screen.getByText("1,234")).toBeTruthy();
  });

  it("opens details as an explicit action", () => {
    const onOpen = vi.fn();
    renderCard(onOpen);
    fireEvent.click(screen.getAllByRole("button", { name: /open details/i })[0]);
    expect(onOpen).toHaveBeenCalledWith(model);
  });
});
