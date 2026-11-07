// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("../lib/chatImage", () => ({
  loadChatImage: vi.fn((hash: string) => Promise.resolve(hash.startsWith("a") ? "blob:a" : null)),
}));

import { ChatImageGrid } from "./ChatImageGrid";
import { LocaleProvider } from "../i18n/provider";

afterEach(cleanup);

describe("ChatImageGrid", () => {
  it("shows the images it can load and says which one is gone", async () => {
    render(<LocaleProvider><ChatImageGrid images={[
      { hash: "a".repeat(64), ext: "png", width: 8, height: 8 },
      { hash: "b".repeat(64), ext: "jpg", width: 8, height: 8 },
    ]} /></LocaleProvider>);
    expect((await screen.findByRole("img")).getAttribute("src")).toBe("blob:a");
    expect(await screen.findByText("This image is no longer on this node")).toBeTruthy();
  });
});
